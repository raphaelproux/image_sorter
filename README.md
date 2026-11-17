# Image Sorter

A lightweight Linux app (Rust, egui/eframe) to explore and sort images in folders. It targets Solus OS but runs on any Linux with X11 or Wayland.

## Features

- Recursive folder scan, with images grouped by subfolder in a grid view.
- RAW + JPEG pairing: files sharing the same name are treated as one photo and the JPEG is displayed. Supported formats:
  - Images: JPEG, PNG, GIF, BMP, TIFF, WebP.
  - RAW: ARW, CR2, CR3, DNG, NEF, NRW, ORF, PEF, RAF, RW2, SRW.
- Image viewer with zoom (mouse wheel), pan (drag) and navigation between photos.
- Sorting actions on one or several selected photos:
  - **Keep**: copies the photo (and its paired RAW) to the keep folder, keeping the relative path.
  - **Leave**: moves on to the next photo without doing anything.
  - **Delete**: moves the photo to the delete folder, keeping the relative path (nothing is permanently erased).
  - **Undo** (Ctrl+Z) reverts the last keep/delete operation.
- Photos already copied to the keep folder are marked "Kept". The keep folder is watched, so the status stays in sync even when files change outside the app.
- Optional date/time, filename and EXIF display (grouped by category: camera and optics, date/time/position, image, others).
- Settings (keep/delete folders, keyboard shortcuts) are saved in `$XDG_CONFIG_HOME/image_sorter/settings.json`.

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| `k` | Keep (configurable) |
| `l` | Leave (configurable) |
| `Backspace` | Delete (configurable) |
| Arrow keys | Move the selection in the grid / previous and next photo in the viewer |
| `Shift` + click or arrows | Extend the selection (grid) |
| `Ctrl` + click | Add to / remove from the selection (grid) |
| Double-click, `Enter` or `Space` | Open the selected photo in the viewer (double-click also goes back to the grid) |
| `Esc` or `Space` | Back to the grid from the viewer |
| `Ctrl+Z` | Undo |
| `Ctrl+,` | Open the settings |

## Network drives and performance

Browsing folders on slow storage (SMB, kio-fuse, ...) is designed to keep the interface responsive:

- The folder scan only lists files. It does not open them, and the photo grid appears as soon as it finishes.
- EXIF data, image sizes and keep-folder status are loaded in background threads, so dates and EXIF details fill in progressively.
- Each file is read once (a single large read of its first bytes, kept in a small memory cache) and reused for EXIF, size and thumbnail extraction.
- Previews are decoded by a pool of worker threads, prioritised by what is on screen. The embedded EXIF thumbnail, or the freedesktop thumbnail cache, is used when available.
- Decoded previews are cached on disk in `$XDG_CACHE_HOME/image_sorter/previews`, validated by file size and modification time.
- In the viewer, the next 3 and previous 2 photos are preloaded at display size, so navigation feels instant. Preloading pauses while zoomed in and drops photos you have moved past.
- A spinner is shown while a folder is being scanned or a large image is loading.

## Build and run

Requirements: Rust (stable) and Cargo.

```sh
cargo run --release
```

Run the checks with:

```sh
cargo check && cargo test
```

## Installation

- **Solus OS** (builds from source and installs into `~/.local`):

  ```sh
  scripts/install-solus.sh            # first install
  scripts/install-solus.sh --update   # update
  scripts/uninstall-solus.sh          # remove
  ```

- **AppImage** (x86_64; requires ImageMagick):

  ```sh
  scripts/build-appimage.sh           # builds into dist/
  scripts/install-appimage.sh [--update] [/path/to/image_sorter.AppImage]
  scripts/uninstall-appimage.sh       # remove
  ```
