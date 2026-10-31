# Decisions and deviations

Every place where ZiOS departs from `CLAUDE.md`, or where `CLAUDE.md` turned out to
be wrong about a third-party codebase, with the reason. Newest phase last.

## Pinned versions

| Component | Version / commit | Notes |
|---|---|---|
| Zed | `v1.26.2` (`c4273271092b0287f7d181f616bd5171e0a2f70c`) | Latest stable tag on 2026-10-31; `v1.27.0-pre` is preview. Imported with `git subtree --squash` at `/zed`. |
| Rust | `1.98.1` | Pinned by `zed/rust-toolchain.toml`; CI installs exactly this. |
| iOS deployment target | 17.0 | Lowest iOS the iPhone 15 shipped with. |
| oslog crate | `=0.2.0` | `os_log` sink for zlog. |
| actions/checkout | v7.0.1 `3d3c42e5aac5ba805825da76410c181273ba90b1` | |
| actions/upload-artifact | v7.0.2 `cf430e030ddbb5b0abf93d22962f4752f3646cd9` | |
| Swatinem/rust-cache | v2.9.3 `d1527c8f67ef9af357290475d4af4b0fab8f12e0` | |
| mozilla-actions/sccache-action | v0.0.11 `fc920bf0ec8de6ee65d409111f7ec508035751ba` | |
| maxim-lobanov/setup-xcode | v1.7.0 `ed7a3b1fda3918c0306d1b724322adc0b8cc0a90` | Selects `latest-stable` Xcode on the runner. |

Rust crate versions are pinned by `zed/Cargo.lock`.

## Phase 1: CI skeleton

### Zed already contains an iOS GPUI platform (CLAUDE.md §2, §5)
`CLAUDE.md` says to add an iOS platform "next to GPUI's existing macOS platform
under `zed/crates/gpui/src/platform/`". In Zed v1.26.2 platforms are separate
crates: `crates/gpui_macos`, `crates/gpui_apple` (the shared Metal renderer,
Core Text text system, dispatcher, keychain, thermal), and an upstream
**`crates/gpui_ios`** built on objc2 (`objc2-ui-kit`, `objc2-quartz-core`,
`objc2-foundation`, Metal via `gpui_apple`). It already implements scenes, the
`CAMetalLayer` view, safe areas, touch, scrolling, `UITextInput` with marked text,
the software keyboard, memory warnings, thermal state, and a `CADisplayLink`.
**Decision:** reuse and extend `gpui_ios` instead of writing a new platform. This
is the "reuse, don't rewrite" rule applied to code that already exists upstream.

### Metal shader build already targets iOS
`gpui_apple/build.rs` already picks `iphoneos` / `iphonesimulator` from Cargo's
target environment, so no change was needed. The only change is a fallback that
writes a placeholder metallib when the *host* is not macOS, so the whole iOS
build can be type-checked on Linux (it cannot be linked there anyway).

### No Xcode project; the app bundle is assembled by a script
`CLAUDE.md` §3 asks for an Xcode project in `/ios/App` with a Swift/ObjC shell
(AppDelegate, SceneDelegates). Upstream `gpui_ios` already defines the app and
scene delegates in Rust (objc2 `define_class!`) and starts `UIApplicationMain`
from Rust, as winit does. A Swift shell would duplicate them. The executable is
therefore the Rust binary itself, and `scripts/build_app.sh` assembles
`ZiOS.app` (Info.plist, entitlements, frameworks, resources), splits the dSYM,
and zips `Payload/ZiOS.app` into the `.ipa`. `/ios/App` holds the Info.plist and
entitlements. Without `xcodebuild` there is no `CODE_SIGNING_ALLOWED=NO`; see
the next item.

### The ".ipa" is ad-hoc signed, not unsigned
A truly unsigned binary carries no entitlements, and SideStore reads the
entitlements it should request from the app's code signature. The device app is
therefore **ad-hoc signed** (`codesign -s -`) with `ZiOS.entitlements`
(`increased-memory-limit`, `get-task-allow`). An ad-hoc signature is not valid
for installation by itself; SideStore re-signs it with the user's certificate.

