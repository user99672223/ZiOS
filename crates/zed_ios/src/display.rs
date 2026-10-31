//! External display support on the Zed side (CLAUDE.md section 10): reacts to
//! the editor moving to a monitor by opening the phone's companion view, and
//! closes it when the editor comes back.

use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, Bounds, ClickEvent, Context, DispatchPhase, KeyDownEvent, KeyUpEvent,
    Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
    Point, Render, ScrollEvent, ScrollSource, Task, TouchDragEvent, TouchPhase, Window,
    WindowHandle, WindowOptions, canvas, px,
};
use gpui_ios::ios::{self as platform, ExternalDisplayEvent, ExternalDisplayMode};
use ios_bridge::status::Snapshot;
use ui::{ToggleButtonGroup, ToggleButtonSimple, prelude::*};

/// Pointer speed of the companion trackpad relative to finger movement.
const TRACKPAD_SPEED: f32 = 2.0;

struct CompanionWindow(Option<WindowHandle<CompanionView>>);

impl gpui::Global for CompanionWindow {}

pub fn init(cx: &mut App) {
    cx.set_global(CompanionWindow(None));
    let (sender, mut receiver) = futures::channel::mpsc::unbounded();
    platform::on_external_display_event(move |event| {
        // UIKit calls this synchronously; handle it once the callback unwinds.
        if sender.unbounded_send(event).is_err() {
            log::debug!("display events are no longer observed");
        }
    });
    cx.spawn(async move |cx| {
        use futures::StreamExt as _;
        while let Some(event) = receiver.next().await {
            log::info!("display: {event:?}");
            cx.update(|cx| match event {
                ExternalDisplayEvent::EditorMoved { on_external: true } => open_companion(cx),
                ExternalDisplayEvent::EditorMoved { on_external: false } => close_companion(cx),
                ExternalDisplayEvent::Connected(_) | ExternalDisplayEvent::Disconnected => {}
            });
        }
    })
    .detach();
}

/// The window showing the editor: the first one opened.
fn editor_window(cx: &App) -> Option<AnyWindowHandle> {
    cx.windows().into_iter().next()
}

fn open_companion(cx: &mut App) {
    if cx.global::<CompanionWindow>().0.is_some() {
        return;
    }
    match cx.open_window(WindowOptions::default(), |_, cx| cx.new(CompanionView::new)) {
        Ok(window) => cx.global_mut::<CompanionWindow>().0 = Some(window),
        Err(error) => log::error!("could not open the companion view: {error:#}"),
    }
}

fn close_companion(cx: &mut App) {
    platform::set_phone_dimmed(false);
    if let Some(window) = cx.global_mut::<CompanionWindow>().0.take() {
        let result = window.update(cx, |_, window, _| window.remove_window());
        if let Err(error) = result {
            log::debug!("companion view already closed: {error:#}");
        }
    }
}

pub struct CompanionView {
    snapshot: Snapshot,
    pointer: Point<gpui::Pixels>,
    sticky_modifiers: Modifiers,
    last_drag: Option<Point<gpui::Pixels>>,
    _refresh: Task<()>,
}

impl CompanionView {
    fn new(cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let snapshot = cx
                    .background_executor()
                    .spawn(async { Snapshot::collect() })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.snapshot = snapshot;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            snapshot: Snapshot::collect(),
            pointer: Point::new(px(200.), px(200.)),
            sticky_modifiers: Modifiers::default(),
            last_drag: None,
            _refresh: refresh,
        }
    }

    fn editor_size(cx: &mut App) -> Option<gpui::Size<gpui::Pixels>> {
        let editor = editor_window(cx)?;
        editor
            .update(cx, |_, window, _| window.viewport_size())
            .ok()
    }

    fn move_pointer(&mut self, delta: Point<gpui::Pixels>, cx: &mut App) {
        let Some(size) = Self::editor_size(cx) else {
            return;
        };
        self.pointer.x = (self.pointer.x + delta.x * TRACKPAD_SPEED).clamp(px(0.), size.width - px(1.));
        self.pointer.y = (self.pointer.y + delta.y * TRACKPAD_SPEED).clamp(px(0.), size.height - px(1.));
        platform::inject_input(PlatformInput::MouseMove(MouseMoveEvent {
            position: self.pointer,
            pressed_button: None,
            modifiers: self.sticky_modifiers,
        }));
    }

    fn click(&self, button: MouseButton) {
        for down in [true, false] {
            let input = if down {
                PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position: self.pointer,
                    modifiers: self.sticky_modifiers,
                    click_count: 1,
                    first_mouse: false,
                })
            } else {
                PlatformInput::MouseUp(MouseUpEvent {
                    button,
                    position: self.pointer,
                    modifiers: self.sticky_modifiers,
                    click_count: 1,
                })
            };
            platform::inject_input(input);
        }
    }

    fn press_key(&mut self, key: &str) {
        let Ok(mut keystroke) = Keystroke::parse(key) else {
            return;
        };
        keystroke.modifiers = keystroke.modifiers | self.sticky_modifiers;
        let keystroke = keystroke.with_simulated_ime();
        platform::inject_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        }));
        platform::inject_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
        // Sticky modifiers apply to one key, like on-screen modifier keys.
        self.sticky_modifiers = Modifiers::default();
    }

    fn quick_key(
        &self,
        id: &'static str,
        label: &'static str,
        key: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Button::new(id, label)
            .style(ButtonStyle::Filled)
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.press_key(key);
                cx.notify();
            }))
    }

    fn modifier_key(
        &self,
        id: &'static str,
        label: &'static str,
        is_set: bool,
        toggle: fn(&mut Modifiers),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Button::new(id, label)
            .style(if is_set {
                ButtonStyle::Tinted(ui::TintColor::Accent)
            } else {
                ButtonStyle::Filled
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                toggle(&mut this.sticky_modifiers);
                cx.notify();
            }))
    }
}

