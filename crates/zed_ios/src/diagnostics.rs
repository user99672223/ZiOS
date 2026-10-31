//! Device diagnostics (CLAUDE.md section 12b): Copy Diagnostics Summary,
//! Export Logs, and Run Device Self-Test, as command palette actions.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use gpui::{App, AppContext as _, ClipboardItem, actions};
use ios_bridge::{memory, paths, status::Snapshot};
use workspace::Workspace;

actions!(
    zed_ios,
    [
        /// Copies a short device status and error summary to the clipboard.
        CopyDiagnosticsSummary,
        /// Collects logs, crash reports, and a status snapshot into a folder in
        /// Documents and opens the share sheet.
        ExportLogs,
        /// Checks JIT, memory, display, terminal commands, and in-process tools.
        RunDeviceSelfTest,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &CopyDiagnosticsSummary, _, cx| {
            let summary = summary();
            cx.write_to_clipboard(ClipboardItem::new_string(summary));
            log::info!("diagnostics summary copied");
        });
        workspace.register_action(|_, _: &ExportLogs, _, cx| {
            let result = export_logs(cx);
            match result {
                Ok(folder) => {
                    log::info!("exported diagnostics to {}", folder.display());
                    #[cfg(target_os = "ios")]
                    share(&folder);
                }
                Err(error) => log::error!("export failed: {error:#}"),
            }
        });
        workspace.register_action(|_, _: &RunDeviceSelfTest, _, cx| {
            cx.background_spawn(async {
                let report = self_test();
                log::info!("self-test:\n{report}");
                if let Err(error) = std::fs::write(paths::logs().join("self-test.txt"), &report) {
                    log::error!("could not save self-test: {error}");
                }
            })
            .detach();
        });
    })
    .detach();
}

fn build_commit() -> String {
    #[cfg(target_os = "ios")]
    {
        use objc2::{msg_send, rc::Retained, runtime::AnyObject};
        use objc2_foundation::{NSBundle, NSString};
        let bundle = NSBundle::mainBundle();
        let key = NSString::from_str("ZiOSGitCommit");
        let value: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*bundle, objectForInfoDictionaryKey: &*key] };
        if let Some(value) = value.and_then(|value| value.downcast::<NSString>().ok()) {
            return value.to_string();
        }
    }
    "unknown".to_string()
}

fn last_error_lines(count: usize) -> Vec<String> {
    let log = std::fs::read_to_string(paths::logs().join("Zed.log")).unwrap_or_default();
    let errors: Vec<String> = log
        .lines()
        .filter(|line| line.contains(" ERROR ") || line.contains(" WARN "))
        .map(str::to_string)
        .collect();
    errors[errors.len().saturating_sub(count)..].to_vec()
}

fn last_crash() -> Option<(PathBuf, String)> {
    let mut crashes: Vec<PathBuf> = std::fs::read_dir(paths::crashes())
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    crashes.sort();
    let last = crashes.pop()?;
    let contents = std::fs::read_to_string(&last).ok()?;
    Some((last, contents.chars().take(4000).collect()))
}

/// Sized to paste into a chat.
pub fn summary() -> String {
    let mut text = String::new();
    writeln!(text, "ZiOS build {}", build_commit()).ok();
    text.push_str(&Snapshot::collect().to_text());
    writeln!(text, "\nLast errors:").ok();
    for line in last_error_lines(50) {
        writeln!(text, "{line}").ok();
    }
    if let Some((path, contents)) = last_crash() {
        writeln!(text, "\nLast crash ({}):\n{contents}", path.display()).ok();
    }
    text
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn export_logs(_cx: &mut App) -> anyhow::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let folder = paths::documents().join(format!("ZiOS-Diagnostics-{stamp}"));
    std::fs::create_dir_all(&folder)?;
    zlog::flush();
    copy_tree(&paths::logs(), &folder.join("logs"))?;
    std::fs::write(folder.join("summary.txt"), summary())?;
    std::fs::write(folder.join("self-test.txt"), self_test())?;
    Ok(folder)
}