### Our crates join Zed's Cargo workspace
`/crates/*` are members of `zed/Cargo.toml`'s workspace (via `package.workspace =
"../../zed"`), so they share Zed's lockfile, dependency versions, and profiles.

### Logging uses Zed's zlog
Rather than a second logger, Zed's `zlog` is the only `log` backend. Two small
additions to `zlog::sink`: a configurable rotation size (10 MB, so current +
rotated ≈ 20 MB, matching §12b) and an extra sink that mirrors records to
`os_log`. Logs live in `Library/Logs/Zed/` inside the app container.

### Strict Simulator mode: interposer dylib
Process creation (`posix_spawn`, `posix_spawnp`, `fork`, `vfork`, `execve`,
`system`) and host file-system access are intercepted by
`ios/Strict/zios_strict.c`, a dylib with `__DATA,__interpose` tuples. It is built
by `crates/ios_bridge/build.rs` only when the `strict` feature is on **and** the
target is the Simulator, and is linked (not injected with
`DYLD_INSERT_LIBRARIES`) so strict mode applies however the app is launched.
`ios_bridge` has a `compile_error!` if strict mode is ever enabled for a device
target.

The sandbox check is a **deny list** of host locations (`/Users`, `/tmp`,
`/private/var/folders`, `/Volumes`, `/opt`, `/usr/local`, …) outside the simulated
device's data directory and user-picked roots, rather than an allow list, because
UIKit and Metal read many system paths that differ between Simulator runtimes.

### Simulated memory limits are estimates
Apple does not publish per-app limits. Public data points: developer reports of a
~6 GB ceiling on 8 GB and 12 GB Pro iPhones with the entitlement
([Apple forums 834805](https://developer.apple.com/forums/thread/834805)), and no
measured figure for the 6 GB iPhone 15. CI uses **3072 MB** without the
entitlement and **4096 MB** with it (about half and two thirds of RAM, consistent
with older 6 GB devices). These are estimates; the Device Self-Test reports
`os_proc_available_memory() + footprint` measured on the real phone so the
constants in `ios_bridge::strict` can be corrected.

### Simulator builds are signed without entitlements
The first CI run showed the Simulator refusing to launch an ad-hoc signed app
that claims `increased-memory-limit` ("request was denied by service delegate
(SBMainWorkspace)"). Simulator builds are signed ad hoc without entitlements;
device builds keep them.

## Phase 2: GPUI on iOS

### Hardware keyboard
Upstream `gpui_ios` had touch, scrolling and `UITextInput`, but no
`pressesBegan`/`pressesEnded` handling, and its hidden text view is only first
responder while the software keyboard is wanted. Added:

- `gpui_ios::keys`: a UIKit-independent `UIKey` → `Keystroke` conversion that
  follows `gpui_macos::events::parse_keystroke`, so Zed's **macOS keymap** works
  unchanged (`cmd-s`, `cmd-shift-p`, shift-symbol folding, option-composed
  characters, ASCII binding on non-ASCII layouts). iOS has no `UCKeyTranslate`,
  so the "command layout" fallback uses a US-layout table keyed by HID usage.
  Unit-tested on the CI runner.
- `presses*` overrides on the Metal view and the text input view. GPUI sees
  every key first (as on macOS). Unhandled keys that produce text go to UIKit's
  text system when the text view is first responder (IME, dead keys), otherwise
  they are inserted directly, as `gpui_linux` does.
- The Metal view keeps first-responder status whenever the text view does not,
  so shortcuts work with no text field focused.
- **Key repeat:** UIKit only repeats keys that reach its text system, so keys
  GPUI consumes (arrows, backspace, shortcuts) are repeated by GPUI, polled from
  the existing display-link frame callback (400 ms delay, 33 ms interval).
- Modifier keys produce `ModifiersChanged`; Caps Lock state is reported.

The Simulator cannot receive synthetic hardware-key events from CI, so CI
drives keystrokes through GPUI (`type:`/`key:` steps) and the UIKit mapping is
covered by unit tests; the real key path is checked on the device
(docs/DEVICE_TESTING.md).

### Clipboard
Upstream already maps GPUI's clipboard to `UIPasteboard` (text only).

## Phase 3: Zed boots

### Local iOS type-checking on Linux
`cargo check --target aarch64-apple-ios` of all of Zed runs on Linux with shims
(an empty-object C compiler, an archiver, and an `xcrun` that reports an empty
SDK) because `check` never links. This finds iOS compile errors in minutes
instead of a CI round trip. The shims are not part of the repository; CI builds
with the real Xcode toolchain.

### Crates changed for iOS (minimal, `cfg`-gated)
- `gpui`: `PlatformScreenCaptureFrame = ()` on iOS (no screen capture).
- `crashes`: the minidump sidecar re-executes Zed as a second process, which
  iOS forbids. Shared data types stay in `crashes.rs`, the desktop sidecar moves
  to `native.rs` unchanged, and `ios.rs` keeps the same API, storing session
  metadata in-process. Native crashes come from MetricKit, panics from
  `ios_bridge` (§12b).
- `fs`: `trash-rs` has no iOS backend; iOS trashes with
  `NSFileManager.trashItemAtURL` (files from Files-app providers land in
  Recently Deleted) and restores by moving back. Darwin syscalls already used on
  macOS (`fcntl(F_GETPATH)`, `renamex_np`) are enabled for iOS.
- `paths`: the macOS layout inside the container (`HOME` is the container):
  `Library/Application Support/Zed`, `Library/Logs/Zed`, `Library/Caches/Zed`.
  Config stays at `~/.config/zed` inside the container, as on macOS.
- `livekit_client`, `call`, `audio`: iOS uses the existing mock LiveKit backend
  (as FreeBSD and windows-gnu do), so calls/screen sharing are disabled and the
  large prebuilt libwebrtc is not linked. Text collaboration is unaffected.
- `client::telemetry`: reports "iOS" and the iOS version (NSProcessInfo).
- `settings::vscode_import`: reads the `osx` terminal keys on iOS.
- `languages`: Darwin asset names for the GitHub download helpers so the code
  compiles; downloads themselves are disabled on iOS (phase 8).
- `zed` (main): logging through `ios_bridge`; no single-instance socket; no
  login-shell environment probe (it starts a shell); no Node binary download;
  no auto-updater (updates come through SideStore); an iOS file-watcher
  initializer that logs instead of offering "troubleshoot and quit".

### `crates/zed_ios`
iOS glue that needs GPUI and the workspace (CI script runner now; status panel
and display companion later). Kept outside `/zed` so upstream merges stay small.
The CI runner opens folders into the existing window because iOS shows a single
window.
