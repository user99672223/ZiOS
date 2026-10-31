//! JIT availability detection (CLAUDE.md section 9.1).
//!
//! iOS only lets a process map writable memory executable while a debugger is
//! attached (the `CS_DEBUGGED` code-signing flag, set by StikDebug, SideStore's
//! JIT enabler, or Xcode). How executable memory is then obtained depends on the
//! device: before iOS 26, or on chips without the Trusted Execution Monitor
//! (TXM), the classic RW/RX dual mapping works; on iOS 26+ with TXM (A15 and
//! newer) the debugger has to cooperate for every executable region.
//!
//! Detection is pure logic over injected inputs so every branch is unit-tested;
//! [`SystemProbe`] supplies the real values.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

use parking_lot::Mutex;

use crate::strict::ForceJit;

/// `CS_OPS_STATUS` from `<sys/codesign.h>`.
const CS_OPS_STATUS: u32 = 0;
/// The process is being debugged, so it may create executable mappings.
pub const CS_DEBUGGED: u32 = 0x1000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl OsVersion {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('.').map(|part| part.parse::<u32>());
        let major = parts.next()?.ok()?;
        let minor = parts.next().unwrap_or(Ok(0)).ok()?;
        let patch = parts.next().unwrap_or(Ok(0)).ok()?;
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for OsVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// How the app gets executable memory for JIT code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitMethod {
    /// RW and RX views of the same pages via `mach_vm_remap`.
    DualMapping,
    /// iOS 26+ on TXM devices: the attached debugger prepares each region.
    Txm,
    /// No JIT: Wasmtime runs on Pulley, Node runs `--jitless`.
    None,
}

impl JitMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::DualMapping => "dual-mapping",
            Self::Txm => "TXM",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitStatus {
    /// Whether the code-signing status says a debugger enabled JIT.
    pub debugged: bool,
    pub method: JitMethod,
    /// Set when `ZED_IOS_FORCE_JIT` overrode real detection.
    pub forced: Option<ForceJit>,
    pub os_version: Option<OsVersion>,
    pub machine: String,
    pub has_txm: bool,
}

impl JitStatus {
    pub fn enabled(&self) -> bool {
        self.method != JitMethod::None
    }
}

/// Whether a device model has the Trusted Execution Monitor: A15 and newer
/// iPhones (`iPhone14,x`+) and A15/M2-class iPads (`iPad14,x`+).
pub fn model_has_txm(machine: &str) -> bool {
    let generation = |prefix: &str| {
        machine
            .strip_prefix(prefix)
            .and_then(|rest| rest.split(',').next())
            .and_then(|major| major.parse::<u32>().ok())
    };
    if let Some(generation) = generation("iPhone") {
        return generation >= 14;
    }
    if let Some(generation) = generation("iPad") {
        return generation >= 14;
    }
    false
}

/// TXM changes how JIT memory is obtained starting with iOS 26.
pub const TXM_JIT_MIN_MAJOR: u32 = 26;

/// Decides the JIT status from raw inputs.
pub fn decide(
    csops_status: Option<u32>,
    os_version: Option<OsVersion>,
    machine: &str,
    force: ForceJit,
) -> JitStatus {
    let has_txm = model_has_txm(machine);
    let debugged = csops_status.is_some_and(|flags| flags & CS_DEBUGGED != 0);
    let available = match force {
        ForceJit::Off => false,
        ForceJit::On => true,
        ForceJit::Auto => debugged,
    };
    let needs_txm_protocol = has_txm
        && os_version.is_some_and(|version| version.major >= TXM_JIT_MIN_MAJOR);
    let method = match (available, needs_txm_protocol) {
        (false, _) => JitMethod::None,
        (true, true) => JitMethod::Txm,
        (true, false) => JitMethod::DualMapping,
    };
    JitStatus {
        debugged,
        method,
        forced: (force != ForceJit::Auto).then_some(force),
        os_version,
        machine: machine.to_string(),
        has_txm,
    }
}

/// Real system inputs.
pub struct SystemProbe;

