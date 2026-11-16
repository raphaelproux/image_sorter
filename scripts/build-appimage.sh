#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="image_sorter"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -n 1)"
ARCH="$(uname -m)"

if [[ "$ARCH" != "x86_64" ]]; then
    printf 'AppImage packaging currently supports x86_64 only; found %s\n' "$ARCH" >&2
    exit 1
fi

OUTPUT_DIR="${OUTPUT_DIR:-$ROOT_DIR/dist}"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
APPDIR="$WORK_DIR/${APP_NAME}.AppDir"
TOOLS_DIR="$WORK_DIR/tools"
ARTIFACTS_DIR="$WORK_DIR/artifacts"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/512x512/apps" "$TOOLS_DIR" "$ARTIFACTS_DIR" "$OUTPUT_DIR"

cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"
install -m755 "$ROOT_DIR/target/release/$APP_NAME" "$APPDIR/usr/bin/$APP_NAME"
install -m644 "$ROOT_DIR/image_sorter.desktop" "$APPDIR/usr/share/applications/image_sorter.desktop"
magick "$ROOT_DIR/icon/icon.png" -resize 512x512 "$APPDIR/usr/share/icons/hicolor/512x512/apps/image_sorter.png"

fetch_tool() {
    local variable="$1"
    local filename="$2"
    local url="$3"
    local configured="${!variable:-}"
    if [[ -n "$configured" ]]; then
        printf '%s\n' "$configured"
        return
    fi
    if command -v "$filename" >/dev/null 2>&1; then
        command -v "$filename"
        return
    fi
    local path="$TOOLS_DIR/$filename"
    curl --fail --location --silent --show-error "$url" --output "$path"
    chmod +x "$path"
    printf '%s\n' "$path"
}

LINUXDEPLOY_BIN="$(fetch_tool LINUXDEPLOY linuxdeploy "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage")"
APPIMAGETOOL_BIN="$(fetch_tool APPIMAGETOOL appimagetool "https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage")"

cd "$ARTIFACTS_DIR"
APPIMAGE_EXTRACT_AND_RUN=1 APPIMAGETOOL="$APPIMAGETOOL_BIN" "$LINUXDEPLOY_BIN" \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/$APP_NAME" \
    --desktop-file "$APPDIR/usr/share/applications/image_sorter.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/512x512/apps/image_sorter.png" \
    --output appimage

shopt -s nullglob
GENERATED_IMAGES=("$ARTIFACTS_DIR"/*.AppImage)
if (( ${#GENERATED_IMAGES[@]} != 1 )); then
    printf 'Expected linuxdeploy to produce one AppImage; found %s\n' "${#GENERATED_IMAGES[@]}" >&2
    exit 1
fi
FINAL="$OUTPUT_DIR/${APP_NAME}-${VERSION}-x86_64.AppImage"
mv "${GENERATED_IMAGES[0]}" "$FINAL"
chmod +x "$FINAL"
printf 'Created %s\n' "$FINAL"
