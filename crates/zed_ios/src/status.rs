//! The Device Status status-bar item and panel (CLAUDE.md section 9.3).

use std::time::Duration;

use gpui::{
    Action, ClickEvent, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    Subscription, Task, actions,
};
use ios_bridge::{
    device::ThermalState,
    entitlements::Grant,
    memory,
    status::{NO_JIT_EXPLANATION, Severity, Snapshot},
};
use project::Project;
use ui::{Tooltip, prelude::*};
use workspace::{ModalView, StatusItemView, Workspace, item::ItemHandle};

actions!(
    zed_ios,
    [
        /// Shows JIT, memory, thermal, display, lan-mouse, and language server status.
        OpenDeviceStatus
    ]
);

/// How often the status bar icon re-evaluates device state.
const BUTTON_REFRESH: Duration = Duration::from_secs(5);
/// How often the open panel refreshes its numbers.
const PANEL_REFRESH: Duration = Duration::from_secs(1);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        workspace.register_action(|workspace, _: &OpenDeviceStatus, window, cx| {
            let project = workspace.project().clone();
            workspace.toggle_modal(window, cx, |_window, cx| {
                DeviceStatusModal::new(project, cx)
            });
        });
        let button = cx.new(DeviceStatusButton::new);
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(button, window, cx);
        });
    })
    .detach();
}

fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Ok => Color::Muted,
        Severity::Warning => Color::Warning,
        Severity::Critical => Color::Error,
    }
}

pub struct DeviceStatusButton {
    severity: Severity,
    _refresh: Task<()>,
}

impl DeviceStatusButton {
    fn new(cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                let snapshot = cx
                    .background_executor()
                    .spawn(async { Snapshot::collect() })
                    .await;
                let updated = this.update(cx, |this, cx| {
                    let severity = snapshot.severity();
                    if this.severity != severity {
                        this.severity = severity;
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor().timer(BUTTON_REFRESH).await;
            }
        });
        Self {
            severity: Severity::Ok,
            _refresh: refresh,
        }
    }
}

impl Render for DeviceStatusButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        IconButton::new("device-status", IconName::Server)
            .icon_size(IconSize::Small)
            .icon_color(severity_color(self.severity))
            .tooltip(Tooltip::text("Device Status"))
            .on_click(|_: &ClickEvent, window, cx| {
                window.dispatch_action(OpenDeviceStatus.boxed_clone(), cx);
            })
    }
}

impl StatusItemView for DeviceStatusButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<workspace::HideStatusItem> {
        // The device status must stay reachable on iOS.
        None
    }
}

pub struct DeviceStatusModal {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    snapshot: Snapshot,
    _refresh: Task<()>,
    _project_subscription: Subscription,
}

