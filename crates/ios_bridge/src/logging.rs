//! Structured logging to `os_log` and a rotating file in the app container.
//!
//! Zed's own logger (`zlog`) is the single `log` backend. This module points its
//! file sink at the container, raises the rotation size so roughly 20 MB of history
//! is kept (current file plus one rotated file), and mirrors every record to
//! `os_log` so Console.app shows it while the phone is attached.

use std::{
    io::Write as _,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use anyhow::{Context as _, Result};

use crate::paths;

/// Each file is capped at this size; one rotated file is kept, so the total stays
/// near 20 MB.
pub const LOG_FILE_SIZE_LIMIT: u64 = 10 * 1024 * 1024;

pub const SUBSYSTEM: &str = "dev.zios.app";

static LOG_PATHS: OnceLock<LogPaths> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct LogPaths {
    pub current: PathBuf,
    pub rotated: PathBuf,
}

pub fn log_paths() -> Option<&'static LogPaths> {
    LOG_PATHS.get()
}

/// Installs the logger. Safe to call more than once; later calls are no-ops.
pub fn init() -> Result<&'static LogPaths> {
    if let Some(paths) = LOG_PATHS.get() {
        return Ok(paths);
    }
    let directory = paths::logs();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating log directory {}", directory.display()))?;
    let log_paths = LOG_PATHS.get_or_init(|| LogPaths {
        current: directory.join("Zed.log"),
        rotated: directory.join("Zed.log.old"),
    });

    zlog::set_file_size_limit(LOG_FILE_SIZE_LIMIT);
    zlog::init();
    zlog::init_output_file(&log_paths.current, Some(&log_paths.rotated))
        .with_context(|| format!("opening {}", log_paths.current.display()))?;
    if zlog::init_output_extra(os_log_sink).is_err() {
        log::debug!("os_log sink was already installed");
    }
    install_panic_hook();
    Ok(log_paths)
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn os_log_sink(level: log::Level, message: &str) {
    static OS_LOG: OnceLock<oslog::OsLog> = OnceLock::new();
    let os_log = OS_LOG.get_or_init(|| oslog::OsLog::new(SUBSYSTEM, "zed"));
    let level = match level {
        log::Level::Error => oslog::Level::Error,
        log::Level::Warn => oslog::Level::Default,
        log::Level::Info => oslog::Level::Info,
        log::Level::Debug | log::Level::Trace => oslog::Level::Debug,
    };
    os_log.with_level(level, message);
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn os_log_sink(_level: log::Level, _message: &str) {}

/// Records Rust panics, with a backtrace, in the log and as a file in the crash
/// folder so the next launch can report them. The previous hook still runs.
fn install_panic_hook() {
    static INSTALLED: Mutex<bool> = Mutex::new(false);
    let mut installed = INSTALLED.lock().unwrap_or_else(|error| error.into_inner());
    if *installed {
        return;
    }
    *installed = true;

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!("panic on thread '{thread_name}': {info}\n{backtrace}");
        log::error!("{report}");
        zlog::flush();
        if let Err(error) = write_crash_record("panic", &report) {
            eprintln!("failed to record panic: {error}");
        }
        previous_hook(info);
    }));
}

/// Writes a crash record that is picked up by diagnostics on the next launch.
pub fn write_crash_record(kind: &str, contents: &str) -> Result<PathBuf> {
    let directory = paths::crashes();
    std::fs::create_dir_all(&directory)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let path = directory.join(format!("{kind}-{timestamp}.txt"));
    let mut file = std::fs::File::create(&path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(path)
}
