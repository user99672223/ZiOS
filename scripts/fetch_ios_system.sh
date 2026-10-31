#!/usr/bin/env bash
# Downloads ios_system's released xcframeworks (v3.0.6) into ios/Frameworks and
# verifies them against the checksums in ios_system's Package.swift.
#
# The command dictionaries that map command names to these frameworks are kept
# in ios/App/Resources (copied from tag v3.0.7, which matches these binaries).
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
destination="$repository_root/ios/Frameworks"
version="v3.0.6"
base="https://github.com/holzschu/ios_system/releases/download/$version"

# name checksum. The v3.0.7 release has no assets; the files served under v3.0.6
# were re-uploaded (2026-10-02) and match the checksums in ios_system's master
# Package.swift (commit b51dd90d), which are pinned here.
frameworks=(
    "ios_system c9c6dc4cde45cc271ccc8bd0553ce4d3149eced1f0ee3d9e0ce01885d93526e6"
    "awk 675d3b2e3b5ba60ae1a727528b2cf5e11eda262d404c1e9708dc7e061898df86"
    "curl_ios f9a74e005115aad3b51dd8daa2fb4727eb8425018805d7dd2973e598af65f968"
    "files 5db63958f686cc79f107bafd0d50feff32fb2b64d0dd976478c5dd405f7600b0"
    "shell 89da3b8bd7f83e42a2311aeac54c38acc7a2f9c2fc0d0d27f75897b393cdfec2"
    "ssh_cmd fbe01fb1cf471cbdc4893ddd6e88ff1def7480cab40a6dfe3ea1626d4b35d8c4"
    "tar 74146ae19720d3d885d181bce4be2b6b92f11d306a47aa07486bf0dd35fc8f68"
    "text 9eef16c23ddaa03d5f28ce2183737b1dbdcfb06e6dc46035a404d9e0f3828604"
)

mkdir -p "$destination"
stamp="$destination/.ios_system-$version"
if [[ -f "$stamp" ]]; then
    echo "ios_system $version already present"
    exit 0
fi

downloads="$(mktemp -d)"
trap 'rm -rf "$downloads"' EXIT
for entry in "${frameworks[@]}"; do
    read -r name checksum <<< "$entry"
    archive="$downloads/$name.xcframework.zip"
    echo "==> $name"
    curl --fail --location --silent --show-error --retry 4 --retry-delay 2 \
        -o "$archive" "$base/$name.xcframework.zip"
    actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
    if [[ "$actual" != "$checksum" ]]; then
        echo "checksum mismatch for $name: expected $checksum, got $actual" >&2
        exit 1
    fi
    rm -rf "$destination/$name.xcframework"
    unzip -q -o "$archive" -d "$destination"
done
touch "$stamp"
ls "$destination"
