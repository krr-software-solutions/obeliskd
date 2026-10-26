#!/bin/sh
# Install obeliskd from a GitHub release: download and verify the binary for this machine, then
# hand over to `obeliskd manage install` (as root, via sudo), which puts it in /usr/local/bin and
# runs it as a service (systemd / launchd), started now and at every boot.
#
#   curl -fsSL https://github.com/krr-software-solutions/obeliskd/releases/latest/download/install.sh | sh
#
# Arguments are passed on to `manage install`, e.g. `… | sh -s -- --log-level debug`.
#
# Environment:
#   OBELISKD_VERSION       the version to install, e.g. 0.1.2 (default: the latest release)
#   OBELISKD_TARGET        override the detected target, e.g. aarch64-unknown-linux-musl
#   OBELISKD_INSTALL_DIR   only put the binary here: no sudo, no service (e.g. to try it out)
#
# Linux (x86_64, aarch64; static, so any distro; the service needs systemd) and macOS (x86_64,
# arm64). To uninstall: `sudo obeliskd manage uninstall`.
#
# Everything is inside main(), called on the last line, so a partial download runs nothing.
set -eu

REPO="krr-software-solutions/obeliskd"

say() { printf 'obeliskd-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }
has() { command -v "$1" > /dev/null 2>&1; }

# The Rust target triple of the release archive for this machine.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "unsupported CPU architecture: $arch (prebuilt: x86_64, aarch64; or: cargo install obeliskd-cli)" ;;
  esac
  case "$os" in
    Linux) echo "$arch-unknown-linux-musl" ;;
    Darwin)
      # An x86_64 shell under Rosetta still gets the native arm64 binary.
      if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2> /dev/null || echo 0)" = 1 ]; then
        arch=aarch64
      fi
      echo "$arch-apple-darwin"
      ;;
    *) die "unsupported OS: $os (prebuilt: Linux, macOS; or: cargo install obeliskd-cli)" ;;
  esac
}

download() { # <url> <file>
  if has curl; then
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location --output "$2" "$1"
  elif has wget; then
    wget --quiet --output-document="$2" "$1"
  else
    die "need curl or wget"
  fi
}

sha256_of() { # <file>
  if has sha256sum; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif has shasum; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

main() {
  has curl || has wget || die "need curl or wget"
  target="${OBELISKD_TARGET:-$(detect_target)}"
  version="${OBELISKD_VERSION:-}"
  version="${version#v}"

  if [ -n "$version" ]; then
    base="https://github.com/$REPO/releases/download/v$version"
  else
    base="https://github.com/$REPO/releases/latest/download"
  fi
  archive="obeliskd-$target.tar.gz"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  say "downloading $archive (${version:-latest})"
  download "$base/$archive" "$tmp/$archive" || die "couldn't download $base/$archive"
  download "$base/$archive.sha256" "$tmp/$archive.sha256" || die "couldn't download $base/$archive.sha256"

  expected="$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")"
  actual="$(sha256_of "$tmp/$archive")"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $archive: expected $expected, got $actual"

  tar -xzf "$tmp/$archive" -C "$tmp"
  binary="$(find "$tmp" -type f -name obeliskd | head -n 1)"
  [ -n "$binary" ] || die "no obeliskd binary in $archive"
  chmod 755 "$binary"

  # Just the binary, where asked: no root, no service.
  if [ -n "${OBELISKD_INSTALL_DIR:-}" ]; then
    dir="$OBELISKD_INSTALL_DIR"
    mkdir -p "$dir" || die "can't create $dir"
    # Copy then rename, so a running obeliskd is never left half-written.
    cp "$binary" "$dir/.obeliskd.tmp"
    mv -f "$dir/.obeliskd.tmp" "$dir/obeliskd"
    say "installed $("$dir/obeliskd" --version) to $dir/obeliskd (no service: OBELISKD_INSTALL_DIR is set)"
    return
  fi

  # The service: `manage install` copies the binary to /usr/local/bin and sets everything up.
  if [ "$(id -u)" -eq 0 ]; then
    "$binary" manage install "$@"
  elif has sudo; then
    say "installing as root (sudo): $("$binary" --version) to /usr/local/bin, as a service"
    sudo "$binary" manage install "$@"
  else
    die "installing the service needs root: run this as root, or install sudo (or set OBELISKD_INSTALL_DIR for just the binary)"
  fi
}

main "$@"
