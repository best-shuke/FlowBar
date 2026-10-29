#!/usr/bin/env bash
# Build the desktop core, including opt-in XPro, from a local rill checkout.
# Usage: build-core.sh /path/to/rill [output directory] [target triple]
set -euo pipefail
script_root="$(cd "$(dirname "$0")" && pwd)"
source_root="$(cd "$1" && pwd)"
output="${2:-$(dirname "$0")/../.agent-artifacts/core}"
mkdir -p "$output"
output="$(cd "$output" && pwd)"
cd "$source_root"
export RILL_FEATURES=xpro
host_target="$(rustc -vV | sed -n 's/^host: //p')"
target="${3:-$host_target}"
build=(cargo build --release --locked -p rill-app --bin rill --features "$RILL_FEATURES")
if [[ $# -ge 3 ]]; then
    build+=(--target "$target")
    relative_output="$target/release"
else
    relative_output=release
fi
case "$target" in
    *-linux-musl) build[1]=zigbuild ;;
    *-apple-darwin) export MACOSX_DEPLOYMENT_TARGET=13.0 ;;
esac
"${build[@]}"
target_root="$(cargo metadata --format-version 1 --no-deps --locked | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
case "$target" in
    *-windows-*) core=rill.exe ;;
    *) core=rill ;;
esac
cp "$target_root/$relative_output/$core" "$output/$core"
# The offline notices generator resolves the complete host graph, including
# packages not compiled by the binary target.
cargo fetch --locked --target "$target"
python3 scripts/licenses.py "$output/LICENSE-RILL-THIRD-PARTY.txt" "$target"
case "$target" in
    *-windows-*)
        bash scripts/fetch-wintun.sh --target "$target" --outdir "$output/wintun"
        cp "$output/wintun/LICENSE.txt" "$output/LICENSE-WINTUN.txt"
        ;;
esac
if [[ "$target" == "$host_target" ]] || [[ "$target" == *-linux-musl && "$host_target" == *-linux-* && "${target%%-*}" == "${host_target%%-*}" ]]; then
    python3 "$script_root/core-smoke-test.py" "$output/$core"
fi
git rev-parse HEAD > "$output/CORE-REVISION.txt"
printf 'Core built with XPro: %s\n' "$output/$core"
