#!/usr/bin/env bash
# Downloads a pinned rill release archive, checks its SHA-256 and unpacks into a directory the
# core, rill (rill.exe), with the notices of what it links: LICENSE-RILL-THIRD-PARTY.txt and,
# for Windows, LICENSE-WINTUN.txt. rill's own LICENSE and NOTICE are in Resources/.
# Usage: fetch-rill.sh <version, e.g. v0.1.0> <target, e.g. aarch64-apple-darwin> <sha256> <directory>
# With GH_TOKEN set, the GitHub CLI downloads it, which also reads a private repository.
set -euo pipefail
version=$1 target=$2 expected=$3 output=$4
repository=best-shuke/rill
case $target in
    *-windows-*) extension=zip ;;
    *) extension=tar.gz ;;
esac
asset="rill-$version-$target.$extension"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
if [[ -n ${GH_TOKEN:-} ]]; then
    gh release download "$version" --repo "$repository" --pattern "$asset" --dir "$work"
else
    curl -fsSL -o "$work/$asset" "https://github.com/$repository/releases/download/$version/$asset"
fi
mkdir -p "$output"
python="$(command -v python3 || command -v python)"
"$python" - "$work/$asset" "$expected" "$output" <<'PY'
import hashlib, os, sys, tarfile, zipfile
archive, expected, output = sys.argv[1:]
with open(archive, 'rb') as file:
    actual = hashlib.sha256(file.read()).hexdigest()
if actual != expected:
    sys.exit(f'rill SHA-256 is {actual}, expected {expected}')
# Archive member name -> name in the output directory.
wanted = {'rill': 'rill', 'rill.exe': 'rill.exe',
          'LICENSE-THIRD-PARTY.txt': 'LICENSE-RILL-THIRD-PARTY.txt',
          'LICENSE-WINTUN.txt': 'LICENSE-WINTUN.txt'}
if archive.endswith('.zip'):
    bundle = zipfile.ZipFile(archive)
    members = [(name, lambda name=name: bundle.open(name)) for name in bundle.namelist()]
else:
    bundle = tarfile.open(archive)
    members = [(m.name, lambda m=m: bundle.extractfile(m)) for m in bundle.getmembers() if m.isfile()]
found = set()
for name, open_member in members:
    # Members sit in one top-level directory; Windows zips may use backslashes.
    base = name.replace('\\', '/').rsplit('/', 1)[-1]
    if base not in wanted:
        continue
    with open_member() as source, open(os.path.join(output, wanted[base]), 'wb') as target:
        target.write(source.read())
    found.add(base)
if not found & {'rill', 'rill.exe'}:
    sys.exit(f'{archive} has no rill executable')
if 'LICENSE-THIRD-PARTY.txt' not in found:
    sys.exit(f'{archive} lacks LICENSE-THIRD-PARTY.txt')
PY
[[ $target == *-windows-* ]] || chmod 755 "$output/rill"
