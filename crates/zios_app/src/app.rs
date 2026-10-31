use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, Context, FocusHandle, KeyDownEvent, Window, WindowOptions, div,
    prelude::*, rgb,
};
use ios_bridge::{
    memory::{self, WarningSource},
    strict::StrictConfig,
};

pub fn run(strict: StrictConfig) {
    gpui_platform::application().run(move |cx: &mut App| {
        gpui_ios::ios::set_status_bar_style(gpui_ios::StatusBarContentStyle::Light);

        cx.on_memory_warning(|_cx| memory::raise_warning(WarningSource::System))
            .detach();

        let window = cx.open_window(WindowOptions::default(), |window, cx| {
            cx.new(|cx| {
                let view = StatusView::new(strict.clone(), cx);
                window.focus(&view.focus_handle, cx);
                view
            })
        });
        let window: Option<AnyWindowHandle> = match window {
            Ok(window) => {
                log::info!("main window opened");
                Some(window.into())
            }
            Err(error) => {
                log::error!("failed to open main window: {error:#}");
                None
            }
        };
        cx.activate(true);
        crate::ci::start(window, cx);
    });
}

struct StatusView {
    strict: StrictConfig,
    memory_warnings: usize,
    focus_handle: FocusHandle,
    last_keystroke: Option<String>,
    typed: String,
}

impl StatusView {
    fn new(strict: StrictConfig, cx: &mut Context<Self>) -> Self {
        // Footprint changes continuously; refresh once a second rather than every frame.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        let this = cx.weak_entity();
        cx.on_memory_warning(move |cx| {
            let result = this.update(cx, |this, cx| {
                this.memory_warnings += 1;
                cx.notify();
            });
            if let Err(error) = result {
                log::debug!("status view dropped before memory warning: {error}");
            }
        })
        .detach();
        Self {
            strict,
            memory_warnings: 0,
            focus_handle: cx.focus_handle(),
            last_keystroke: None,
            typed: String::new(),
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        log::info!("key down: {} (held: {})", keystroke.unparse(), event.is_held);
        self.last_keystroke = Some(keystroke.unparse());
        let modified = keystroke.modifiers.platform || keystroke.modifiers.control;
        match keystroke.key.as_str() {
            "backspace" if !modified => {
                self.typed.pop();
            }
            _ if !modified => {
                if let Some(key_char) = &keystroke.key_char {
                    self.typed.push_str(key_char);
                }
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for StatusView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let footprint =
            memory::footprint_bytes().map_or_else(|| "unknown".into(), memory::format_bytes);
        let cap = self
            .strict
            .memory_cap_mb
            .map_or_else(|| "none".into(), |cap| format!("{cap} MB"));
        let log_path = ios_bridge::logging::log_paths()
            .map(|paths| paths.current.display().to_string())
            .unwrap_or_else(|| "unavailable".into());
        let strict = if ios_bridge::strict::ENABLED { "on" } else { "off" };
        let rows = [
            ("Strict mode", strict.to_string()),
            ("Forced JIT", format!("{:?}", self.strict.force_jit)),
            ("Memory cap", cap),
            ("Footprint", footprint),
            ("Memory warnings", self.memory_warnings.to_string()),
            ("Log", log_path),
            (
                "Last key",
                self.last_keystroke
                    .clone()
                    .unwrap_or_else(|| "press a key on a hardware keyboard".into()),
            ),
        ];

        div()
            .id("status")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap_3()
            .p_8()
            .bg(rgb(0x1e2128))
            .text_color(rgb(0xdce0e5))
            .child(div().text_3xl().child("Zed for iPhone"))
            .child(
                div()
                    .text_color(rgb(0x8b929e))
                    .child("Phase 2: GPUI on iOS with hardware keyboard"),
            )
            .children(rows.into_iter().map(|(label, value)| {
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().text_color(rgb(0x8b929e)).child(label))
                    .child(div().child(value))
            }))
            .child(
                div()
                    .mt_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(0x464b57))
                    .bg(rgb(0x2f343e))
                    .child(format!("{}▏", self.typed)),
            )
    }
}
