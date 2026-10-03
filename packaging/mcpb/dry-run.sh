#!/usr/bin/env bash
# Local dry run of the release `mcpb` job for the HOST platform only.
#
# Usage: packaging/mcpb/dry-run.sh [out_dir]      (default: target/mcpb-dry-run)
#
# Needs: a release build (`cargo build --release --workspace`), node/npm
# (installs the lockfile-pinned @anthropic-ai/mcpb into
# packaging/mcpb/node_modules on first run, unless $MCPB is set), jq,
# unzip. Builds
# vex-mcp-<host>.mcpb from target/release/{vex,vex-mcp}, then unpacks it
# and checks the manifest, file list and executable bits, launches the
# bundled server for an MCP initialize + tools/list round trip, and prints
# the bundle's SHA-256. Publishes nothing.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out_dir="${1:-${repo}/target/mcpb-dry-run}"
target="$(rustc -vV | sed -n 's/^host: //p')"
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "${repo}/Cargo.toml")"

case "$target" in
  *-windows-*) echo "error: run the dry run on macOS or Linux" >&2; exit 1 ;;
esac

for b in vex vex-mcp; do
  if [ ! -x "${repo}/target/release/${b}" ]; then
    echo "error: target/release/${b} missing — run cargo build --release --workspace" >&2
    exit 1
  fi
done

if [ -z "${MCPB:-}" ] && [ ! -x "${repo}/packaging/mcpb/node_modules/.bin/mcpb" ]; then
  npm ci --ignore-scripts --prefix "${repo}/packaging/mcpb"
fi

rm -rf "$out_dir"
stage="${out_dir}/stage"
mkdir -p "${stage}/server"
cp "${repo}/target/release/vex" "${repo}/target/release/vex-mcp" "${stage}/server/"

"${repo}/packaging/mcpb/build-bundle.sh" "$target" "$version" "$stage" "$out_dir"
bundle="${out_dir}/vex-mcp-${target}.mcpb"

echo "--- contents"
unzip -Z -l "$bundle"

unpacked="${out_dir}/unpacked"
mkdir -p "$unpacked"
unzip -q "$bundle" -d "$unpacked"

echo "--- checks"
for b in vex vex-mcp; do
  [ -x "${unpacked}/server/${b}" ] || { echo "FAIL: server/${b} not executable after unzip" >&2; exit 1; }
  echo "OK: server/${b} executable"
done
jq -e --arg v "$version" '.version == $v and .server.type == "binary" and .name == "vex"' \
  "${unpacked}/manifest.json" >/dev/null || { echo "FAIL: manifest fields" >&2; exit 1; }
echo "OK: manifest version=${version} platforms=$(jq -c .compatibility.platforms "${unpacked}/manifest.json")"

# Launch the bundled server the way a host would: command + env from the
# manifest with ${__dirname} and ${user_config.project_root} substituted.
echo "--- server smoke (initialize + tools/list)"
reply="$(
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"dry-run","version":"0"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' |
  VEX_BIN="${unpacked}/server/vex" VEX_ROOT="$repo" "${unpacked}/server/vex-mcp"
)"
tools="$(jq -s 'map(select(.id == 2)) | .[0].result.tools | length' <<<"$reply")"
[ "$tools" -gt 0 ] || { echo "FAIL: tools/list returned no tools" >&2; exit 1; }
echo "OK: bundled vex-mcp lists ${tools} tools"

echo "--- sha256"
if command -v sha256sum >/dev/null 2>&1; then sha256sum "$bundle"; else shasum -a 256 "$bundle"; fi
