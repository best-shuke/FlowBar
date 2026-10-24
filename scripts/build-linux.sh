#!/usr/bin/env bash
# Builds dist/FlowBar: the program, its rill core and the license files, ready to pack.
# Usage: build-linux.sh [rill] [Geo directory from scripts/fetch-geo.sh]
# The rill core's LICENSE-RILL-THIRD-PARTY.txt (from scripts/fetch-rill.sh) is bundled when it
# sits beside the core.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --locked
if [[ $# -ge 1 ]]; then
    CORE="$1"
elif [[ -x "$HOME/.local/share/flowbar/rill" ]]; then
    CORE="$HOME/.local/share/flowbar/rill"
elif command -v rill >/dev/null; then
    CORE="$(command -v rill)"
else
    echo '请指定 rill 二进制路径：./scripts/build-linux.sh /path/to/rill' >&2
    exit 1
fi
mkdir -p dist
STAGING="$(mktemp -d "$PWD/dist/.pack-XXXXXX")"
trap 'rm -rf "$STAGING"' EXIT
APP="$STAGING/FlowBar"
mkdir -p "$APP"
install -m 755 target/release/flowbar "$APP/flowbar"
install -m 755 "$CORE" "$APP/rill"
# Not a program: lets FlowBar 0.4.0 and earlier recognise this package when they update.
install -m 644 Resources/legacy-core-placeholder.txt "$APP/mihomo"
install -m 644 Resources/linux/flowbar.png Resources/THIRD_PARTY.md Resources/LICENSE-RILL \
    Resources/NOTICE-RILL Resources/LICENSE-GPL-3.0 "$APP/"
if [[ -f "$(dirname "$CORE")/LICENSE-RILL-THIRD-PARTY.txt" ]]; then
    install -m 644 "$(dirname "$CORE")/LICENSE-RILL-THIRD-PARTY.txt" "$APP/"
fi
if [[ $# -ge 2 ]]; then
    install -m 644 "$2/geoip.metadb" "$2/GeoSite.dat" "$APP/"
fi
python3 scripts/licenses.py "$APP/LICENSE-RUST-DEPENDENCIES.txt"
"$APP/flowbar" --version
rm -rf dist/FlowBar
mv "$APP" dist/FlowBar
echo "已生成：$PWD/dist/FlowBar"
