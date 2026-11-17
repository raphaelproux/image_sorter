#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="image_sorter"
MODE="install"
if [[ "${1:-}" == "--update" ]]; then
    MODE="update"
    shift
fi
if (( $# > 0 )); then
    printf 'Usage: %s [--update]\n' "$0" >&2
    exit 1
fi
BIN_HOME="${BIN_HOME:-$HOME/.local/bin}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_DIR="$DATA_HOME/icons/hicolor/512x512/apps"
DESKTOP_DIR="$DATA_HOME/applications"

if [[ "$(uname -s)" != "Linux" ]]; then
    printf 'This installer supports Solus OS on Linux only.\n' >&2
    exit 1
fi

if [[ ! -r /etc/os-release ]] || ! . /etc/os-release || [[ "${ID:-}" != "solus" ]]; then
    printf 'This installer is intended for Solus OS.\n' >&2
    exit 1
fi

if [[ "$MODE" == "update" && ! -x "$BIN_HOME/$APP_NAME" ]]; then
    printf 'Image Sorter is not installed at %s; run this script without --update first.\n' "$BIN_HOME/$APP_NAME" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    printf 'Cargo is required. Install Rust and Cargo with eopkg, then rerun this script.\n' >&2
    exit 1
fi

cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"

mkdir -p "$BIN_HOME" "$ICON_DIR" "$DESKTOP_DIR"
install -m755 "$ROOT_DIR/target/release/$APP_NAME" "$BIN_HOME/$APP_NAME"
install -m644 "$ROOT_DIR/icon/icon.png" "$ICON_DIR/$APP_NAME.png"

cat > "$DESKTOP_DIR/$APP_NAME.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Image Sorter
Exec=$BIN_HOME/$APP_NAME %U
TryExec=$BIN_HOME/$APP_NAME
Icon=$APP_NAME
StartupWMClass=$APP_NAME
StartupNotify=true
Terminal=false
Categories=Graphics;Photography;
EOF
chmod 644 "$DESKTOP_DIR/$APP_NAME.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --force "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
fi

if [[ "$MODE" == "update" ]]; then
    printf 'Updated Image Sorter. Launch it from your application menu or run: %s\n' "$BIN_HOME/$APP_NAME"
else
    printf 'Installed Image Sorter. Launch it from your application menu or run: %s\n' "$BIN_HOME/$APP_NAME"
fi
