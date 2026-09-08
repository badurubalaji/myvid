#!/usr/bin/env bash
# myvid installer.
#
#   sudo ./install.sh            deps + clean + build + install + reclaim space
#   sudo ./install.sh deps       system packages only
#        ./install.sh build      build only (no root needed)
#        ./install.sh reclaim    delete build artifacts, keep the installed binary
#   sudo ./install.sh uninstall  remove everything this script installed
#
# Root is needed only for apt. The build always runs as the invoking user, so
# nothing in ~/.cargo or ./target ends up owned by root.

set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
APP=myvid

# --- who are we, really -------------------------------------------------------
if [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
    BUILD_USER="$SUDO_USER"
    BUILD_HOME="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
else
    BUILD_USER="$(id -un)"
    BUILD_HOME="$HOME"
fi
PREFIX="${PREFIX/#\$HOME/$BUILD_HOME}"
[[ "$PREFIX" == "$HOME/.local" ]] && PREFIX="$BUILD_HOME/.local"

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Only one of these may run at a time. `clean` deletes the whole target tree, so
# a second run — or a cargo build someone left going in another terminal — would
# have its artifacts pulled out from under it mid-compile. The resulting errors
# are baffling ("extern location for glib does not exist"), so refuse up front.
exec 9>"$PROJECT_DIR/.install.lock"
if ! flock -n 9; then
    printf '\033[1;31m!!\033[0m  another install.sh is already running in %s\n' "$PROJECT_DIR" >&2
    exit 1
fi


say()  { printf '\033[1;33m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;31m!!\033[0m  %s\n' "$*" >&2; }

# `sudo -u ... -H` gives a non-login shell, which never sources ~/.profile or
# ~/.cargo/env — so a rustup toolchain in ~/.cargo/bin is invisible. Put it back
# on the PATH explicitly rather than relying on the caller's environment.
USER_PATH="$BUILD_HOME/.cargo/bin:$BUILD_HOME/.local/bin:/usr/local/bin:/usr/bin:/bin"

as_user() {
    if [[ "$(id -un)" == "$BUILD_USER" ]]; then
        PATH="$USER_PATH:$PATH" "$@"
    else
        sudo -u "$BUILD_USER" -H env "PATH=$USER_PATH:$PATH" "$@"
    fi
}

require_cargo() {
    if ! as_user bash -c 'command -v cargo' >/dev/null 2>&1; then
        warn "cargo not found for user '$BUILD_USER' (looked in $BUILD_HOME/.cargo/bin)"
        warn "install Rust with: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        exit 1
    fi
}

need_root() {
    if [[ $EUID -ne 0 ]]; then
        warn "this step needs root — run: sudo $0 ${1:-}"
        exit 1
    fi
}

# --- steps --------------------------------------------------------------------

PACKAGES=(
    # Build-time headers for the gstreamer/gstreamer-app/gstreamer-video crates.
    libgstreamer1.0-dev
    libgstreamer-plugins-base1.0-dev
    pkg-config
    # Runtime: demuxers, decoders and sinks.
    gstreamer1.0-plugins-base
    gstreamer1.0-plugins-good
    gstreamer1.0-plugins-bad
    gstreamer1.0-plugins-ugly
    gstreamer1.0-libav
    gstreamer1.0-pulseaudio
    # Lossless clip export shells out to ffmpeg: a flushing seek cannot pass
    # through a GStreamer muxer, and this does the job correctly in one command.
    ffmpeg
    # Hardware decode on Intel/AMD.
    gstreamer1.0-vaapi
    va-driver-all
    intel-media-va-driver-non-free
    # wgpu needs a Vulkan loader/ICD at runtime.
    libvulkan1
    mesa-vulkan-drivers
)

deps() {
    need_root deps
    say "installing system packages"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y --no-install-recommends "${PACKAGES[@]}"
    say "packages installed"
}

# Refuse to delete a tree that something else is actively compiling into.
assert_no_build_running() {
    local pids
    pids=$(pgrep -f "cargo (build|check|test)" 2>/dev/null | grep -v "^$$\$" || true)
    if [[ -n "$pids" ]]; then
        warn "a cargo build is already running (pid $(tr '\n' ' ' <<<"$pids"))"
        warn "wait for it to finish, or stop it, before cleaning this tree"
        exit 1
    fi
}

clean() {
    require_cargo
    assert_no_build_running
    say "cleaning previous builds"
    as_user bash -c "cd '$PROJECT_DIR' && cargo clean" || true
    rm -rf "$PROJECT_DIR/target"
    say "removed $PROJECT_DIR/target"
}

build() {
    require_cargo
    say "building release binary (this takes a few minutes on a cold cache)"
    as_user bash -c "cd '$PROJECT_DIR' && cargo build --release --locked"
    local bin="$PROJECT_DIR/target/release/$APP"
    [[ -x "$bin" ]] || { warn "build produced no binary"; exit 1; }
    say "built $(du -h "$bin" | cut -f1) binary"
}

install_app() {
    say "installing to $PREFIX"
    as_user install -Dm755 "$PROJECT_DIR/target/release/$APP" "$PREFIX/bin/$APP"

    # Icons: hicolor theme, referenced by name from the desktop entry.
    for size in 512 256 128 64 48; do
        src="$PROJECT_DIR/assets/myvid-${size}.png"
        [[ -f "$src" ]] && as_user install -Dm644 "$src" \
            "$PREFIX/share/icons/hicolor/${size}x${size}/apps/$APP.png"
    done
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        as_user gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" 2>/dev/null || true
    fi

    as_user install -d "$PREFIX/share/applications"
    as_user tee "$PREFIX/share/applications/$APP.desktop" >/dev/null <<DESKTOP
[Desktop Entry]
Type=Application
Name=myvid
GenericName=Video Player
Comment=Play video files
Icon=$APP
Exec=$PREFIX/bin/$APP %f
StartupWMClass=myvid
Terminal=false
Categories=AudioVideo;Player;Video;
MimeType=video/mp4;video/x-matroska;video/webm;video/x-msvideo;video/quicktime;video/mpeg;video/x-flv;video/mp2t;video/ogg;video/x-ms-wmv;
DESKTOP

    if command -v update-desktop-database >/dev/null 2>&1; then
        as_user update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
    fi

    say "installed $PREFIX/bin/$APP"
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) warn "$PREFIX/bin is not on PATH — add it to ~/.profile" ;;
    esac
}

