# Image Sorter - Agent Notes

## Objective
Rust (egui/eframe) Linux GUI image sorter (target: Solus OS). Features: recursive folder scan, RAW+JPEG pairing (the JPEG is displayed), grid view grouped by subfolder, image viewer with keep/leave/delete and keyboard shortcuts, Ctrl+Z undo, zoom/pan, EXIF display, settings (keep/delete folders, shortcuts).

## Project layout
- `src/main.rs`: the whole app (~1650 lines).
- `Cargo.toml`: eframe 0.31 (glow/x11/wayland), image 0.25 [png,gif,bmp,tiff,webp], png 0.18, flate2, jpeg-decoder 0.3 (default-features=false), md5, url, kamadak-exif, native-dialog 0.7, serde (+json), walkdir, notify 6. Dev-dep: image with `jpeg` feature. `[profile.dev] opt-level=2`, dependencies opt-level=3.
- `EXIF Tags.html`: ExifTool tag reference (ID -> name table, ~651 unique IDs, rows like `<td title='0x0001 = 1'>0x0001</td><td>Name</td>`). Currently unused; tag names come from kamadak-exif `Tag::description()`. A full HTML-derived mapping is an optional improvement.
- `image_tricks.md`: user-supplied performance guidance (already applied).

## Verification
- `cargo check && cargo test && git diff --check`
- No rustfmt/clippy available (`cargo fmt` fails).
- The GUI has never been run by the assistant; ask the user to restart with `cargo run` and report visual issues.

## Editing notes
- `edit` calls often fail on exact-match/whitespace; read the file region first. Avoid broad `replaceAll`. A small Python script doing string replacement worked well for multi-region edits.

## Architecture
- Background threads: folder scan, keep-folder scan, file operations (keep = copy, delete = move to the configured delete folder preserving the relative path; history kept for undo).
- Prioritized preview worker pool (`PreviewPool`, `TextureKey{path,width,height,fingerprint}`). `VIEW_PREVIEW=(320,240)`; viewer requests sized via `viewer_request_size`; JPEGs decoded with jpeg_decoder `scale`.
- Thumbnails: freedesktop cache (validated by URI/MTime/Size), embedded EXIF thumbnail, own zlib disk cache (only for 320x240).
- Settings (`Settings`, serde default) saved to `$XDG_CONFIG_HOME/image_sorter/settings.json`: keep_folder, delete_folder, shortcuts, `show_exif`, `show_datetime` (default true), `show_filenames` (default true; toolbar checkbox hides filenames in grid cells and the viewer title bar). Settings live in an `egui::Modal`; a missing keep/delete folder opens the modal with a `pending_action`.
- Kept status is reconciled by scanning the keep folder (`scan_keep_folder`, `is_kept`, `refresh_kept_status`, `poll_kept_status`, about every 3s as a fallback). A `notify` watcher on the keep folder (`sync_keep_watcher`, `keep_dirty` flag) triggers a rescan ~300ms after any file is added, removed or renamed there.
- Layout: top toolbar (Open folder, Grid, Date/time + EXIF checkboxes, Keep/Leave/Delete/Undo, Settings). Grid mode has a bottom panel `grid_actions`. Viewer uses `show_inside` panels: `viewer_info` (top), `viewer_actions` (bottom, includes Reset zoom), `viewer_exif` (right, `show_animated_inside`).
- `Photo { display, files, subfolder, datetime: Option<String>, exif: Vec<(exif::Tag, String, String)>, kept }`; the tuple is (tag, human-readable name, value).
- EXIF helpers: `exif_tag_name(tag)` (description, fallback `"Unknown EXIF tag 0xXXXX"`), `read_exif`, `exif_datetime`, `exif_category(tag)` mapping to `EXIF_CATEGORIES` ("Camera and optics", "Date, time and position" (includes GPS), "Image", "Others"), and `show_exif_fields` (shared by viewer and grid panels; categorized, values over 50 chars get Expand/Collapse via `SorterApp.expanded_exif`, keyed by `path:tag`).
- Grid EXIF side panel (`grid_exif`) is only added when `show_exif` is on, and shows EXIF for a single selected photo. EXIF is lazily loaded when a photo's `exif` is empty.

## Status
All requested features are implemented. Latest round (all done, `cargo check` and 6 tests pass):
- Removed the residual right-hand space in grid view (side panel only exists when EXIF is enabled; the scroll area uses `auto_shrink([false, false])`).
- Removed the tag ID from tag names.
- Added EXIF categories.
- Grid cells show the date/time below the filename in a smaller (10pt) font.
- The root-level subfolder title shows the root folder's name instead of ".".
- Subfolder titles are bold, 20pt, strong (white-ish) text with a separator. A smaller (13pt) date or date range (`date_range_label`, date part of `Photo.datetime`) is right-aligned on the title row.
- Grid cells stretch to fill the window width (`cell_width`, min 160px, 4:3 thumbnails). Esc in the viewer, the Grid button, or double-click returns to the grid via `show_grid()`, which selects the last viewed photo and scrolls to it (`scroll_to_selection`).
- Viewer image is painted with `painter().with_clip_rect(rect)` (rect = free area left after the inner info/actions/EXIF panels) so a zoomed image no longer draws over them.

## Known caveats / possible follow-ups
- Photos without any EXIF data are re-read each frame while `show_exif` is on and `exif` is empty (cheap failure, but could be guarded with a "loaded" flag).
- Optionally build a fuller tag ID -> name table from `EXIF Tags.html` if kamadak-exif names are insufficient (many MakerNote/unknown tags show as "Unknown EXIF tag 0xXXXX" and fall in "Others").
