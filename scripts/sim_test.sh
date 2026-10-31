#!/usr/bin/env bash
# Runs the strict Simulator test pass (CLAUDE.md sections 12 and 12a).
#
#   scripts/sim_test.sh <path/to/ZiOS.app> <output directory> [zed|gpui]
#
# `zed` (default) drives the full editor; `gpui` drives the GPUI smoke app
# (crates/zios_app).
#
# Every scenario runs twice: with ZED_IOS_FORCE_JIT=off and =on. Screenshots,
# logs, and console output land in the output directory.
set -euo pipefail

app="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
output="$2"
scenario="${3:-zed}"
mkdir -p "$output"
output="$(cd "$output" && pwd)"
bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Info.plist")"
failures=()

log() { printf '\n==> %s\n' "$*"; }

# with_timeout <seconds> <command...>: macOS has no `timeout`; a hung simctl
# call (for example, an app that never finishes launching) must not stall CI.
with_timeout() {
    local seconds="$1"
    shift
    "$@" &
    local pid=$!
    ( sleep "$seconds" && kill -TERM "$pid" 2>/dev/null && echo "timed out after ${seconds}s: $*" >&2 ) &
    local watchdog=$!
    local status=0
    wait "$pid" || status=$?
    kill "$watchdog" 2>/dev/null || true
    wait "$watchdog" 2>/dev/null || true
    return "$status"
}
fail() { echo "FAIL: $*" >&2; failures+=("$*"); }

# iPhone 15 when the runtime still offers it; otherwise the closest current iPhone.
pick_device_type() {
    xcrun simctl list devicetypes -j | python3 -c '
import json, sys
types = [t for t in json.load(sys.stdin)["devicetypes"] if t["name"].startswith("iPhone")]
names = [t["name"] for t in types]
for preferred in ("iPhone 15", "iPhone 16", "iPhone 17"):
    if preferred in names:
        print(next(t["identifier"] for t in types if t["name"] == preferred)); break
else:
    print(types[-1]["identifier"])'
}
pick_runtime() {
    xcrun simctl list runtimes -j | python3 -c '
import json, sys
runtimes = [r for r in json.load(sys.stdin)["runtimes"] if r["platform"] == "iOS" and r["isAvailable"]]
runtimes.sort(key=lambda r: [int(p) for p in r["version"].split(".")])
print(runtimes[-1]["identifier"])'
}

device_type="$(pick_device_type)"
runtime="$(pick_runtime)"
log "creating simulator ($device_type, $runtime)"
device="$(xcrun simctl create zios-ci "$device_type" "$runtime")"
cleanup() {
    xcrun simctl shutdown "$device" >/dev/null 2>&1 || true
    xcrun simctl delete "$device" >/dev/null 2>&1 || true
}
trap cleanup EXIT
xcrun simctl boot "$device"
xcrun simctl bootstatus "$device" -b > /dev/null
# Fixed status bar so screenshots are comparable between runs.
xcrun simctl status_bar "$device" override --time 9:41 --batteryState charged --batteryLevel 100 \
    --cellularBars 4 --wifiBars 3 || true
xcrun simctl install "$device" "$app"
container="$(xcrun simctl get_app_container "$device" "$bundle_id" data)"
logs="$container/Library/Logs/Zed"
echo "data container: $container"

