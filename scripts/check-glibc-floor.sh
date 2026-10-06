#!/usr/bin/env bash
# Fail if any given Linux binary needs a newer glibc than the floor.
# Usage: scripts/check-glibc-floor.sh <max-glibc, e.g. 2.35> <binary>...
set -euo pipefail
max="$1"; shift
status=0
for bin in "$@"; do
  need=$(grep -a -o 'GLIBC_[0-9][0-9.]*' "$bin" | sed 's/GLIBC_//' | sort -V | tail -1)
  if [ -z "$need" ]; then
    echo "::error::$bin: no GLIBC version symbols found"; status=1; continue
  fi
  if [ "$(printf '%s\n%s\n' "$need" "$max" | sort -V | tail -1)" != "$max" ]; then
    echo "::error::$bin needs glibc $need, above the $max floor (older distros such as Ubuntu 22.04 could not run it)"
    status=1
  else
    echo "$bin: needs glibc $need (floor $max) — ok"
  fi
done
exit "$status"
