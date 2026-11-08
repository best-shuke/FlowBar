#!/usr/bin/env bash
# Downloads the Geo databases a package bundles from the latest meta-rules-dat release and
# checks each against the SHA-256 published beside it.
# Usage: fetch-geo.sh <output directory>
set -euo pipefail
output=$1
python="$(command -v python3 || command -v python)"
asset="$(dirname "$0")/github-asset.sh"
mkdir -p "$output"
# The release's file name, then the name FlowBar copies into the core's data directory.
for pair in geoip.metadb:geoip.metadb geosite.dat:GeoSite.dat; do
    remote=${pair%%:*} name=${pair#*:}
    "$asset" MetaCubeX/meta-rules-dat latest "$remote" "$output/$name"
    "$asset" MetaCubeX/meta-rules-dat latest "$remote.sha256sum" "$output/.$name.sha256sum"
    expected="$(cat "$output/.$name.sha256sum")"
    rm "$output/.$name.sha256sum"
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
