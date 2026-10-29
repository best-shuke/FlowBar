#!/bin/zsh
# Usage: build.sh [rill] [Geo directory from scripts/fetch-geo.sh]
# The rill core's LICENSE-RILL-THIRD-PARTY.txt (from scripts/fetch-rill.sh) is bundled when it
# sits beside the core.
set -euo pipefail
cd "$(dirname "$0")/.."
MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release --locked
mkdir -p "$PWD/dist"
STAGING_ROOT="$(mktemp -d "$PWD/dist/.pack-XXXXXX")"
trap 'rm -rf "$STAGING_ROOT"' EXIT
APP="$STAGING_ROOT/FlowBar.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/flowbar "$APP/Contents/MacOS/FlowBar"
cp Resources/Info.plist "$APP/Contents/Info.plist"
if [[ $# -ge 1 ]]; then
    CORE="$1"
elif [[ -x "$HOME/Applications/FlowBar.app/Contents/Resources/rill" ]]; then
    CORE="$HOME/Applications/FlowBar.app/Contents/Resources/rill"
elif command -v rill >/dev/null; then
    CORE="$(command -v rill)"
else
    print -u2 '请指定 rill 二进制路径：./scripts/build.sh /path/to/rill'
    exit 1
fi
cp "$CORE" "$APP/Contents/Resources/rill"
chmod 755 "$APP/Contents/Resources/rill"
# Not a program: lets FlowBar 0.4.0 and earlier recognise this package when they update.
cp Resources/legacy-core-placeholder.txt "$APP/Contents/Resources/mihomo"
cp Resources/THIRD_PARTY.md "$APP/Contents/Resources/"
if [[ $# -ge 2 ]]; then
    cp "$2/geoip.metadb" "$2/GeoSite.dat" "$APP/Contents/Resources/"
fi
cp Resources/LICENSE-RILL Resources/NOTICE-RILL Resources/LICENSE-GPL-3.0 "$APP/Contents/Resources/"
if [[ -f "${CORE:h}/LICENSE-RILL-THIRD-PARTY.txt" ]]; then
    cp "${CORE:h}/LICENSE-RILL-THIRD-PARTY.txt" "$APP/Contents/Resources/"
fi
if [[ -f "${CORE:h}/CORE-REVISION.txt" ]]; then
    cp "${CORE:h}/CORE-REVISION.txt" "$APP/Contents/Resources/"
fi
python3 scripts/licenses.py "$APP/Contents/Resources/LICENSE-RUST-DEPENDENCIES.txt"
cp Resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
codesign --force --sign - "$APP/Contents/Resources/rill"
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"
rm -rf "$PWD/dist/FlowBar.app"
mv "$APP" "$PWD/dist/FlowBar.app"
print "已生成：$PWD/dist/FlowBar.app"
