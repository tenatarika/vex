#!/bin/sh
# Install vex and vex-mcp from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/tenatarika/vex/main/install.sh | sh
#
# Environment:
#   VEX_VERSION      release to install, e.g. 1.27.4 (default: latest)
#   VEX_INSTALL_DIR  where the binaries go (default: ~/.local/bin)
#   VEX_NO_MCP=1     skip vex-mcp (the MCP server)
set -eu

repo="https://github.com/tenatarika/vex"
install_dir="${VEX_INSTALL_DIR:-$HOME/.local/bin}"
min_glibc="2.34"

say() { printf 'vex-install: %s\n' "$*"; }
die() { printf 'vex-install: error: %s\n' "$*" >&2; exit 1; }

source_hint="build from source instead: cargo install vex-search --locked (see $repo#installation)"

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os/$arch" in
    Darwin/arm64) echo "aarch64-apple-darwin" ;;
    Linux/x86_64 | Linux/amd64) echo "x86_64-unknown-linux-gnu" ;;
    *) die "no prebuilt vex for $os/$arch; $source_hint" ;;
  esac
}

# The Linux binaries link glibc dynamically: refuse musl, warn below 2.34.
check_glibc() {
  ldd_out="$(ldd --version 2>&1 || true)"
  case "$ldd_out" in
    *musl*) die "this system uses musl libc; the prebuilt vex needs glibc $min_glibc+; $source_hint" ;;
  esac
  have="$(printf '%s\n' "$ldd_out" | head -n1 | grep -oE '[0-9]+\.[0-9]+$' || true)"
  if [ -n "$have" ] && [ "$(printf '%s\n%s\n' "$have" "$min_glibc" | sort -V | head -n1)" != "$min_glibc" ]; then
    die "glibc $have is older than the $min_glibc vex needs; $source_hint"
  fi
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    die "need curl or wget"
  fi
}

install_one() {
  name="$1"
  archive="$name-$target.tar.gz"
  say "downloading $archive"
  download "$base/$archive" "$tmp/$archive" || die "could not download $base/$archive"
  tar -xzf "$tmp/$archive" -C "$tmp" "$name" || die "$archive has no $name binary"
  chmod 755 "$tmp/$name"
  mv -f "$tmp/$name" "$install_dir/$name"
}

target="$(detect_target)"
[ "$target" = "x86_64-unknown-linux-gnu" ] && check_glibc

if [ -n "${VEX_VERSION:-}" ]; then
  base="$repo/releases/download/v${VEX_VERSION#v}"
else
  base="$repo/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$install_dir"

install_one vex
[ "${VEX_NO_MCP:-0}" = "1" ] || install_one vex-mcp

say "installed $("$install_dir/vex" --version) to $install_dir"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) say "$install_dir is not on PATH; add it, e.g.: export PATH=\"$install_dir:\$PATH\"" ;;
esac
say "next: cd <your repo> && vex index"
