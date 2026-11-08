#!/usr/bin/env bash
# Downloads one release asset of a GitHub repository to a file.
# Usage: github-asset.sh <owner/repository> <tag> <asset name> <output>
# The release download links fail now and then, sometimes for hours for one repository,
# while the API serves the same file; the callers check every download's SHA-256.
# GITHUB_TOKEN, when set, lifts the API's rate limit for shared CI runners.
set -euo pipefail
repository=$1 tag=$2 name=$3 output=$4
retry=(--retry 4 --retry-delay 3 --retry-all-errors)
if curl -fsSL "${retry[@]}" -o "$output" \
    "https://github.com/$repository/releases/download/$tag/$name"; then
    exit 0
fi
echo "$name: the release download failed; trying the GitHub API" >&2
# macOS's bash 3.2 treats an empty array as unset, hence the ${auth[@]+…} form.
auth=()
if [[ -n ${GITHUB_TOKEN:-} ]]; then
    auth=(-H "Authorization: Bearer $GITHUB_TOKEN")
fi
python="$(command -v python3 || command -v python)"
id="$(curl -fsSL "${retry[@]}" ${auth[@]+"${auth[@]}"} -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$repository/releases/tags/$tag" |
    "$python" -c 'import json, sys
# No newline: on Windows it would end in a carriage return that $(…) keeps.
sys.stdout.write(str(next(a["id"] for a in json.load(sys.stdin)["assets"] if a["name"] == sys.argv[1])))' "$name")"
curl -fsSL "${retry[@]}" ${auth[@]+"${auth[@]}"} -H 'Accept: application/octet-stream' -o "$output" \
    "https://api.github.com/repos/$repository/releases/assets/$id"