impl SystemProbe {
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub fn csops_status() -> Option<u32> {
        unsafe extern "C" {
            fn csops(
                pid: libc::pid_t,
                ops: libc::c_uint,
                useraddr: *mut libc::c_void,
                usersize: libc::size_t,
            ) -> libc::c_int;
        }
        let mut flags: u32 = 0;
        let result = unsafe {
            csops(
                libc::getpid(),
                CS_OPS_STATUS,
                (&raw mut flags).cast(),
                std::mem::size_of::<u32>(),
            )
        };
        (result == 0).then_some(flags)
    }

    #[cfg(not(any(target_os = "ios", target_os = "macos")))]
    pub fn csops_status() -> Option<u32> {
        None
    }

    pub fn os_version() -> Option<OsVersion> {
        sysctl_string("kern.osproductversion").and_then(|text| OsVersion::parse(&text))
    }

    /// The device model, such as `iPhone15,4`. The Simulator reports the host's
    /// CPU, so prefer the simulated model it exports in the environment.
    pub fn machine() -> String {
        std::env::var("SIMULATOR_MODEL_IDENTIFIER")
            .ok()
            .or_else(|| sysctl_string("hw.machine"))
            .unwrap_or_else(|| "unknown".to_string())
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn sysctl_string(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut length: libc::size_t = 0;
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || length == 0 {
        return None;
    }
    let mut buffer = vec![0u8; length];
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    buffer.truncate(length);
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    String::from_utf8(buffer).ok()
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn sysctl_string(_name: &str) -> Option<String> {
    None
}

/// Probes the real system now.
pub fn detect(force: ForceJit) -> JitStatus {
    decide(
        SystemProbe::csops_status(),
        SystemProbe::os_version(),
        &SystemProbe::machine(),
        force,
    )
}

static CURRENT: Mutex<Option<JitStatus>> = Mutex::new(None);
static FORCE: OnceLock<ForceJit> = OnceLock::new();
static MONITOR_STARTED: AtomicBool = AtomicBool::new(false);

/// The most recent detection result, probing now if the monitor has not run.
pub fn current() -> JitStatus {
    if let Some(status) = CURRENT.lock().clone() {
        return status;
    }
    let status = detect(FORCE.get().copied().unwrap_or_default());
    *CURRENT.lock() = Some(status.clone());
    status
}

type JitCallback = Box<dyn Fn(&JitStatus) + Send + Sync>;
static SUBSCRIBERS: Mutex<Vec<JitCallback>> = Mutex::new(Vec::new());

/// Runs `callback` (on the monitor thread) whenever the JIT status changes, for
/// example when the user attaches a debugger after launch.
pub fn on_change(callback: impl Fn(&JitStatus) + Send + Sync + 'static) {
    SUBSCRIBERS.lock().push(Box::new(callback));
}

/// Detects JIT at launch and then every `interval` (5 s per CLAUDE.md).
pub fn start_monitor(force: ForceJit, interval: std::time::Duration) {
    if MONITOR_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if FORCE.set(force).is_err() {
        log::debug!("JIT force mode was already set");
    }
    let initial = detect(force);
    log::info!(
        "JIT: {} (method {}, debugged {}, iOS {}, {}{})",
        if initial.enabled() { "enabled" } else { "not enabled" },
        initial.method.label(),
        initial.debugged,
        initial
            .os_version
            .map_or_else(|| "unknown".to_string(), |version| version.to_string()),
        initial.machine,
        initial
            .forced
            .map_or_else(String::new, |force| format!(", forced {force:?}")),
    );
    *CURRENT.lock() = Some(initial);
    let spawned = std::thread::Builder::new()
        .name("jit-monitor".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                let status = detect(force);
                let changed = {
                    let mut current = CURRENT.lock();
                    let changed = current.as_ref() != Some(&status);
                    *current = Some(status.clone());
                    changed
                };
                if changed {
                    log::info!(
                        "JIT status changed: {} via {}",
                        if status.enabled() { "enabled" } else { "not enabled" },
                        status.method.label()
                    );
                    for subscriber in SUBSCRIBERS.lock().iter() {
                        subscriber(&status);
                    }
                }
            }
        });
    if let Err(error) = spawned {
        log::error!("failed to start the JIT monitor: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IOS_18: Option<OsVersion> = Some(OsVersion {
        major: 18,
        minor: 6,
        patch: 2,
    });
    const IOS_26: Option<OsVersion> = Some(OsVersion {
        major: 26,
        minor: 1,
        patch: 0,
    });
    const IPHONE_15: &str = "iPhone15,4";
    const IPHONE_12: &str = "iPhone13,2";

    #[test]
    fn not_debugged_means_no_jit() {
        let status = decide(Some(0x0300_0001), IOS_26, IPHONE_15, ForceJit::Auto);
        assert!(!status.debugged);
        assert_eq!(status.method, JitMethod::None);
        assert!(!status.enabled());
    }

    #[test]
    fn failed_csops_means_no_jit() {
        let status = decide(None, IOS_26, IPHONE_15, ForceJit::Auto);
        assert_eq!(status.method, JitMethod::None);
    }

    #[test]
    fn debugged_txm_device_on_ios_26_uses_txm() {
        let status = decide(Some(CS_DEBUGGED | 0x1), IOS_26, IPHONE_15, ForceJit::Auto);
        assert!(status.debugged);
        assert!(status.has_txm);
        assert_eq!(status.method, JitMethod::Txm);
    }

    #[test]
    fn debugged_txm_device_before_ios_26_uses_dual_mapping() {
        let status = decide(Some(CS_DEBUGGED), IOS_18, IPHONE_15, ForceJit::Auto);
        assert!(status.has_txm);
        assert_eq!(status.method, JitMethod::DualMapping);
    }

    #[test]
    fn debugged_pre_txm_device_uses_dual_mapping_on_any_version() {
        let status = decide(Some(CS_DEBUGGED), IOS_26, IPHONE_12, ForceJit::Auto);
        assert!(!status.has_txm);
        assert_eq!(status.method, JitMethod::DualMapping);
    }

    #[test]
    fn unknown_version_falls_back_to_dual_mapping() {
        let status = decide(Some(CS_DEBUGGED), None, IPHONE_15, ForceJit::Auto);
        assert_eq!(status.method, JitMethod::DualMapping);
    }

    #[test]
    fn forcing_overrides_detection() {
        let off = decide(Some(CS_DEBUGGED), IOS_26, IPHONE_15, ForceJit::Off);
        assert_eq!(off.method, JitMethod::None);
        assert_eq!(off.forced, Some(ForceJit::Off));
        assert!(off.debugged, "the real flag is still reported");

        let on = decide(Some(0), IOS_18, IPHONE_12, ForceJit::On);
        assert_eq!(on.method, JitMethod::DualMapping);
        assert_eq!(on.forced, Some(ForceJit::On));
    }

    #[test]
    fn txm_models() {
        assert!(model_has_txm("iPhone14,5"), "iPhone 13 (A15)");
        assert!(model_has_txm("iPhone15,4"), "iPhone 15 (A16)");
        assert!(model_has_txm("iPhone17,1"), "iPhone 16 Pro (A18 Pro)");
        assert!(!model_has_txm("iPhone13,2"), "iPhone 12 (A14)");
        assert!(model_has_txm("iPad14,1"));
        assert!(!model_has_txm("arm64"));
        assert!(!model_has_txm(""));
    }

    #[test]
    fn parses_versions() {
        assert_eq!(
            OsVersion::parse("26.1"),
            Some(OsVersion {
                major: 26,
                minor: 1,
                patch: 0
            })
        );
        assert_eq!(OsVersion::parse("18.6.2"), IOS_18);
        assert_eq!(OsVersion::parse("beta"), None);
    }

    #[cfg(any(target_os = "ios", target_os = "macos"))]
    #[test]
    fn real_probe_runs() {
        // A test process is normally not debugged; either way csops answers.
        assert!(SystemProbe::csops_status().is_some());
        assert!(SystemProbe::os_version().is_some());
    }
}