wait_for_mark() {
    local mark="$1" timeout="${2:-90}"
    for _ in $(seq "$timeout"); do
        if [[ -f "$logs/ci-marks.txt" ]] && grep -qx "$mark" "$logs/ci-marks.txt"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# launch <name> <script> [ENV=VALUE ...]: starts the app and prints its pid.
launch() {
    local name="$1" script="$2"
    shift 2
    rm -rf "$logs/ci-marks.txt" "$logs/strict-violation.txt"
    local environment=(SIMCTL_CHILD_ZIOS_CI_SCRIPT="$script" SIMCTL_CHILD_ZED_LOG=info)
    for assignment in "$@"; do environment+=("SIMCTL_CHILD_$assignment"); done
    (
        export "${environment[@]}"
        with_timeout 120 xcrun simctl launch --terminate-running-process \
            --stdout="$output/$name.stdout.txt" --stderr="$output/$name.stderr.txt" \
            "$device" "$bundle_id"
    ) | awk '{print $NF}'
}

save_logs() {
    local name="$1"
    mkdir -p "$output/logs/$name"
    cp -R "$logs/." "$output/logs/$name/" 2>/dev/null || true
}

screenshot() {
    with_timeout 60 xcrun simctl io "$device" screenshot --type=png "$output/$1.png" > /dev/null
    echo "screenshot $1.png"
}

is_running() { kill -0 "$1" 2>/dev/null; }

gpui_scenario() {
    for jit in off on; do
        if [[ "$jit" == off ]]; then cap=3072; else cap=4096; fi
        log "pass: ZED_IOS_FORCE_JIT=$jit, ZED_IOS_MEMORY_CAP_MB=$cap"
        pid="$(launch "gpui-jit-$jit" "wait:4000;type:hello zed;key:cmd-s;wait:500;mark:main;memory_warning;wait:2000;mark:after_memory_warning" \
            ZED_IOS_FORCE_JIT="$jit" ZED_IOS_MEMORY_CAP_MB="$cap")"
        if wait_for_mark main; then
            screenshot "gpui-jit-$jit"
        else
            fail "jit=$jit: app never reached the main mark"
        fi
        if wait_for_mark after_memory_warning 30; then
            is_running "$pid" || fail "jit=$jit: app died after a memory warning"
            grep -q "memory warning (System)" "$logs/Zed.log" ||
                fail "jit=$jit: UIKit memory warning did not reach the app"
        else
            fail "jit=$jit: app did not survive the memory warning"
        fi
        grep -q "interposer loaded: true" "$logs/Zed.log" || fail "jit=$jit: strict interposer not loaded"
        grep -q "key down: cmd-s" "$logs/Zed.log" || fail "jit=$jit: keystrokes did not reach the view"
        grep -q "force_jit: $(tr '[:lower:]' '[:upper:]' <<< "${jit:0:1}")${jit:1}" "$logs/Zed.log" ||
            fail "jit=$jit: ZED_IOS_FORCE_JIT was not applied"
        save_logs "gpui-jit-$jit"
    done
}

zed_scenario() {
    documents="$container/Documents"
    edited_file="$documents/demo/src/main.rs"
    edit_marker="// edited on iPhone"

    for jit in off on; do
        if [[ "$jit" == off ]]; then cap=3072; else cap=4096; fi
        log "pass: ZED_IOS_FORCE_JIT=$jit, ZED_IOS_MEMORY_CAP_MB=$cap"

        # Zed launches, opens a folder and a file, edits and saves it, and survives a
        # real UIKit memory warning.
        script="wait:8000;mark:launched;seed_project;open:demo;wait:6000;mark:workspace"
        script+=";open:demo/src/main.rs;wait:4000;type:$edit_marker;key:enter;key:cmd-s;wait:3000;mark:edited"
        script+=";action:zed_ios::OpenDeviceStatus;wait:2500;mark:status;action:menu::Cancel"
        script+=";memory_warning;wait:3000;mark:after_memory_warning"
        pid="$(launch "main-jit-$jit" "$script" ZED_IOS_FORCE_JIT="$jit" ZED_IOS_MEMORY_CAP_MB="$cap")"
        for mark in launched workspace edited status; do
            if wait_for_mark "$mark" 120; then
                screenshot "$mark-jit-$jit"
            else
                fail "jit=$jit: app never reached the $mark mark"
                break
            fi
        done
        if grep -q "^failed:" "$logs/ci-marks.txt" 2>/dev/null; then
            fail "jit=$jit: CI steps failed: $(grep '^failed:' "$logs/ci-marks.txt" | tr '\n' ' ')"
        fi
        if head -1 "$edited_file" 2>/dev/null | grep -qF "$edit_marker"; then
            echo "edit saved to disk: $(head -1 "$edited_file")"
        else
            fail "jit=$jit: the edit was not saved ($(head -1 "$edited_file" 2>/dev/null))"
        fi
        if wait_for_mark after_memory_warning 30; then
            is_running "$pid" || fail "jit=$jit: app died after a memory warning"
            grep -q "memory warning (System)" "$logs/Zed.log" ||
                fail "jit=$jit: UIKit memory warning did not reach the app"
        else
            fail "jit=$jit: app did not survive the memory warning"
        fi
        grep -q "interposer loaded: true" "$logs/Zed.log" || fail "jit=$jit: strict interposer not loaded"
        grep -q "force_jit: $(tr '[:lower:]' '[:upper:]' <<< "${jit:0:1}")${jit:1}" "$logs/Zed.log" ||
            fail "jit=$jit: ZED_IOS_FORCE_JIT was not applied"
        save_logs "main-jit-$jit"
    done

    log "thermal and Low Power Mode overrides reach the status panel"
    pid="$(launch thermal "wait:9000;action:zed_ios::OpenDeviceStatus;wait:2500;mark:status_thermal" \
        ZED_IOS_THERMAL_STATE=serious ZED_IOS_LOW_POWER_MODE=1)"
    if wait_for_mark status_thermal 120; then
        screenshot status-thermal-serious
    else
        fail "thermal: status panel never opened"
    fi
    save_logs thermal

    log "session restore: relaunch reopens the previous workspace"
    pid="$(launch restore "wait:10000;mark:restored")"
    if wait_for_mark restored 120; then
        screenshot restored
        is_running "$pid" || fail "restore: app is not running"
    else
        fail "restore: app never reached the restored mark"
    fi
    save_logs restore
}

case "$scenario" in
    zed) zed_scenario ;;
    gpui) gpui_scenario ;;
    *) echo "unknown scenario $scenario" >&2; exit 2 ;;
