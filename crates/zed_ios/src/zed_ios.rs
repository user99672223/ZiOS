//! iOS integration for the Zed workspace: memory-warning wiring and the CI
//! script runner. Later phases add the device status panel and the external
//! display companion here.

pub mod ci;
pub mod status;

use std::sync::Arc;

use gpui::App;
use ios_bridge::memory::{self, WarningSource};
use workspace::AppState;

/// Called once the workspace is initialized.
pub fn init(app_state: Arc<AppState>, cx: &mut App) {
    cx.on_memory_warning(|_cx| memory::raise_warning(WarningSource::System))
        .detach();
    status::init(cx);
    ci::start(app_state, cx);
}