impl DeviceStatusModal {
    fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PANEL_REFRESH).await;
                let snapshot = cx
                    .background_executor()
                    .spawn(async { Snapshot::collect() })
                    .await;
                let updated = this.update(cx, |this, cx| {
                    this.snapshot = snapshot;
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let lsp_store = project.read(cx).lsp_store();
        let project_subscription = cx.observe(&lsp_store, |_, _, cx| cx.notify());
        Self {
            focus_handle: cx.focus_handle(),
            project,
            snapshot: Snapshot::collect(),
            _refresh: refresh,
            _project_subscription: project_subscription,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn section(title: &'static str) -> impl IntoElement {
        div()
            .pt_2()
            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
    }

    fn row(label: impl Into<SharedString>, value: impl Into<SharedString>, color: Color) -> impl IntoElement {
        h_flex()
            .w_full()
            .justify_between()
            .gap_4()
            .child(Label::new(label))
            .child(Label::new(value).color(color))
    }

    fn language_server_rows(&self, cx: &App) -> Vec<AnyElement> {
        let lsp_store = self.project.read(cx).lsp_store();
        let rows: Vec<AnyElement> = lsp_store
            .read(cx)
            .language_server_statuses()
            .map(|(_, status)| {
                let state = if status.pending_work.is_empty() {
                    "running"
                } else {
                    "busy"
                };
                Self::row(status.name.to_string(), state, Color::Success).into_any_element()
            })
            .collect();
        if rows.is_empty() {
            vec![
                Self::row("Language servers", "none running", Color::Muted).into_any_element(),
            ]
        } else {
            rows
        }
    }
}

impl EventEmitter<DismissEvent> for DeviceStatusModal {}

impl Focusable for DeviceStatusModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for DeviceStatusModal {}

impl Render for DeviceStatusModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = &self.snapshot;
        let jit_enabled = snapshot.jit.enabled();
        let yes_no = |ok: bool| if ok { Color::Success } else { Color::Warning };
        let bytes = |value: Option<u64>| value.map_or_else(|| "unknown".to_string(), memory::format_bytes);
        let jit_value = format!(
            "{} ({}){}",
            if jit_enabled { "Enabled" } else { "Not enabled" },
            snapshot.jit.method.label(),
            snapshot
                .jit
                .forced
                .map_or_else(String::new, |force| format!(" · forced {force:?}")),
        );
        let memory_grant = snapshot.entitlements.increased_memory_limit;
        let thermal_color = match snapshot.thermal {
            ThermalState::Nominal | ThermalState::Fair => Color::Success,
            ThermalState::Serious => Color::Warning,
            ThermalState::Critical => Color::Error,
        };
        let displays = cx.displays();
        let display_value = match displays.len() {
            0 | 1 => "phone only".to_string(),
            count => format!("{} connected", count - 1),
        };
        let addresses: Vec<AnyElement> = if snapshot.addresses.is_empty() {
            vec![Self::row("Addresses", "no network", Color::Warning).into_any_element()]
        } else {
            snapshot
                .addresses
                .iter()
                .map(|address| {
                    Self::row(
                        format!("{} ({:?})", address.interface, address.kind),
                        address.address.to_string(),
                        Color::Default,
                    )
                    .into_any_element()
                })
                .collect()
        };
        let max_height = window.viewport_size().height - px(120.);

        v_flex()
            .id("device-status")
            .key_context("DeviceStatus")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .w(rems(30.))
            .max_h(max_height)
            .overflow_y_scroll()
            .p_4()
            .gap_1()
            .elevation_3(cx)
            .child(
                h_flex()
                    .justify_between()
                    .child(Headline::new("Device Status").size(HeadlineSize::Small))
                    .child(IconButton::new("close", IconName::Close).on_click(cx.listener(
                        |_, _: &ClickEvent, _window, cx| cx.emit(DismissEvent),
                    ))),
            )
            .child(Self::section("JIT"))
            .child(Self::row("JIT", jit_value, yes_no(jit_enabled)))
            .child(Self::row("Wasmtime", snapshot.wasmtime_mode(), yes_no(jit_enabled)))
            .child(Self::row("Node", snapshot.node_mode(), yes_no(jit_enabled)))
            .when(!jit_enabled, |this| {
                this.child(
                    Label::new(NO_JIT_EXPLANATION)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(Self::section("Memory"))
            .child(Self::row(
                "Increased Memory Limit",
                Snapshot::grant_label(memory_grant),
                yes_no(memory_grant == Grant::Granted),
            ))
            .child(Self::row("Available", bytes(snapshot.available), Color::Default))
            .child(Self::row("App uses", bytes(snapshot.footprint), Color::Default))
            .child(Self::section("Power"))
            .child(Self::row("Thermal state", snapshot.thermal.label(), thermal_color))
            .child(Self::row(
                "Low Power Mode",
                if snapshot.low_power { "on" } else { "off" },
                yes_no(!snapshot.low_power),
            ))
            .child(Self::section("External display"))
            .child(Self::row("Monitor", display_value, Color::Default))
            .child(Self::section("lan-mouse"))
            .child(Self::row("Receiver", "not running", Color::Muted))
            .children(addresses)
            .child(Self::section("Language servers"))
            .children(self.language_server_rows(cx))
    }
}
