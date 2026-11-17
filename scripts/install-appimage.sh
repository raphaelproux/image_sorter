#!/usr/bin/env bash
set -euo pipefail

APP_NAME="image_sorter"
MODE="install"
if [[ "${1:-}" == "--update" ]]; then
    MODE="update"
    shift
fi
if (( $# > 1 )); then
    printf 'Usage: %s [--update] [/path/to/image_sorter.AppImage]\n' "$0" >&2
    exit 1
fi
APPIMAGE_PATH="${1:-}"

if [[ -z "$APPIMAGE_PATH" ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
    APPIMAGE_PATH="$(find "$REPO_ROOT/dist" -maxdepth 1 -type f -name 'image_sorter-*.AppImage' -print -quit 2>/dev/null || true)"
fi

if [[ -z "$APPIMAGE_PATH" || ! -f "$APPIMAGE_PATH" ]]; then
    printf 'Usage: %s /path/to/image_sorter.AppImage\n' "$0" >&2
    printf 'Or build the AppImage first with scripts/build-appimage.sh.\n' >&2
    exit 1
fi

APPIMAGE_PATH="$(realpath "$APPIMAGE_PATH")"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_HOME="$HOME/.local/bin"
APP_DIR="$HOME/.local/opt/$APP_NAME"
ICON_DIR="$DATA_HOME/icons/hicolor/512x512/apps"
DESKTOP_DIR="$DATA_HOME/applications"
if [[ "$MODE" == "update" && ! -f "$APP_DIR/$APP_NAME.AppImage" ]]; then
    printf 'Image Sorter is not installed at %s; run this script without --update first.\n' "$APP_DIR/$APP_NAME.AppImage" >&2
    exit 1
fi
EXTRACT_DIR="$(mktemp -d)"
trap 'rm -rf "$EXTRACT_DIR"' EXIT

mkdir -p "$APP_DIR" "$BIN_HOME" "$ICON_DIR" "$DESKTOP_DIR"
install -m755 "$APPIMAGE_PATH" "$APP_DIR/$APP_NAME.AppImage"
ln -sfn "$APP_DIR/$APP_NAME.AppImage" "$BIN_HOME/$APP_NAME"

(
    cd "$EXTRACT_DIR"
    "$APPIMAGE_PATH" --appimage-extract "usr/share/icons/hicolor/512x512/apps/$APP_NAME.png" >/dev/null
)
ICON_SOURCE="$EXTRACT_DIR/squashfs-root/usr/share/icons/hicolor/512x512/apps/$APP_NAME.png"
if [[ ! -f "$ICON_SOURCE" ]]; then
    printf 'The AppImage does not contain the expected app icon.\n' >&2
    exit 1
fi
install -m644 "$ICON_SOURCE" "$ICON_DIR/$APP_NAME.png"

cat > "$DESKTOP_DIR/$APP_NAME.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Image Sorter
Exec="$BIN_HOME/$APP_NAME" %U
TryExec=$BIN_HOME/$APP_NAME
Icon=$APP_NAME
StartupWMClass=$APP_NAME
StartupNotify=true
Terminal=false
Categories=Graphics;Photography;
EOF
chmod 644 "$DESKTOP_DIR/$APP_NAME.desktop"

if command -v xdg-icon-resource >/dev/null 2>&1; then
    xdg-icon-resource install --mode user --context apps --size 512 "$ICON_DIR/$APP_NAME.png" "$APP_NAME" >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --force "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
fi
if command -v xdg-desktop-menu >/dev/null 2>&1; then
    xdg-desktop-menu install --mode user --novendor "$DESKTOP_DIR/$APP_NAME.desktop" >/dev/null 2>&1 || true
    xdg-desktop-menu forceupdate --mode user >/dev/null 2>&1 || true
fi
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
    kbuildsycoca5 --noincremental >/dev/null 2>&1 || true
fi

if [[ "$MODE" == "update" ]]; then
    printf 'Updated Image Sorter. Launch it from your application menu or run: %s\n' "$BIN_HOME/$APP_NAME"
else
    printf 'Installed Image Sorter. Launch it from your application menu or run: %s\n' "$BIN_HOME/$APP_NAME"
fi
printf 'Ensure %s is in PATH to launch it by command name.\n' "$BIN_HOME"
