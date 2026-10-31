//! Crash reporting.
//!
//! On desktop platforms a sidecar process (Zed re-executed as a minidump server)
//! captures crashes. iOS cannot start processes, so there the same API records
//! panics in-process and relies on MetricKit for native crashes; see
//! `ios_bridge` and docs/DECISIONS.md.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg(not(target_os = "ios"))]
mod native;
#[cfg(not(target_os = "ios"))]
pub use native::*;

#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "ios")]
pub use ios::*;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CrashInfo {
    pub init: InitCrashHandler,
    pub panic: Option<CrashPanic>,
    pub minidump_error: Option<String>,
    /// The diagnostic the C runtime recorded before aborting the process, e.g.
    /// glibc's "free(): invalid pointer". Only present when the crash was a
    /// runtime-initiated abort rather than a signal like SIGSEGV or a panic.
    #[serde(default)]
    pub abort_message: Option<String>,
    pub gpus: Vec<system_specs::GpuInfo>,
    pub active_gpu: Option<system_specs::GpuSpecs>,
    /// Session state the crashed application attached with [`set_tag`], keyed
    /// by Sentry submission field.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

/// Where to find the C runtime's abort diagnostic in the crashed process's
/// memory. Sent by the client at startup so that after a crash the server can
/// recover the message with `process_vm_readv`; the crashed process itself
/// can't safely do this work, since its heap may be corrupt and its allocator
/// locks may be held by the crashed thread.
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
pub struct AbortMessageLocation {
    pub pid: u32,
    pub address: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct InitCrashHandler {
    pub session_id: String,
    pub zed_version: String,
    pub binary: String,
    pub release_channel: String,
    pub commit_sha: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CrashPanic {
    pub message: String,
    pub span: String,
}

/// The Sentry field identifying the crashing user, which the uploader falls
/// back to filling in itself when a session never reported one.
pub const SENTRY_USER_ID: &str = "sentry[user][id]";

#[derive(Debug, Clone)]
pub struct UserInfo {
    pub metrics_id: Option<String>,
    pub is_staff: Option<bool>,
}
