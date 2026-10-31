//! Memory accounting, memory-warning fan-out, and the simulated jetsam watchdog.
//!
//! iOS kills apps whose *physical footprint* (not RSS) crosses the per-device
//! limit, so footprint is what is measured and reported here.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;

/// Why a memory warning was raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningSource {
    /// UIKit's `applicationDidReceiveMemoryWarning`.
    System,
    /// The strict-mode watchdog crossed 80% of the configured cap.
    Watchdog,
    /// Raised deliberately by a test or the self-test.
    Test,
}

type WarningCallback = Arc<dyn Fn(WarningSource) + Send + Sync>;

static WARNING_SUBSCRIBERS: Mutex<Vec<WarningCallback>> = Mutex::new(Vec::new());

/// Registers a callback that runs on every memory warning, on the thread that
/// raised it.
pub fn on_warning(callback: impl Fn(WarningSource) + Send + Sync + 'static) {
    WARNING_SUBSCRIBERS.lock().push(Arc::new(callback));
}

/// Delivers a memory warning to every subscriber.
pub fn raise_warning(source: WarningSource) {
    log::warn!(
        "memory warning ({source:?}): footprint {}",
        format_bytes(footprint_bytes().unwrap_or(0))
    );
    // Callbacks may subscribe again; never hold the lock while calling them.
    let subscribers = WARNING_SUBSCRIBERS.lock().clone();
    for subscriber in subscribers {
        subscriber(source);
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let mebibytes = bytes as f64 / MIB;
    if mebibytes >= 1024.0 {
        format!("{:.2} GB", mebibytes / 1024.0)
    } else {
        format!("{mebibytes:.0} MB")
    }
}

/// The app's physical footprint, the number jetsam compares against its limit.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn footprint_bytes() -> Option<u64> {
    // Layout of `task_vm_info` up to `phys_footprint` (rev1). The header packs
    // this struct to 4 bytes.
    #[repr(C, packed(4))]
    #[derive(Default)]
    struct TaskVmInfoRev1 {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }
    const TASK_VM_INFO: libc::c_int = 22;

    unsafe extern "C" {
        fn task_info(
            target_task: libc::mach_port_t,
            flavor: libc::c_int,
            task_info_out: *mut libc::c_int,
            task_info_count: *mut libc::c_uint,
        ) -> libc::c_int;
    }

    let mut info = TaskVmInfoRev1::default();
    let mut count = (std::mem::size_of::<TaskVmInfoRev1>() / std::mem::size_of::<libc::c_int>())
        as libc::c_uint;
    #[allow(deprecated)]
    let task = unsafe { libc::mach_task_self() };
    let result = unsafe {
        task_info(
            task,
            TASK_VM_INFO,
            (&raw mut info).cast::<libc::c_int>(),
            &mut count,
        )
    };
    (result == 0).then_some(info.phys_footprint)
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
pub fn footprint_bytes() -> Option<u64> {
    None
}

/// Memory the app can still allocate before jetsam (`os_proc_available_memory`).
/// Only iOS provides this; it is looked up dynamically so macOS tests link.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn available_bytes() -> Option<u64> {
    type OsProcAvailableMemory = unsafe extern "C" fn() -> libc::size_t;
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"os_proc_available_memory".as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    let function: OsProcAvailableMemory = unsafe { std::mem::transmute(symbol) };
    let available = unsafe { function() } as u64;
    // The Simulator returns 0 because it has no jetsam limit.
    (available > 0).then_some(available)
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
pub fn available_bytes() -> Option<u64> {
    None
}

/// What the watchdog decided for one footprint sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    None,
    Warn,
    Jetsam,
}

/// Threshold logic for the simulated memory cap, kept free of I/O so it can be
/// tested. A warning fires once per excursion above 80% and re-arms after the
/// footprint falls below 70%, matching how iOS repeats warnings under sustained
/// pressure without flooding.
#[derive(Debug)]
pub struct WatchdogState {
    cap_bytes: u64,
    warned: bool,
}

impl WatchdogState {
    pub const WARN_FRACTION: f64 = 0.80;
    pub const REARM_FRACTION: f64 = 0.70;

