//! Locations inside the app container.
//!
//! On iOS `HOME` is the app's data container, so the same code works on the
//! Simulator, the device, and on macOS (where it resolves to the user's home and is
//! only used by tests).

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// User-visible files. Exposed in the Files app through `UIFileSharingEnabled`.
pub fn documents() -> PathBuf {
    home().join("Documents")
}

/// Persistent app state that the user does not see (settings, databases).
pub fn application_support() -> PathBuf {
    home().join("Library/Application Support/Zed")
}

/// Rebuildable data. iOS may purge it under storage pressure.
pub fn caches() -> PathBuf {
    home().join("Library/Caches/Zed")
}

pub fn logs() -> PathBuf {
    home().join("Library/Logs/Zed")
}

/// Crash reports, panic records, and jetsam breadcrumbs.
pub fn crashes() -> PathBuf {
    logs().join("crashes")
}

pub fn temporary() -> PathBuf {
    home().join("tmp")
}
