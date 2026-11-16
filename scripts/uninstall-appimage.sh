#!/usr/bin/env bash
set -euo pipefail

APP_NAME="image_sorter"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
APP_DIR="$HOME/.local/opt/$APP_NAME"
LAUNCHER="$HOME/.local/bin/$APP_NAME"
DESKTOP_FILE="$DATA_HOME/applications/$APP_NAME.desktop"
ICON_FILE="$DATA_HOME/icons/hicolor/512x512/apps/$APP_NAME.png"

if [[ -f "$DESKTOP_FILE" ]] && command -v xdg-desktop-menu >/dev/null 2>&1; then
    xdg-desktop-menu uninstall --mode user "$DESKTOP_FILE" >/dev/null 2>&1 || true
fi
if command -v xdg-icon-resource >/dev/null 2>&1; then
    xdg-icon-resource uninstall --mode user --context apps --size 512 "$APP_NAME" >/dev/null 2>&1 || true
fi

rm -f "$LAUNCHER" "$DESKTOP_FILE" "$ICON_FILE"
rm -f "$HOME/.local/share/applications/$APP_NAME.desktop"
rm -f "$HOME/.local/share/icons/hicolor/512x512/apps/$APP_NAME.png"
rm -rf "$APP_DIR"

DESKTOP_DIR="$(dirname "$DESKTOP_FILE")"
if [[ -d "$DESKTOP_DIR" ]] && command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
fi
if [[ -d "$DATA_HOME/icons/hicolor" ]] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --force "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
fi
if command -v xdg-desktop-menu >/dev/null 2>&1; then
    xdg-desktop-menu forceupdate --mode user >/dev/null 2>&1 || true
fi
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
    kbuildsycoca5 --noincremental >/dev/null 2>&1 || true
fi

printf 'Uninstalled Image Sorter. App settings and sorted images were left untouched.\n'
