#!/bin/zsh
# Usage: build.sh [mihomo] [Geo directory from scripts/fetch-geo.sh]
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
elif [[ -x "$HOME/Applications/FlowBar.app/Contents/Resources/mihomo" ]]; then
    CORE="$HOME/Applications/FlowBar.app/Contents/Resources/mihomo"
elif [[ -x '/Applications/Clash Verge.app/Contents/MacOS/verge-mihomo' ]]; then
    CORE='/Applications/Clash Verge.app/Contents/MacOS/verge-mihomo'
elif command -v mihomo >/dev/null; then
    CORE="$(command -v mihomo)"
else
    print -u2 '请指定 mihomo 二进制路径：./scripts/build.sh /path/to/mihomo'
    exit 1
fi
cp "$CORE" "$APP/Contents/Resources/mihomo"
chmod 755 "$APP/Contents/Resources/mihomo"
cp Resources/THIRD_PARTY.md "$APP/Contents/Resources/"
if [[ $# -ge 2 ]]; then
    cp "$2/geoip.metadb" "$2/GeoSite.dat" "$APP/Contents/Resources/"
fi
cp Resources/LICENSE-MIHOMO "$APP/Contents/Resources/"
python3 scripts/licenses.py "$APP/Contents/Resources/LICENSE-RUST-DEPENDENCIES.txt"
cp Resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
codesign --force --sign - "$APP/Contents/Resources/mihomo"
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP"
rm -rf "$PWD/dist/FlowBar.app"
mv "$APP" "$PWD/dist/FlowBar.app"
print "已生成：$PWD/dist/FlowBar.app"
