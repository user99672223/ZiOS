use std::time::Duration;

use gpui::{App, Context, Window, WindowOptions, div, prelude::*, rgb};
use ios_bridge::{
    memory::{self, WarningSource},
    strict::StrictConfig,
};

pub fn run(strict: StrictConfig) {
    gpui_platform::application().run(move |cx: &mut App| {
        gpui_ios::ios::set_status_bar_style(gpui_ios::StatusBarContentStyle::Light);

        cx.on_memory_warning(|_cx| memory::raise_warning(WarningSource::System))
            .detach();

        let window = cx.open_window(WindowOptions::default(), |_, cx| {
            cx.new(|cx| StatusView::new(strict.clone(), cx))
        });
        match window {
            Ok(_) => log::info!("main window opened"),
            Err(error) => log::error!("failed to open main window: {error:#}"),
        }
        cx.activate(true);
        crate::ci::start(cx);
    });
}

struct StatusView {
    strict: StrictConfig,
    memory_warnings: usize,
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
        }
    }
}

impl Render for StatusView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let footprint = memory::footprint_bytes().map_or_else(|| "unknown".into(), memory::format_bytes);
        let cap = self
            .strict
            .memory_cap_mb
            .map_or_else(|| "none".into(), |cap| format!("{cap} MB"));
        let log_path = ios_bridge::logging::log_paths()
            .map(|paths| paths.current.display().to_string())
            .unwrap_or_else(|| "unavailable".into());
        let rows = [
            ("Strict mode", if ios_bridge::strict::ENABLED { "on".to_string() } else { "off".to_string() }),
            ("Forced JIT", format!("{:?}", self.strict.force_jit)),
            ("Memory cap", cap),
            ("Footprint", footprint),
            ("Memory warnings", self.memory_warnings.to_string()),
            ("Log", log_path),
        ];

        div()
            .id("status")
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap_3()
            .p_8()
            .bg(rgb(0x1e2128))
            .text_color(rgb(0xdce0e5))
            .child(div().text_3xl().child("Zed for iPhone"))
            .child(div().text_color(rgb(0x8b929e)).child("Phase 1: platform skeleton"))
            .children(rows.into_iter().map(|(label, value)| {
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().text_color(rgb(0x8b929e)).child(label))
                    .child(div().child(value))
            }))
    }
}
