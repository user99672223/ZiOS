# ZiOS — Zed for iPhone

A private, sideloaded port of the [Zed](https://zed.dev) editor that runs fully
on-device on an iPhone 15. See `CLAUDE.md` for the plan and
[`docs/DECISIONS.md`](docs/DECISIONS.md) for every deviation from it.

## Layout

| Path | Contents |
|---|---|
| `zed/` | Zed v1.26.2 (git subtree), modified in place |
| `crates/ios_bridge` | Container paths, logging, memory accounting, strict Simulator mode |
| `crates/inproc` | In-process replacement for child processes |
| `crates/lanmouse_rx` | lan-mouse receiver |
| `crates/zios_app` | App entry point |
| `ios/App` | Info.plist and entitlements |
| `ios/Strict` | Strict-mode interposer (Simulator only) |
| `scripts/` | `build_app.sh` (app/ipa), `sim_test.sh` (Simulator test pass) |

## Building

On a Mac with Xcode and Rust:

```sh
scripts/build_app.sh sim                     # build/sim/ZiOS.app, strict mode on
scripts/sim_test.sh build/sim/ZiOS.app out   # screenshots and logs in out/
scripts/build_app.sh device                  # build/device/ZiOS.ipa + dSYM
```

CI (`.github/workflows/build.yml`) does all three on every push and uploads the
unsigned `.ipa`, the dSYM, Simulator screenshots, and logs as artifacts. Install
the `.ipa` with SideStore.
