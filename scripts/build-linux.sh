#!/usr/bin/env bash
# Builds dist/FlowBar: the program, its mihomo core and the license files, ready to pack.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --locked
if [[ $# -ge 1 ]]; then
    CORE="$1"
elif [[ -x "$HOME/.local/share/flowbar/mihomo" ]]; then
    CORE="$HOME/.local/share/flowbar/mihomo"
elif command -v mihomo >/dev/null; then
    CORE="$(command -v mihomo)"
else
    echo '请指定 mihomo 二进制路径：./scripts/build-linux.sh /path/to/mihomo' >&2
    exit 1
fi
mkdir -p dist
STAGING="$(mktemp -d "$PWD/dist/.pack-XXXXXX")"
trap 'rm -rf "$STAGING"' EXIT
APP="$STAGING/FlowBar"
mkdir -p "$APP"
install -m 755 target/release/flowbar "$APP/flowbar"
install -m 755 "$CORE" "$APP/mihomo"
install -m 644 Resources/linux/flowbar.png Resources/THIRD_PARTY.md Resources/LICENSE-MIHOMO "$APP/"
python3 scripts/licenses.py "$APP/LICENSE-RUST-DEPENDENCIES.txt"
"$APP/flowbar" --version
rm -rf dist/FlowBar
mv "$APP" dist/FlowBar
echo "已生成：$PWD/dist/FlowBar"