esac

log "strict mode: process spawning must abort the app"
pid="$(launch spawn-test "wait:2000;spawn_test")"
for _ in $(seq 30); do is_running "$pid" || break; sleep 1; done
if is_running "$pid"; then
    fail "spawn test: app is still running after spawning a process"
elif [[ -f "$logs/ci-marks.txt" ]] && grep -qx spawn_not_blocked "$logs/ci-marks.txt"; then
    fail "spawn test: posix_spawn was not blocked"
elif ! grep -q "ZIOS STRICT MODE VIOLATION: posix_spawn" "$logs/strict-violation.txt" 2>/dev/null; then
    fail "spawn test: no violation report"
else
    echo "spawn blocked as expected:"
    head -3 "$logs/strict-violation.txt"
fi
save_logs spawn-test

log "strict mode: crossing the memory cap must kill the app (simulated jetsam)"
pid="$(launch jetsam-test "wait:60000" ZED_IOS_MEMORY_CAP_MB=24)"
for _ in $(seq 30); do is_running "$pid" || break; sleep 1; done
if is_running "$pid"; then
    fail "jetsam test: app survived a 24 MB cap"
elif ! grep -q "simulated jetsam" "$logs/Zed.log"; then
    fail "jetsam test: no simulated jetsam message"
elif ! grep -q "simulated jetsam" "$logs/crashes/last-state.txt" 2>/dev/null; then
    fail "jetsam test: last state was not recorded"
else
    echo "simulated jetsam as expected"
fi
save_logs jetsam-test

if ((${#failures[@]})); then
    log "collecting system logs and crash reports"
    with_timeout 120 xcrun simctl spawn "$device" log show --last 10m --style compact \
        --predicate 'process == "ZiOS" OR eventMessage CONTAINS "dev.zios.app"' \
        > "$output/system-log.txt" 2>&1 || true
    tail -n 60 "$output/system-log.txt" || true
    mkdir -p "$output/crash-reports"
    cp ~/Library/Logs/DiagnosticReports/ZiOS* "$output/crash-reports/" 2>/dev/null || true
    for report in "$output"/crash-reports/*; do
        [[ -f "$report" ]] && head -n 80 "$report"
    done
    printf '\n%d failure(s):\n' "${#failures[@]}"
    printf '  - %s\n' "${failures[@]}"
    exit 1
fi
echo "all Simulator scenarios passed"
