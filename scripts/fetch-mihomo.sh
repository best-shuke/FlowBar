#!/usr/bin/env bash
# Downloads a pinned mihomo release asset, checks its SHA-256 and unpacks the core.
# Usage: fetch-mihomo.sh <version> <platform, e.g. linux-arm64> <sha256> <output>
set -euo pipefail
version=$1 platform=$2 expected=$3 output=$4
case $platform in
    windows-*) extension=zip ;;
    *) extension=gz ;;
esac
archive="$(mktemp -d)/mihomo.$extension"
curl -fsSL -o "$archive" \
    "https://github.com/MetaCubeX/mihomo/releases/download/$version/mihomo-$platform-$version.$extension"
python="$(command -v python3 || command -v python)"
"$python" - "$archive" "$expected" "$output" <<'EOF'
import gzip, hashlib, shutil, sys, zipfile
archive, expected, output = sys.argv[1:]
with open(archive, 'rb') as file:
    actual = hashlib.sha256(file.read()).hexdigest()
if actual != expected:
    sys.exit(f'mihomo SHA-256 is {actual}, expected {expected}')
if archive.endswith('.zip'):
    bundle = zipfile.ZipFile(archive)
    [name] = [name for name in bundle.namelist() if name.endswith('.exe')]
    source = bundle.open(name)
else:
    source = gzip.open(archive)
with source, open(output, 'wb') as target:
    shutil.copyfileobj(source, target)
EOF
[[ $platform == windows-* ]] || chmod 755 "$output"
