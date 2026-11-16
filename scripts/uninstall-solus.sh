#!/usr/bin/env bash
set -euo pipefail

APP_NAME="image_sorter"
BIN_HOME="${BIN_HOME:-$HOME/.local/bin}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_DIR="$DATA_HOME/icons/hicolor/512x512/apps"
DESKTOP_DIR="$DATA_HOME/applications"

rm -f "$BIN_HOME/$APP_NAME" \
    "$ICON_DIR/$APP_NAME.png" \
    "$DESKTOP_DIR/$APP_NAME.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --force "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true
fi

printf 'Uninstalled Image Sorter. User settings and cache were left untouched.\n'
