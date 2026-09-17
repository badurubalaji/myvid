#!/usr/bin/env bash
# One-line installer for myvid on Debian, Ubuntu and derivatives:
#
#   curl -fsSL https://raw.githubusercontent.com/badurubalaji/myvid/main/get.sh | bash
#
# Downloads the latest release .deb, checks it against the published SHA256SUMS,
# and installs it with apt, which also pulls in GStreamer, ffmpeg and Vulkan.
# Remove with: sudo apt remove myvid

set -euo pipefail

REPO="badurubalaji/myvid"
BASE="https://github.com/$REPO/releases/latest/download"

say()  { printf '\033[1;33m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m!!\033[0m  %s\n' "$*" >&2; exit 1; }

[[ "$(uname -s)" == Linux ]] || die "myvid packages are Linux-only"
command -v apt-get >/dev/null && command -v dpkg >/dev/null \
    || die "this installer needs apt (Debian, Ubuntu, Mint, Pop!_OS…); build from source instead: https://github.com/$REPO#build-from-source"
ARCH="$(dpkg --print-architecture)"
[[ "$ARCH" == amd64 ]] || die "no prebuilt package for $ARCH yet; build from source: https://github.com/$REPO#build-from-source"
command -v curl >/dev/null || die "curl is required"

SUDO=""
if [[ $EUID -ne 0 ]]; then
    command -v sudo >/dev/null || die "run as root or install sudo"
    SUDO="sudo"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
# apt's _apt sandbox user must be able to read the file.
chmod 755 "$tmp"

say "downloading myvid"
curl -fL --progress-bar -o "$tmp/myvid_$ARCH.deb" "$BASE/myvid_$ARCH.deb"
curl -fsSL -o "$tmp/SHA256SUMS" "$BASE/SHA256SUMS"

say "verifying checksum"
(cd "$tmp" && grep " myvid_$ARCH.deb\$" SHA256SUMS | sha256sum -c --quiet -) \
    || die "checksum mismatch — download corrupted or tampered with"
chmod 644 "$tmp/myvid_$ARCH.deb"

say "installing (apt will ask for your password if needed)"
$SUDO apt-get update -qq
$SUDO apt-get install -y "$tmp/myvid_$ARCH.deb"

say "done — launch myvid from your applications menu, or run: myvid <file>"
