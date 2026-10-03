#!/usr/bin/env bash
# Build ONE MCP Bundle (.mcpb) for one release target.
#
# Usage: packaging/mcpb/build-bundle.sh <target> <version> <stage_dir> <out_dir>
#
#   <stage_dir>/server/ must already hold the release binaries for <target>:
#     unix:    vex, vex-mcp
#     windows: vex.exe, vex-mcp.exe, DirectML.dll  (the DLL rides in the
#              vex-<target>.tar.gz archive; without it the DirectML EP
#              silently falls back to CPU)
#   The script renders <stage_dir>/manifest.json from the template next to
#   it, validates it, and packs <out_dir>/vex-mcp-<target>.mcpb.
#
# The bundle is named `vex-mcp-<target>.mcpb`, never `vex-<target>.mcpb`:
# `vex self-update` selects its asset with `name.contains("vex-<target>")`
# and must keep picking `vex-<target>.tar.gz` (pinned by the
# `mcpb_bundle_never_matches_self_update_identifier` test in
# src/cli/cmd_self_update.rs).
#
# Used by the `mcpb` job in .github/workflows/release.yml and by
# packaging/mcpb/dry-run.sh. The mcpb CLI comes from the lockfile next to
# this script (`npm ci --ignore-scripts --prefix packaging/mcpb`); $MCPB
# overrides it with a command line, e.g. MCPB="npx -y @anthropic-ai/mcpb@2.1.2".
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 <target> <version> <stage_dir> <out_dir>" >&2
  exit 2
fi

target="$1"
version="$2"
stage="$3"
out_dir="$4"
here="$(cd "$(dirname "$0")" && pwd)"
template="${here}/manifest.json"
if [ -n "${MCPB:-}" ]; then
  # Deliberate word splitting: $MCPB is a command line, not a path.
  read -r -a mcpb <<<"$MCPB"
else
  mcpb=("${here}/node_modules/.bin/mcpb")
  if [ ! -x "${mcpb[0]}" ]; then
    echo "error: ${mcpb[0]} missing — run: npm ci --ignore-scripts --prefix packaging/mcpb" >&2
    exit 1
  fi
fi

if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$'; then
  echo "error: version '$version' is not semver (pass it without the leading v)" >&2
  exit 1
fi

case "$target" in
  *-apple-darwin) platform=darwin; exe="" ;;
  *-linux-*) platform=linux; exe="" ;;
  *-windows-*) platform=win32; exe=".exe" ;;
  *) echo "error: unsupported target '$target'" >&2; exit 1 ;;
esac

server="${stage}/server"
required=("vex${exe}" "vex-mcp${exe}")
if [ "$platform" = win32 ]; then
  required+=("DirectML.dll")
fi
for f in "${required[@]}"; do
  if [ ! -f "${server}/${f}" ]; then
    echo "error: ${server}/${f} missing — extract vex-${target}.tar.gz and vex-mcp-${target}.tar.gz into ${server} first" >&2
    exit 1
  fi
done

# `mcpb pack` stores the on-disk mode in the zip; a host that unpacks a
# non-executable vex-mcp cannot launch the server, and vex-mcp refuses a
# non-executable VEX_BIN.
if [ "$platform" != win32 ]; then
  chmod 0755 "${server}/vex" "${server}/vex-mcp"
fi

# Windows paths use backslashes; the host substitutes ${__dirname} only.
if [ "$platform" = win32 ]; then
  entry='server/vex-mcp.exe'
  cmd='${__dirname}\server\vex-mcp.exe'
  bin='${__dirname}\server\vex.exe'
else
  entry='server/vex-mcp'
  cmd='${__dirname}/server/vex-mcp'
  bin='${__dirname}/server/vex'
fi

jq --arg version "$version" \
   --arg platform "$platform" \
   --arg entry "$entry" \
   --arg cmd "$cmd" \
   --arg bin "$bin" \
   '.version = $version
    | .server.entry_point = $entry
    | .server.mcp_config.command = $cmd
    | .server.mcp_config.env.VEX_BIN = $bin
    | .compatibility.platforms = [$platform]' \
   "$template" > "${stage}/manifest.json"

mkdir -p "$out_dir"
bundle="$(cd "$out_dir" && pwd)/vex-mcp-${target}.mcpb"
rm -f "$bundle"

"${mcpb[@]}" validate "${stage}/manifest.json"
"${mcpb[@]}" pack "$stage" "$bundle"

echo "built ${bundle}"
