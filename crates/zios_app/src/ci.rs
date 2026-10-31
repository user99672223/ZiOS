//! Scripted actions for CI runs in the Simulator.
//!
//! `ZIOS_CI_SCRIPT` holds `;`-separated steps, run in order on the main thread:
//!
//! - `wait:<milliseconds>`
//! - `mark:<name>`: appends `<name>` to `Library/Logs/Zed/ci-marks.txt`; CI waits
//!   for marks before taking screenshots.
//! - `memory_warning`: asks UIKit for a real memory warning, exercising the same
//!   path as the device.
//! - `spawn_test`: tries to spawn a process; strict mode must abort the app.
//! - `type:<text>`: dispatches each character as a keystroke through GPUI.
//! - `key:<keystroke>`: dispatches one keystroke, such as `cmd-s`.

use std::{io::Write as _, time::Duration};

use gpui::{AnyWindowHandle, App, AsyncApp, Keystroke};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Wait(Duration),
    Mark(String),
    MemoryWarning,
    SpawnTest,
    Type(String),
    Key(String),
}

pub fn parse(script: &str) -> Result<Vec<Step>, String> {
    script
        .split(';')
        .map(str::trim)
        .filter(|step| !step.is_empty())
        .map(|step| {
            let (name, argument) = step.split_once(':').unwrap_or((step, ""));
            match name {
                "wait" => argument
                    .parse()
                    .map(|milliseconds| Step::Wait(Duration::from_millis(milliseconds)))
                    .map_err(|error| format!("bad wait {argument:?}: {error}")),
                "mark" if !argument.is_empty() => Ok(Step::Mark(argument.to_string())),
                "memory_warning" => Ok(Step::MemoryWarning),
                "spawn_test" => Ok(Step::SpawnTest),
                "type" if !argument.is_empty() => Ok(Step::Type(argument.to_string())),
                "key" => Keystroke::parse(argument)
                    .map(|_| Step::Key(argument.to_string()))
                    .map_err(|error| format!("bad keystroke {argument:?}: {error}")),
                _ => Err(format!("unknown CI step {step:?}")),
            }
        })
        .collect()
}

pub fn start(window: Option<AnyWindowHandle>, cx: &mut App) {
    let Ok(script) = std::env::var("ZIOS_CI_SCRIPT") else {
        return;
    };
    let steps = match parse(&script) {
        Ok(steps) => steps,
        Err(error) => {
            log::error!("ZIOS_CI_SCRIPT: {error}");
            return;
        }
    };
    log::info!("running CI script: {steps:?}");
    cx.spawn(async move |cx| {
        for step in steps {
            match step {
                Step::Wait(duration) => cx.background_executor().timer(duration).await,
                Step::Mark(name) => mark(&name),
                Step::MemoryWarning => perform_system_memory_warning(),
                Step::SpawnTest => spawn_test(),
                Step::Type(text) => {
                    for character in text.chars() {
                        let keystroke = match character {
                            ' ' => "space".to_string(),
                            character => character.to_string(),
                        };
                        dispatch_keystroke(window, &keystroke, cx);
                    }
                }
                Step::Key(keystroke) => dispatch_keystroke(window, &keystroke, cx),
            }
        }
    })
    .detach();
}

fn dispatch_keystroke(window: Option<AnyWindowHandle>, keystroke: &str, cx: &mut AsyncApp) {
    let Some(window) = window else {
        log::error!("no window to type into");
        return;
    };
    let keystroke = match Keystroke::parse(keystroke) {
        Ok(keystroke) => keystroke.with_simulated_ime(),
        Err(error) => {
            log::error!("bad keystroke {keystroke:?}: {error}");
            return;
        }
    };
    let result = window.update(cx, |_, window, cx| {
        window.dispatch_keystroke(keystroke, cx);
    });
    if let Err(error) = result {
        log::error!("dispatching keystroke failed: {error:#}");
    }
}

fn mark(name: &str) {
    log::info!("ZIOS_CI_MARK {name}");
    zlog::flush();
    let path = ios_bridge::paths::logs().join("ci-marks.txt");
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{name}"));
    if let Err(error) = result {
        log::error!("failed to write {}: {error}", path.display());
    }
}

/// Triggers UIKit's own memory-warning delivery (what the Simulator's "Simulate
/// Memory Warning" menu does), so the app delegate path is exercised end to end.
fn perform_system_memory_warning() {
    use objc2::{MainThreadMarker, msg_send, sel};
    use objc2_ui_kit::UIApplication;

    let Some(main_thread) = MainThreadMarker::new() else {
        log::error!("memory_warning step must run on the main thread");
        return;
    };
    let application = UIApplication::sharedApplication(main_thread);
    let selector = sel!(_performMemoryWarning);
    let responds: bool = unsafe { msg_send![&*application, respondsToSelector: selector] };
    if responds {
        log::info!("requesting a UIKit memory warning");
        let () = unsafe { msg_send![&*application, _performMemoryWarning] };
    } else {
        log::warn!("UIKit memory warning unavailable; raising an app-level warning instead");
        ios_bridge::memory::raise_warning(ios_bridge::memory::WarningSource::Test);
    }
}

fn spawn_test() {
    log::info!("spawn_test: attempting to spawn /bin/echo");
    zlog::flush();
    #[allow(clippy::disallowed_methods, reason = "this deliberately tests that spawning fails")]
    let result = std::process::Command::new("/bin/echo").arg("spawned").status();
    // Reaching this line means nothing stopped the spawn.
    log::error!("ZIOS_CI_SPAWN_NOT_BLOCKED {result:?}");
    mark("spawn_not_blocked");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steps() {
        assert_eq!(
            parse("wait:250; mark:main ;memory_warning;spawn_test;type:hi;key:cmd-s").unwrap(),
            vec![
                Step::Wait(Duration::from_millis(250)),
                Step::Mark("main".into()),
                Step::MemoryWarning,
                Step::SpawnTest,
                Step::Type("hi".into()),
                Step::Key("cmd-s".into()),
            ]
        );
        assert!(parse("mark:").is_err());
        assert!(parse("explode").is_err());
    }
}