reclaim() {
    local before after
    before=$(du -sm "$PROJECT_DIR/target" 2>/dev/null | cut -f1 || echo 0)
    say "reclaiming build space"
    as_user bash -c "cd '$PROJECT_DIR' && cargo clean"
    rm -rf "$PROJECT_DIR/target"
    after=$(du -sm "$BUILD_HOME/.cargo" 2>/dev/null | cut -f1 || echo 0)
    say "freed ${before} MiB of build artifacts (~/.cargo cache is ${after} MiB and shared with your other projects)"
    say "to shrink that too: rm -rf $BUILD_HOME/.cargo/registry/src  # re-extracted on demand"
}

uninstall() {
    say "removing installed files"
    rm -f "$PREFIX/bin/$APP" "$PREFIX/share/applications/$APP.desktop"
    rm -f "$PREFIX/share/icons/hicolor/"*"/apps/$APP.png"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
    fi
    say "removed"
}

case "${1:-all}" in
    deps)      deps ;;
    clean)     clean ;;
    build)     build ;;
    install)   install_app ;;
    reclaim)   reclaim ;;
    uninstall) uninstall ;;
    all)
        deps
        clean
        build
        install_app
        reclaim
        say "done — run '$APP <file>' or launch it from your applications menu"
        ;;
    *) warn "unknown step '$1' (deps|clean|build|install|reclaim|uninstall|all)"; exit 1 ;;
esac