#[cfg(target_os = "ios")]
fn share(folder: &Path) {
    use objc2::{MainThreadMarker, msg_send, rc::{Allocated, Retained}, runtime::AnyObject};
    use objc2_foundation::{NSArray, NSString, NSURL};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&folder.to_string_lossy()));
    let items = NSArray::from_retained_slice(&[url]);
    unsafe {
        let Some(class) = objc2::runtime::AnyClass::get(c"UIActivityViewController") else {
            return;
        };
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        let controller: Option<Retained<AnyObject>> = msg_send![
            allocated,
            initWithActivityItems: &*items,
            applicationActivities: std::ptr::null::<AnyObject>()
        ];
        let Some(controller) = controller else {
            return;
        };
        let application = objc2_ui_kit::UIApplication::sharedApplication(main_thread);
        #[allow(deprecated)]
        let Some(window) = application.keyWindow() else {
            return;
        };
        let Some(mut presenter) = window.rootViewController() else {
            return;
        };
        while let Some(presented) = presenter.presentedViewController() {
            presenter = presented;
        }
        // iPad needs an anchor for the popover; harmless on iPhone.
        let popover: Option<Retained<AnyObject>> =
            msg_send![&*controller, popoverPresentationController];
        if let Some(popover) = popover {
            let view = presenter.view();
            let () = msg_send![&*popover, setSourceView: view.as_deref()];
        }
        let () = msg_send![&*presenter, presentViewController: &*controller, animated: true, completion: std::ptr::null::<AnyObject>()];
    }
}

fn check(report: &mut String, name: &str, result: Result<String, String>) {
    match result {
        Ok(detail) => writeln!(report, "PASS  {name}: {detail}"),
        Err(detail) => writeln!(report, "FAIL  {name}: {detail}"),
    }
    .ok();
}

/// Runs on a background thread.
pub fn self_test() -> String {
    let mut report = String::new();
    let snapshot = Snapshot::collect();

    check(&mut report, "JIT detection", Ok(format!(
        "{} via {}",
        if snapshot.jit.enabled() { "enabled" } else { "not enabled" },
        snapshot.jit.method.label()
    )));
    check(
        &mut report,
        "memory headroom",
        match (snapshot.available, snapshot.footprint) {
            (Some(available), Some(footprint)) => Ok(format!(
                "{} available, {} used, limit ≈ {}",
                memory::format_bytes(available),
                memory::format_bytes(footprint),
                memory::format_bytes(available + footprint)
            )),
            (None, Some(footprint)) => Ok(format!(
                "{} used (no jetsam limit reported here)",
                memory::format_bytes(footprint)
            )),
            _ => Err("footprint unavailable".into()),
        },
    );
    #[cfg(target_os = "ios")]
    check(
        &mut report,
        "display detection",
        Ok(match gpui_ios::ios::external_display() {
            Some(info) => format!("monitor {}×{}", info.width_pixels, info.height_pixels),
            None => "phone only".into(),
        }),
    );
    for (name, command) in [
        ("terminal echo", "echo zios-self-test"),
        ("git (lg2)", "lg2 --version"),
        ("python3", "python3 -c print(1)"),
        ("node", "node -e 1"),
    ] {
        check(&mut report, name, run_command(command));
    }
    for server in ["rust-analyzer", "ruff", "biome", "taplo", "vtsls", "clangd", "gopls"] {
        check(
            &mut report,
            server,
            if inproc::is_registered(std::ffi::OsStr::new(server)) {
                Ok("bundled".into())
            } else {
                Err("not bundled in this build".into())
            },
        );
    }
    report
}

fn run_command(command_line: &str) -> Result<String, String> {
    let mut words = command_line.split_whitespace();
    let program = words.next().ok_or("empty command")?;
    let mut request = inproc::Request::new(program).args(words);
    request.cwd = paths::documents();
    let mut child = inproc::spawn(request).map_err(|error| error.to_string())?;
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read as _;
        stdout.read_to_string(&mut output).ok();
    }
    let status = child.wait();
    if status.success() {
        Ok(output.trim().chars().take(80).collect())
    } else {
        Err(format!("exit {:?}: {}", status.code(), output.trim()))
    }
}