    pub fn new(cap_bytes: u64) -> Self {
        Self {
            cap_bytes,
            warned: false,
        }
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    pub fn sample(&mut self, footprint: u64) -> WatchdogAction {
        let fraction = footprint as f64 / self.cap_bytes as f64;
        if fraction >= 1.0 {
            WatchdogAction::Jetsam
        } else if fraction >= Self::WARN_FRACTION {
            if self.warned {
                WatchdogAction::None
            } else {
                self.warned = true;
                WatchdogAction::Warn
            }
        } else {
            if fraction < Self::REARM_FRACTION {
                self.warned = false;
            }
            WatchdogAction::None
        }
    }
}

type JetsamReporter = Box<dyn Fn() -> String + Send + Sync>;
static JETSAM_REPORTER: Mutex<Option<JetsamReporter>> = Mutex::new(None);

/// Supplies extra state (for example, running language servers) to record just
/// before a simulated jetsam kill.
pub fn set_jetsam_reporter(reporter: impl Fn() -> String + Send + Sync + 'static) {
    *JETSAM_REPORTER.lock() = Some(Box::new(reporter));
}

/// Writes the "last state" breadcrumb used to tell the user, on the next launch,
/// that the app was probably killed for memory.
pub fn record_last_state(reason: &str) {
    let footprint = footprint_bytes().unwrap_or(0);
    let extra = JETSAM_REPORTER
        .lock()
        .as_ref()
        .map(|reporter| reporter())
        .unwrap_or_default();
    let contents = format!(
        "reason: {reason}\nfootprint: {}\navailable: {}\n{extra}\n",
        format_bytes(footprint),
        available_bytes().map_or_else(|| "unknown".to_string(), format_bytes),
    );
    let path = crate::paths::crashes().join("last-state.txt");
    if let Err(error) = std::fs::create_dir_all(crate::paths::crashes())
        .and_then(|()| std::fs::write(&path, contents))
    {
        log::error!("failed to write {}: {error}", path.display());
    }
}

static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);

/// Starts the strict-mode watchdog thread that enforces `cap_bytes`.
pub fn start_watchdog(cap_bytes: u64, interval: Duration) {
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    log::info!("memory watchdog: simulated cap {}", format_bytes(cap_bytes));
    let spawn_result = std::thread::Builder::new()
        .name("memory-watchdog".into())
        .spawn(move || {
            let mut state = WatchdogState::new(cap_bytes);
            loop {
                if let Some(footprint) = footprint_bytes() {
                    match state.sample(footprint) {
                        WatchdogAction::None => {}
                        WatchdogAction::Warn => raise_warning(WarningSource::Watchdog),
                        WatchdogAction::Jetsam => simulated_jetsam(footprint, cap_bytes),
                    }
                }
                std::thread::sleep(interval);
            }
        });
    if let Err(error) = spawn_result {
        log::error!("failed to start memory watchdog: {error}");
    }
}

fn simulated_jetsam(footprint: u64, cap_bytes: u64) -> ! {
    let message = format!(
        "simulated jetsam: footprint {} reached the {} cap",
        format_bytes(footprint),
        format_bytes(cap_bytes)
    );
    log::error!("{message}");
    record_last_state(&message);
    zlog::flush();
    eprintln!("{message}");
    // Jetsam is an uncatchable SIGKILL; match it so nothing gets to clean up.
    #[cfg(unix)]
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn watchdog_warns_once_per_excursion_and_rearms() {
        let mut state = WatchdogState::new(1000 * MIB);
        assert_eq!(state.sample(500 * MIB), WatchdogAction::None);
        assert_eq!(state.sample(800 * MIB), WatchdogAction::Warn);
        assert_eq!(state.sample(900 * MIB), WatchdogAction::None);
        // Still above the re-arm line: no second warning.
        assert_eq!(state.sample(750 * MIB), WatchdogAction::None);
        assert_eq!(state.sample(850 * MIB), WatchdogAction::None);
        assert_eq!(state.sample(600 * MIB), WatchdogAction::None);
        assert_eq!(state.sample(810 * MIB), WatchdogAction::Warn);
    }

    #[test]
    fn watchdog_kills_at_cap() {
        let mut state = WatchdogState::new(1000 * MIB);
        assert_eq!(state.sample(999 * MIB), WatchdogAction::Warn);
        assert_eq!(state.sample(1000 * MIB), WatchdogAction::Jetsam);
    }

    #[test]
    fn warning_reaches_every_subscriber() {
        use std::sync::atomic::AtomicUsize;
        let count = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let count = count.clone();
            on_warning(move |source| {
                if source == WarningSource::Test {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        raise_warning(WarningSource::Test);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[cfg(any(target_os = "ios", target_os = "macos"))]
    #[test]
    fn footprint_is_reported_and_grows_with_allocation() {
        let before = footprint_bytes().expect("task_info succeeds");
        let block = vec![1u8; 64 * MIB as usize];
        let after = footprint_bytes().expect("task_info succeeds");
        assert!(before > 0);
        assert!(after >= before + 32 * MIB, "before {before}, after {after}");
        drop(block);
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(512 * MIB), "512 MB");
        assert_eq!(format_bytes(3 * 1024 * MIB), "3.00 GB");
    }
}
