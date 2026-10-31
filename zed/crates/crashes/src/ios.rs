//! iOS crash reporting.
//!
//! iOS forbids the minidump sidecar process. Native crashes and hangs are
//! collected by MetricKit and Rust panics by the panic hook installed by
//! `ios_bridge::logging`; both are surfaced by the app's diagnostics export.
//! This module keeps Zed's crash-handler API so callers need no iOS branches,
//! and remembers the session metadata so diagnostics can include it.

use std::{collections::BTreeMap, future::Future, panic::Location, path::Path, pin::Pin, sync::Arc};

use parking_lot::Mutex;
use system_specs::GpuSpecs;

use crate::{InitCrashHandler, SENTRY_USER_ID, UserInfo};

/// Session metadata that a desktop build would send to its crash server.
#[derive(Default)]
pub struct Client {
    init: Mutex<Option<InitCrashHandler>>,
    gpu: Mutex<Option<GpuSpecs>>,
    tags: Mutex<BTreeMap<String, String>>,
}

impl Client {
    /// A plain-text summary for exported diagnostics.
    pub fn summary(&self) -> String {
        let mut summary = String::new();
        if let Some(init) = self.init.lock().as_ref() {
            summary.push_str(&format!(
                "session {} zed {} ({}, {})\n",
                init.session_id, init.zed_version, init.release_channel, init.commit_sha
            ));
        }
        if let Some(gpu) = self.gpu.lock().as_ref() {
            summary.push_str(&format!("gpu {gpu:?}\n"));
        }
        for (key, value) in self.tags.lock().iter() {
            summary.push_str(&format!("{key} = {value}\n"));
        }
        summary
    }
}

static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);

/// The client created by [`init`], if any.
pub fn current_client() -> Option<Arc<Client>> {
    CLIENT.lock().clone()
}

pub fn force_backtrace() {
    // SAFETY: called during startup before other threads read the environment.
    unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
}

pub fn init<F, S, C, P>(
    crash_init: InitCrashHandler,
    _spawn: S,
    _socket_path: P,
    _wait_timer: C,
) -> impl Future<Output = Arc<Client>> + use<F, C, S, P>
where
    F: Future<Output = ()> + Send + Sync + 'static,
    C: (Fn(std::time::Duration) -> F) + Send + Sync + 'static,
    S: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    P: FnOnce(u32) -> std::path::PathBuf,
{
    let client = Arc::new(Client::default());
    *client.init.lock() = Some(crash_init);
    *CLIENT.lock() = Some(client.clone());
    std::future::ready(client)
}

pub fn set_gpu_info(crash_client: &Arc<Client>, specs: GpuSpecs) {
    *crash_client.gpu.lock() = Some(specs);
}

pub fn set_user_info(crash_client: &Arc<Client>, info: UserInfo) {
    set_tags(
        crash_client,
        [
            (SENTRY_USER_ID, info.metrics_id),
            (
                "sentry[user][is_staff]",
                info.is_staff.map(|is_staff| is_staff.to_string()),
            ),
        ],
    );
}

pub fn set_tag(crash_client: &Arc<Client>, key: impl Into<String>, value: Option<String>) {
    set_tags(crash_client, [(key, value)]);
}

pub fn set_tags<K>(crash_client: &Arc<Client>, tags: impl IntoIterator<Item = (K, Option<String>)>)
where
    K: Into<String>,
{
    let mut stored = crash_client.tags.lock();
    for (key, value) in tags {
        let key = key.into();
        match value {
            Some(value) => {
                stored.insert(key, value);
            }
            None => {
                stored.remove(&key);
            }
        }
    }
}

pub fn shutdown_crash_handler(_crash_client: &Arc<Client>) {}

/// Panics are recorded by `ios_bridge`'s panic hook, which runs for every panic.
pub fn panic_hook(_crash_client: Arc<Client>, message: &str, location: Option<&Location>) {
    log::error!(
        "panic: {message} at {}",
        location.map_or_else(|| "unknown location".to_string(), ToString::to_string)
    );
}

/// The crash server is a separate process on desktop; it never runs on iOS.
pub fn crash_server(_socket: &Path, _logs_dir: std::path::PathBuf) {
    log::error!("the crash server process is not available on iOS");
}