impl Render for CompanionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let display = platform::external_display();
        let display_text = display.as_ref().map_or_else(
            || "No monitor".to_string(),
            |info| {
                format!(
                    "{}×{} @ {} Hz",
                    info.width_pixels, info.height_pixels, info.maximum_frames_per_second
                )
            },
        );
        let on_monitor = platform::editor_on_external_display();
        let dimmed = platform::phone_dimmed();
        let entity = cx.entity();
        let insets = window.insets().safe_area;

        v_flex()
            .size_full()
            .pt(insets.top + px(8.))
            .pb(insets.bottom + px(8.))
            .px_3()
            .gap_2()
            .bg(cx.theme().colors().background)
            .child(
                h_flex()
                    .justify_between()
                    .child(Headline::new("Zed is on your monitor").size(HeadlineSize::Small))
                    .child(Label::new(display_text).color(Color::Muted)),
            )
            .child(
                Label::new(format!(
                    "JIT {} · {} free · thermal {}",
                    if self.snapshot.jit.enabled() { "on" } else { "off" },
                    self.snapshot
                        .available
                        .map_or_else(|| "?".to_string(), ios_bridge::memory::format_bytes),
                    self.snapshot.thermal.label()
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            // Trackpad: drag moves the monitor's pointer, tap clicks,
            // two-finger pans scroll.
            .child(
                div()
                    .id("trackpad")
                    .flex_1()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().surface_background)
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds: Bounds<gpui::Pixels>, _, window, _| {
                                let entity = entity.clone();
                                window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                                    if phase != DispatchPhase::Bubble
                                        || !bounds.contains(&event.start_position)
                                    {
                                        return;
                                    }
                                    if event.phase == TouchPhase::Started {
                                        window.prevent_default();
                                    }
                                    entity.update(cx, |this, cx| {
                                        let previous = this.last_drag.replace(event.position);
                                        if let Some(previous) = previous {
                                            this.move_pointer(event.position - previous, cx);
                                        }
                                        if matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                                            this.last_drag = None;
                                        }
                                    });
                                });
                            },
                        )
                        .size_full(),
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, _cx| {
                        this.click(MouseButton::Left);
                    }))
                    .on_scroll(cx.listener(|this, event: &ScrollEvent, _window, _cx| {
                        platform::inject_input(PlatformInput::Scroll(ScrollEvent {
                            position: this.pointer,
                            source: ScrollSource::Pointer,
                            ..event.clone()
                        }));
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(self.quick_key("esc", "Esc", "escape", cx))
                    .child(self.quick_key("tab", "Tab", "tab", cx))
                    .child(self.modifier_key(
                        "ctrl",
                        "Ctrl",
                        self.sticky_modifiers.control,
                        |modifiers| modifiers.control = !modifiers.control,
                        cx,
                    ))
                    .child(self.modifier_key(
                        "cmd",
                        "Cmd",
                        self.sticky_modifiers.platform,
                        |modifiers| modifiers.platform = !modifiers.platform,
                        cx,
                    ))
                    .child(self.quick_key("left", "←", "left", cx))
                    .child(self.quick_key("up", "↑", "up", cx))
                    .child(self.quick_key("down", "↓", "down", cx))
                    .child(self.quick_key("right", "→", "right", cx))
                    .child(self.quick_key("palette", "Commands", "cmd-shift-p", cx))
                    .child(
                        Button::new("right-click", "Right-click")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, _cx| {
                                this.click(MouseButton::Right);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Show editor on:"))
                    .child(
                        ToggleButtonGroup::single_row(
                            "editor-location",
                            [
                                ToggleButtonSimple::new("Monitor", |_, _, _| {
                                    platform::set_external_display_mode(ExternalDisplayMode::Auto)
                                }),
                                ToggleButtonSimple::new("Phone", |_, _, _| {
                                    platform::set_external_display_mode(ExternalDisplayMode::Off)
                                }),
                            ],
                        )
                        .selected_index(if on_monitor { 0 } else { 1 }),
                    )
                    .child(
                        Button::new("dim", if dimmed { "Undim phone" } else { "Dim phone screen" })
                            .on_click(cx.listener(move |_, _: &ClickEvent, _window, cx| {
                                platform::set_phone_dimmed(!dimmed);
                                cx.notify();
                            })),
                    ),
            )
    }
}
