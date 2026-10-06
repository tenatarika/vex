#!/usr/bin/env bash
# Run the Linux release binaries on Rocky Linux 9 (the RHEL 9 family: glibc
# 2.34, GCC 11's libstdc++). Proves they start, index and answer there, not
# just that their symbol versions look right.
# Usage: scripts/smoke-rhel9.sh <dir containing vex and vex-mcp>
set -euo pipefail
bin_dir="$(cd "$1" && pwd)"
image="${SMOKE_IMAGE:-rockylinux/rockylinux:9}"

docker run --rm -v "${bin_dir}:/w:ro" "${image}" bash -euo pipefail -c '
  . /etc/os-release && echo "$PRETTY_NAME"
  /w/vex --version

  mkdir -p /tmp/proj && cd /tmp/proj
  printf "def smoke_probe_fn():\n    return 1\n" > app.py
  /w/vex index --path . >/dev/null
  /w/vex check smoke_probe_fn --path . | grep -q smoke_probe_fn

  init='"'"'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'"'"'
  echo "$init" | VEX_BIN=/w/vex timeout 20 /w/vex-mcp | grep -q serverInfo
  echo "vex and vex-mcp run on $PRETTY_NAME"
'
