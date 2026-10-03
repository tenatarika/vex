#!/usr/bin/env bash
# Render the MCP Registry server.json for one release.
#
# Usage: packaging/mcpb/render-server-json.sh <version> <bundle_dir> [template]
#
# Reads the template (default: server.json at the repo root), replaces
# every __VERSION__ with <version>, and sets each package's fileSha256 to
# the SHA-256 of the matching bundle in <bundle_dir> (matched by the file
# name at the end of the package's identifier URL). Writes the result to
# stdout. Fails when a bundle is missing, a hash is malformed, or a
# placeholder survives.
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  echo "usage: $0 <version> <bundle_dir> [template]" >&2
  exit 2
fi

version="$1"
bundle_dir="$2"
here="$(cd "$(dirname "$0")" && pwd)"
template="${3:-${here}/../../server.json}"

if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$'; then
  echo "error: version '$version' is not semver (pass it without the leading v)" >&2
  exit 1
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

shas='{}'
while IFS= read -r url; do
  name="${url##*/}"
  file="${bundle_dir}/${name}"
  if [ ! -f "$file" ]; then
    echo "error: bundle ${file} not found (referenced by ${url})" >&2
    exit 1
  fi
  sha="$(sha256_of "$file")"
  if ! printf '%s' "$sha" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "error: bad sha256 '${sha}' for ${file}" >&2
    exit 1
  fi
  shas="$(jq --arg k "$name" --arg v "$sha" '. + {($k): $v}' <<<"$shas")"
done < <(jq -r --arg v "$version" '.packages[].identifier | gsub("__VERSION__"; $v)' "$template")

rendered="$(jq --arg v "$version" --argjson shas "$shas" '
  walk(if type == "string" then gsub("__VERSION__"; $v) else . end)
  | .packages |= map(.fileSha256 = $shas[.identifier | split("/") | last])
' "$template")"

if grep -q '__[A-Z0-9]*__' <<<"$rendered"; then
  echo "error: unrendered placeholder left in server.json" >&2
  exit 1
fi
desc_len="$(jq -r '.description | length' <<<"$rendered")"
if [ "$desc_len" -gt 100 ]; then
  echo "error: description is ${desc_len} chars; the registry caps it at 100" >&2
  exit 1
fi

printf '%s\n' "$rendered"
