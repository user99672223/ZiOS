//! Runs the lan-mouse receiver and feeds its events into the editor window.

use std::{cell::RefCell, rc::Rc, time::Instant};

use futures::StreamExt as _;
use gpui::{App, Global, Size};
use lanmouse_rx::{Config, DEFAULT_PORT, Output, Receiver, ServiceEvent, Status, Translator};

pub struct LanMouse {
    receiver: Option<Receiver>,
    error: Option<String>,
}

impl Global for LanMouse {}

impl LanMouse {
    pub fn status(cx: &App) -> Option<Result<Status, String>> {
        let lan_mouse = cx.try_global::<Self>()?;
        Some(match (&lan_mouse.receiver, &lan_mouse.error) {
            (Some(receiver), _) => Ok(receiver.status()),
            (None, Some(error)) => Err(error.clone()),
            (None, None) => Err("not started".to_string()),
        })
    }

    pub fn authorize(fingerprint: &str, cx: &App) {
        let Some(receiver) = cx.try_global::<Self>().and_then(|this| this.receiver.as_ref()) else {
            return;
        };
        if let Err(error) = receiver.authorize(fingerprint, "authorized on iPhone") {
            log::error!("lan-mouse: could not authorize {fingerprint}: {error:#}");
        }
    }
}

fn window_size(cx: &mut App) -> Option<Size<f64>> {
    let window = cx.windows().into_iter().next()?;
    window
        .update(cx, |_, window, _| {
            let size = window.viewport_size();
            Size {
                width: f64::from(f32::from(size.width)),
                height: f64::from(f32::from(size.height)),
            }
        })
        .ok()
}

pub fn init(cx: &mut App) {
    let directory = ios_bridge::paths::application_support().join("lan-mouse");
    let started = Receiver::start(Config {
        port: DEFAULT_PORT,
        certificate_path: directory.join("certificate.pem"),
        authorized_path: directory.join("authorized_fingerprints.json"),
    });
    let mut events = match started {
        Ok((receiver, events)) => {
            log::info!(
                "lan-mouse: this iPhone's fingerprint is {}",
                receiver.status().fingerprint
            );
            cx.set_global(LanMouse {
                receiver: Some(receiver),
                error: None,
            });
            events
        }
        Err(error) => {
            log::error!("lan-mouse: receiver failed to start: {error:#}");
            cx.set_global(LanMouse {
                receiver: None,
                error: Some(format!("{error:#}")),
            });
            return;
        }
    };

    let translator = Rc::new(RefCell::new(Translator::new(Size {
        width: 1.0,
        height: 1.0,
    })));
    cx.spawn(async move |cx| {
        while let Some(event) = events.next().await {
            let outputs = cx.update(|cx| {
                let mut translator = translator.borrow_mut();
                match event {
                    ServiceEvent::Entered { position, addr } => {
                        log::info!("lan-mouse: {addr} entered from {position}");
                        if let Some(size) = window_size(cx) {
                            translator.set_bounds(size);
                        }
                        translator.enter(position);
                        Vec::new()
                    }
                    ServiceEvent::Input { event, .. } => translator.translate(event, Instant::now()),
                    ServiceEvent::Left { .. } | ServiceEvent::Disconnected { .. } => {
                        translator.leave()
                    }
                    ServiceEvent::Connected { addr, .. } => {
                        log::info!("lan-mouse: {addr} connected");
                        Vec::new()
                    }
                    ServiceEvent::ConnectionAttempt { fingerprint } => {
                        log::info!("lan-mouse: authorization requested by {fingerprint}");
                        Vec::new()
                    }
                }
            });
            for output in outputs {
                match output {
                    Output::Input(input) => {
                        #[cfg(target_os = "ios")]
                        gpui_ios::ios::inject_input(input);
                        #[cfg(not(target_os = "ios"))]
                        drop(input);
                    }
                    Output::Release => cx.update(|cx| {
                        if let Some(receiver) =
                            cx.try_global::<LanMouse>().and_then(|this| this.receiver.as_ref())
                        {
                            receiver.release();
                        }
                    }),
                }
            }
        }
    })
    .detach();
}
