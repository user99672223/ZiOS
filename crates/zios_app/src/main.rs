//! iOS app entry point.
//!
//! Until Zed itself boots on iOS (phase 3), this hosts a small GPUI view that
//! proves the platform layer, logging, and strict Simulator mode work.

#[cfg(target_os = "ios")]
mod app;
#[cfg(target_os = "ios")]
mod ci;

#[cfg(target_os = "ios")]
fn main() {
    if let Err(error) = ios_bridge::logging::init() {
        eprintln!("logging unavailable: {error:#}");
    }
    let strict = ios_bridge::strict::StrictConfig::from_env();
    log::info!(
        "ZiOS starting: strict mode {}, {strict:?}",
        if ios_bridge::strict::ENABLED { "on" } else { "off" }
    );
    if ios_bridge::strict::ENABLED {
        log::info!(
            "strict interposer loaded: {}",
            ios_bridge::strict::interposer_loaded()
        );
    }
    ios_bridge::jit::start_monitor(strict.force_jit, std::time::Duration::from_secs(5));
    if let Some(cap) = strict.memory_cap_mb {
        ios_bridge::memory::start_watchdog(cap * 1024 * 1024, std::time::Duration::from_millis(250));
    }
    app::run(strict);
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("Build this crate for an iOS target with scripts/build_app.sh.");
}
