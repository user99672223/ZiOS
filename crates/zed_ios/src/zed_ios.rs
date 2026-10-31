//! iOS integration for the Zed workspace: memory-warning wiring and the CI
//! script runner. Later phases add the device status panel and the external
//! display companion here.

pub mod ci;
pub mod diagnostics;
#[cfg(target_os = "ios")]
pub mod display;
pub mod lan_mouse;
pub mod status;

use std::sync::Arc;

use gpui::App;
use ios_bridge::memory::{self, WarningSource};
use workspace::AppState;

/// Called once the workspace is initialized.
pub fn init(app_state: Arc<AppState>, cx: &mut App) {
    // Folders picked in the Files app need their security scope reopened.
    #[cfg(target_os = "ios")]
    for folder in gpui_ios::ios::restore_bookmarked_folders() {
        log::info!("restored access to {}", folder.display());
        ios_bridge::strict::allow_sandbox_root(&folder);
    }
    cx.on_memory_warning(|_cx| memory::raise_warning(WarningSource::System))
        .detach();
    #[cfg(target_os = "ios")]
    display::init(cx);
    lan_mouse::init(cx);
    status::init(cx);
    diagnostics::init(cx);
    ci::start(app_state, cx);
}
