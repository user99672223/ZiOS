//! Scripted actions for CI runs in the Simulator.
//!
//! `ZIOS_CI_SCRIPT` holds `;`-separated steps, run in order on the main thread:
//!
//! - `wait:<milliseconds>`
//! - `mark:<name>`: appends `<name>` to `Library/Logs/Zed/ci-marks.txt`; CI waits
//!   for marks before taking screenshots.
//! - `seed_project`: writes a small sample project to `Documents/demo`.
//! - `open:<path>`: opens a folder or file, relative to `Documents`.
//! - `type:<text>`: dispatches each character as a keystroke.
//! - `key:<keystroke>`: dispatches one keystroke, such as `cmd-s`.
//! - `action:<name>`: dispatches an action by name, such as
//!   `zed_ios::OpenDeviceStatus`.
//! - `memory_warning`: asks UIKit for a real memory warning.
//! - `spawn_test`: tries to spawn a process; strict mode must abort the app.

use std::{io::Write as _, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp, Keystroke};
use workspace::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Wait(Duration),
    Mark(String),
    SeedProject,
    Open(String),
    Type(String),
    Key(String),
    Action(String),
    MemoryWarning,
    SpawnTest,
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
                "seed_project" => Ok(Step::SeedProject),
                "open" if !argument.is_empty() => Ok(Step::Open(argument.to_string())),
                "type" if !argument.is_empty() => Ok(Step::Type(argument.to_string())),
                "key" => Keystroke::parse(argument)
                    .map(|_| Step::Key(argument.to_string()))
                    .map_err(|error| format!("bad keystroke {argument:?}: {error}")),
                "action" if !argument.is_empty() => Ok(Step::Action(argument.to_string())),
                "memory_warning" => Ok(Step::MemoryWarning),
                "spawn_test" => Ok(Step::SpawnTest),
                _ => Err(format!("unknown CI step {step:?}")),
            }
        })
        .collect()
}

pub fn start(app_state: Arc<AppState>, cx: &mut App) {
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
            let result = run_step(&step, &app_state, cx).await;
            if let Err(error) = result {
                log::error!("CI step {step:?} failed: {error:#}");
                mark(&format!("failed:{step:?}"));
            }
        }
    })
    .detach();
}

async fn run_step(step: &Step, app_state: &Arc<AppState>, cx: &mut AsyncApp) -> Result<()> {
    match step {
        Step::Wait(duration) => cx.background_executor().timer(*duration).await,
        Step::Mark(name) => mark(name),
        Step::SeedProject => seed_project()?,
        Step::Open(path) => {
            let path = ios_bridge::paths::documents().join(path);
            let task = cx.update(|cx| {
                // iOS shows one window; open into it instead of creating another.
                let requesting_window = cx
                    .windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<workspace::MultiWorkspace>());
                workspace::open_paths(
                    &[path.clone()],
                    app_state.clone(),
                    workspace::OpenOptions {
                        requesting_window,
                        ..Default::default()
                    },
                    cx,
                )
            });
            task.await
                .with_context(|| format!("opening {}", path.display()))?;
        }
        Step::Type(text) => {
            for character in text.chars() {
                let keystroke = match character {
                    ' ' => "space".to_string(),
                    '\n' => "enter".to_string(),
                    character => character.to_string(),
                };
                dispatch_keystroke(&keystroke, cx)?;
            }
        }
        Step::Key(keystroke) => dispatch_keystroke(keystroke, cx)?,
        Step::Action(name) => {
            let action = cx.update(|cx| cx.build_action(name, None))?;
            let window = cx
                .update(|cx| cx.windows().into_iter().next())
                .context("no window for the action")?;
            window.update(cx, |_, window, cx| window.dispatch_action(action, cx))?;
        }
        Step::MemoryWarning => perform_system_memory_warning(),
        Step::SpawnTest => spawn_test(),
    }
    Ok(())
}

fn dispatch_keystroke(keystroke: &str, cx: &mut AsyncApp) -> Result<()> {
    let keystroke = Keystroke::parse(keystroke)?.with_simulated_ime();
    // iOS has a single window.
    let window = cx
        .update(|cx| cx.windows().into_iter().next())
        .context("no window to type into")?;
    window.update(cx, |_, window, cx| {
        window.dispatch_keystroke(keystroke, cx);
    })
}

fn seed_project() -> Result<()> {
    let root: PathBuf = ios_bridge::paths::documents().join("demo");
    std::fs::create_dir_all(root.join("src"))?;
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        (
            "src/main.rs",
            "fn main() {\n    let greeting = greet(\"iPhone\");\n    println!(\"{greeting}\");\n}\n\nfn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n",
        ),
        (
            "README.md",
            "# Demo\n\nA sample project created by the ZiOS CI script.\n",
        ),
        ("notes.txt", ""),
    ];
    for (path, contents) in files {
        std::fs::write(root.join(path), contents)
            .with_context(|| format!("writing {path}"))?;
    }
    log::info!("seeded {}", root.display());
    Ok(())
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
#[cfg(target_os = "ios")]
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

#[cfg(not(target_os = "ios"))]
fn perform_system_memory_warning() {
    ios_bridge::memory::raise_warning(ios_bridge::memory::WarningSource::Test);
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
            parse("wait:250; mark:main ;seed_project;open:demo;type:hi;key:cmd-s;action:a::B;memory_warning;spawn_test;")
                .unwrap(),
            vec![
                Step::Wait(Duration::from_millis(250)),
                Step::Mark("main".into()),
                Step::SeedProject,
                Step::Open("demo".into()),
                Step::Type("hi".into()),
                Step::Key("cmd-s".into()),
                Step::Action("a::B".into()),
                Step::MemoryWarning,
                Step::SpawnTest,
            ]
        );
        assert!(parse("mark:").is_err());
        assert!(parse("open:").is_err());
        assert!(parse("explode").is_err());
    }
}
