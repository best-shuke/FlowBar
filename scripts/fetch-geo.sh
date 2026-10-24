#!/usr/bin/env bash
# Downloads the Geo databases a package bundles from the latest meta-rules-dat release and
# checks each against the SHA-256 published beside it.
# Usage: fetch-geo.sh <output directory>
set -euo pipefail
output=$1
release=https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest
python="$(command -v python3 || command -v python)"
mkdir -p "$output"
# The release's file name, then the name FlowBar copies into mihomo's data directory.
for pair in geoip.metadb:geoip.metadb geosite.dat:GeoSite.dat; do
    remote=${pair%%:*} name=${pair#*:}
    curl -fsSL --retry 3 -o "$output/$name" "$release/$remote"
    expected="$(curl -fsSL --retry 3 "$release/$remote.sha256sum")"
    "$python" - "$output/$name" "${expected%% *}" <<'EOF'
import hashlib, sys
path, expected = sys.argv[1:]
with open(path, 'rb') as file:
    actual = hashlib.sha256(file.read()).hexdigest()
if actual != expected:
    sys.exit(f'{path} SHA-256 is {actual}, expected {expected}')
print(f'{actual}  {path}')
EOF
done
