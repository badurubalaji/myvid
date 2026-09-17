#!/usr/bin/env bash
# Package a built myvid binary as a .deb for Debian, Ubuntu and derivatives.
#
#   packaging/build-deb.sh [path/to/myvid]
#
# The binary defaults to target/release/myvid. Output goes to dist/:
#   myvid_<version>_amd64.deb   the package
#   myvid_amd64.deb             the same file under a version-less name, so
#                               releases/latest/download/myvid_amd64.deb is a
#                               link that never goes stale
#   SHA256SUMS
#
# Install the result with `sudo apt install ./dist/myvid_amd64.deb`; apt pulls
# in GStreamer, ffmpeg and the Vulkan loader from the Depends line.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-$ROOT/target/release/myvid}"
[[ -x "$BIN" ]] || { echo "no executable at $BIN — build first (cargo build --release --locked)" >&2; exit 1; }

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
ARCH="$(dpkg --print-architecture)"
DIST="$ROOT/dist"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# The oldest glibc the binary will load against: the highest GLIBC_x.y symbol
# version it references. Built on Ubuntu 24.04 this is 2.39.
GLIBC="$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"

pkg="$STAGE/pkg"
install -Dm755 "$BIN" "$pkg/usr/bin/myvid"
install -Dm644 "$ROOT/packaging/linux/myvid.desktop" "$pkg/usr/share/applications/myvid.desktop"
install -Dm644 "$ROOT/packaging/linux/io.github.badurubalaji.myvid.metainfo.xml" \
    "$pkg/usr/share/metainfo/io.github.badurubalaji.myvid.metainfo.xml"
for size in 512 256 128 64 48; do
    install -Dm644 "$ROOT/assets/myvid-$size.png" \
        "$pkg/usr/share/icons/hicolor/${size}x${size}/apps/myvid.png"
done

doc="$pkg/usr/share/doc/myvid"
install -Dm644 "$ROOT/README.md" "$doc/README.md"
install -Dm644 "$ROOT/THIRD-PARTY-LICENSES.md" "$doc/THIRD-PARTY-LICENSES.md"
gzip -9n "$doc/THIRD-PARTY-LICENSES.md"
{
    cat <<COPYRIGHT
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: myvid
Upstream-Contact: https://github.com/badurubalaji/myvid/issues
Source: https://github.com/badurubalaji/myvid

Files: *
Copyright: 2026 badurubalaji
License: MIT or Apache-2.0
Comment: /usr/bin/myvid statically links Rust crates under MIT, Apache-2.0,
 BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0 and CC0-1.0. Every crate,
 its license and the full license texts are listed in
 /usr/share/doc/myvid/THIRD-PARTY-LICENSES.md.gz

License: MIT
COPYRIGHT
    sed -e 's/^$/./' -e 's/^/ /' "$ROOT/LICENSE-MIT"
    echo
    echo "License: Apache-2.0"
    echo " On Debian systems, the full text of the Apache License 2.0 is in"
    echo " /usr/share/common-licenses/Apache-2.0"
} >"$doc/copyright"
chmod 644 "$doc/copyright"

installed_kb="$(du -sk "$pkg/usr" | cut -f1)"
install -d "$pkg/DEBIAN"
cat >"$pkg/DEBIAN/control" <<CONTROL
Package: myvid
Version: $VERSION
Architecture: $ARCH
Maintainer: badurubalaji <badurubalaji@gmail.com>
Installed-Size: $installed_kb
Section: video
Priority: optional
Homepage: https://github.com/badurubalaji/myvid
Depends: libc6 (>= $GLIBC), libgcc-s1,
 libgstreamer1.0-0, libgstreamer-plugins-base1.0-0,
 gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad,
 gstreamer1.0-libav, gstreamer1.0-pulseaudio,
 libvulkan1, mesa-vulkan-drivers,
 libxkbcommon0, libxkbcommon-x11-0, libwayland-client0,
 libx11-6, libxcursor1, libxrandr2, libxi6
Recommends: ffmpeg, gstreamer1.0-plugins-ugly, gstreamer1.0-vaapi, va-driver-all,
 xdg-desktop-portal
Suggests: intel-media-va-driver-non-free
Description: sandboxed video player with hardware decoding
 A video player built on GStreamer, wgpu and iced. It plays local files and
 network streams (HTTP, HLS, DASH, RTSP, RTMP, UDP, SRT), decodes on the GPU
 through VA-API where the hardware allows, and runs each file's decoder in its
 own process confined with Landlock to that one file.
 .
 Includes an audio and subtitle track picker, clear dialogue and night mode
 switches, and lossless clip export (which needs ffmpeg).
CONTROL

# Refresh the icon cache and desktop database after install and removal.
for script in postinst postrm; do
    cat >"$pkg/DEBIAN/$script" <<'HOOK'
#!/bin/sh
set -e
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
HOOK
    chmod 755 "$pkg/DEBIAN/$script"
done

mkdir -p "$DIST"
out="$DIST/myvid_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group -Zxz --build "$pkg" "$out" >/dev/null
cp "$out" "$DIST/myvid_${ARCH}.deb"
(cd "$DIST" && sha256sum "myvid_${VERSION}_${ARCH}.deb" "myvid_${ARCH}.deb" >SHA256SUMS)

echo "built $out ($(du -h "$out" | cut -f1), needs glibc >= $GLIBC)"
