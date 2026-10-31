#!/usr/bin/env bash
# Builds ZiOS.app for the Simulator or a device, without Xcode project files.
#
#   scripts/build_app.sh sim     [--profile dev]          -> build/sim/ZiOS.app
#   scripts/build_app.sh device  [--profile ios-release]  -> build/device/ZiOS.ipa (+ dSYM)
#
# Simulator builds always enable strict Simulator mode (CLAUDE.md section 12a).
# Device builds never do; ios_bridge refuses to compile strict mode for a device.
#
# Options:
#   --profile <name>     Cargo profile (default: dev for sim, ios-release for device)
#   --package <name>     Cargo package with the app binary (default: zios_app)
#   --bin <name>         binary target (default: ZiOS)
#   --features <list>    extra Cargo features
#   --no-strict          Simulator only: build without strict mode (debugging aid)
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
platform="${1:-}"
shift || true
case "$platform" in
    sim | device) ;;
    *)
        echo "usage: $0 sim|device [--profile name] [--package name] [--bin name] [--features list] [--no-strict]" >&2
        exit 2
        ;;
esac

profile=""
package="zios_app"
binary="ZiOS"
features=""
strict=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile) profile="$2"; shift 2 ;;
        --package) package="$2"; shift 2 ;;
        --bin) binary="$2"; shift 2 ;;
        --features) features="$2"; shift 2 ;;
        --no-strict) strict=0; shift ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done

export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}"
if [[ "$platform" == sim ]]; then
    target="aarch64-apple-ios-sim"
    sdk="iphonesimulator"
    plist_platform="iPhoneSimulator"
    profile="${profile:-dev}"
    if [[ "$strict" == 1 ]]; then
        features="${features:+$features,}strict"
    fi
else
    target="aarch64-apple-ios"
    sdk="iphoneos"
    plist_platform="iPhoneOS"
    profile="${profile:-ios-release}"
fi

output_directory="$repository_root/build/$platform"
app="$output_directory/ZiOS.app"
cargo_log="$output_directory/cargo.json"
rm -rf "$app" "$output_directory/Payload" "$output_directory/ZiOS.ipa" "$output_directory/ZiOS.app.dSYM"
mkdir -p "$app/Frameworks"

echo "==> cargo build -p $package --bin $binary --target $target --profile $profile ${features:+--features $features}"
(
    cd "$repository_root/zed"
    cargo build -p "$package" --bin "$binary" --target "$target" --profile "$profile" \
        ${features:+--features "$features"} \
        --message-format=json-render-diagnostics > "$cargo_log"
)

read -r executable strict_dylib_directory < <(python3 - "$cargo_log" "$binary" <<'PY'
import json, sys
executable, strict_directory = None, "-"
with open(sys.argv[1]) as log:
    for line in log:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        reason = event.get("reason")
        if reason == "compiler-artifact" and event.get("target", {}).get("name") == sys.argv[2] \
                and "bin" in event.get("target", {}).get("kind", []) and event.get("executable"):
            executable = event["executable"]
        # Package ids look like "path+file:///.../crates/ios_bridge#0.1.0".
        if reason == "build-script-executed" and "/crates/ios_bridge#" in event.get("package_id", ""):
            strict_directory = event["out_dir"]
if not executable:
    sys.exit("cargo did not report the app executable")
print(executable, strict_directory)
PY
)

cp "$executable" "$app/ZiOS"

git_commit="$(git -C "$repository_root" rev-parse HEAD 2>/dev/null || echo unknown)"
build_number="$(git -C "$repository_root" rev-list --count HEAD 2>/dev/null || echo 1)"
sdk_version="$(xcrun --sdk "$sdk" --show-sdk-version)"
sed -e "s/@VERSION@/0.1.0/" \
    -e "s/@BUILD_NUMBER@/$build_number/" \
    -e "s/@GIT_COMMIT@/$git_commit/" \
    -e "s/@PLATFORM@/$plist_platform/" \
    -e "s/@PLATFORM_LOWER@/$sdk/" \
    -e "s/@SDK_NAME@/$sdk$sdk_version/" \
    -e "s/@MINIMUM_OS@/$IPHONEOS_DEPLOYMENT_TARGET/" \
    "$repository_root/ios/App/Info.plist" > "$app/Info.plist"
plutil -lint "$app/Info.plist" > /dev/null
printf 'APPL????' > "$app/PkgInfo"

if [[ -d "$repository_root/resources/bundle" ]]; then
    cp -R "$repository_root/resources/bundle/." "$app/"
fi

if [[ "$platform" == sim && "$strict" == 1 ]]; then
    if [[ ! -f "$strict_dylib_directory/libzios_strict.dylib" ]]; then
        echo "strict interposer dylib missing from ios_bridge's OUT_DIR ($strict_dylib_directory)" >&2
        exit 1
    fi
    cp "$strict_dylib_directory/libzios_strict.dylib" "$app/Frameworks/"
fi
rmdir "$app/Frameworks" 2>/dev/null || true

entitlements="$repository_root/ios/App/ZiOS.entitlements"
if [[ "$platform" == device ]]; then
    echo "==> splitting debug info into ZiOS.app.dSYM"
    xcrun dsymutil "$app/ZiOS" -o "$output_directory/ZiOS.app.dSYM"
    xcrun strip -S -x "$app/ZiOS"
fi

# Sign nested code first, then the app. Device builds are ad-hoc signed only so
# the entitlements travel inside the binary; SideStore re-signs with the user's
# certificate on the phone.
if [[ -d "$app/Frameworks" ]]; then
    find "$app/Frameworks" \( -name '*.dylib' -o -name '*.framework' \) -maxdepth 1 -print0 |
        xargs -0 -I{} codesign --force --sign - --timestamp=none {}
fi
codesign --force --sign - --timestamp=none --entitlements "$entitlements" "$app"

if [[ "$platform" == device ]]; then
    echo "==> packaging ZiOS.ipa"
    mkdir -p "$output_directory/Payload"
    cp -R "$app" "$output_directory/Payload/"
    (cd "$output_directory" && rm -f ZiOS.ipa && zip -qry ZiOS.ipa Payload)
    rm -rf "$output_directory/Payload"
    (cd "$output_directory" && rm -f ZiOS.app.dSYM.zip && zip -qry ZiOS.app.dSYM.zip ZiOS.app.dSYM)
    ls -lh "$output_directory/ZiOS.ipa" "$output_directory/ZiOS.app.dSYM.zip"
else
    ls -lh "$app/ZiOS"
fi
echo "built $app"
