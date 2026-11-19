# Image Sorter - Agent Notes

## Objective
Rust (egui/eframe) Linux GUI image sorter (target: Solus OS). Features: recursive folder scan, RAW+JPEG pairing (the JPEG is displayed), grid view grouped by subfolder, image viewer with keep/leave/delete and keyboard shortcuts, Ctrl+Z undo, zoom/pan, EXIF display, settings (keep/delete folders, shortcuts).

## Project layout
- `src/main.rs`: the app. `src/grid.rs`: pure grid geometry (`GridLayout`, `group_ranges`) with unit tests.
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

## Performance strategy (thousands of photos)
- Grid is virtualized: `show_grid_rows` uses `ScrollArea::show_viewport`; every row has a fixed height (`GridLayout`, cached per `photos_version`/width/toggles), so only intersecting rows are painted (painter calls + `ui.interact`, no widgets). Any code that mutates `photos` (add/remove/reorder) must call `photos_changed()`.
- `stream_grid_neighbours` requests thumbnails for the viewport (priority 50), then ~3 screens ahead in the scroll direction / 1 behind (30/20), capped at `GRID_PREFETCH_LIMIT`; queued thumbnails that left the window are cancelled (`retain_grid_thumbnails`).
- `enqueue_preview` promotes already-waiting jobs; workers pick the best job with `take_best_job` (no sort per enqueue).
- Textures: `HashMap<PathBuf, Vec<TextureEntry>>` + running `texture_bytes` (LRU eviction), at most `MAX_UPLOADS_PER_FRAME` uploads per frame; failed decodes are remembered in `failed_textures` (no retry loop).
- Dates: no more full EXIF read of every photo at open. `DatetimeScan` (3 threads) reads only the date from the first 128 KB of each JPEG, visible photos first (`prioritize_datetimes`); grid shows "…" until loaded (`Photo.datetime_loaded`). Full EXIF is still lazy (`request_exif`).
- `photo_index` (path -> index) replaces O(N) lookups; date range labels per folder are cached in `GridLayout.ranges` keyed by `dates_version`.

- RW2 without a JPEG: `rw2_locate`/`rw2_embedded_jpeg` read the embedded full-size JPEG (IFD0 tag 0x2E, orientation 0x112) by byte range; `decode_preview` and `viewer_decode_size` use it, and `parse_exif` patches the RW2 magic (0x55 -> 0x2A) so EXIF/date work. `system_thumbnail_at` still fails on `zTXt`-compressed URIs (harmless now for RW2).
- Full-res RAW: `decode_raw` develops the file with `imagepipe` (rawloader + demosaic/WB/sRGB; max 2 concurrent via `RawSlot`, side capped at `MAX_RAW_SIDE`). RW2 grid thumbnails and fit-to-window views keep using the embedded JPEG; the RAW is developed only when a bigger size is requested (zoom, `rw2_needs_raw`). `viewer_decode_size` returns the RAW size (`raw_dimensions`, via `decode_dummy`, ~12 ms). Other RAW formats (ARW, CR2, NEF...) go through `decode_raw` directly (untested: no samples).

## Leak/slowdown fixes (latest round)
- `clear_textures` also empties the preview queue (`PreviewPool::clear_jobs`) and resets `grid_window`; opening a folder also clears `history`, `grid_layout`, `expanded_exif`. History is capped (`MAX_HISTORY`).
- Preview/date workers wrap decoding in `catch_unwind` (a panic used to kill a worker permanently, shrinking the pool). Superseded full-size viewer requests (zoom steps) are dropped in `enqueue_preview`.
- Disk preview cache: size is measured at startup; trimming goes down to 75% and only one trimmer runs (`CACHE_TRIMMING`).
- Keep scan: no stacked scan threads; `is_kept` takes a precomputed `same_tree` flag (no per-photo `canonicalize`, costly on SMB).
- Keep copy uses `copy_file` (plain read/write loop) instead of `fs::copy`, which fails with ENOTTY ("Inappropriate ioctl", os error 25) on CIFS/Samba. `move_file` fallback also uses it.

## Flat keep + grid size (latest round)
- Keep copies files flat into the keep folder (no subfolders). `process_files` precomputes one clash suffix per photo (`_1`, `_2`...) via `suffixed_name`, shared by its RAW+JPEG so pair names stay aligned. Delete still preserves the relative path.
- Kept status: `scan_keep_folder` returns `KeepFiles` (file name -> sizes, suffixed copies also registered under the original name via `unsuffixed_name`); `is_kept(photo, &keep_files)` matches name + size of any of the photo's files.
- Toolbar slider (grid mode only) sets `Settings.grid_cell_size` (80..=320, default 160, saved) -> `GridMetrics.min_cell`; 320 max because thumbnails are decoded at 320x240.

## Operation completion
- `poll_operations` no longer yanks the user back: `action_start` (current photo when the operation began) and `action_next` are compared with the photo being viewed on completion. If the user moved on, `current`, `mode` and the selection are kept (re-resolved by path via `photo_index`, since a delete shifts indices); otherwise it advances to `action_next` as before. `action_origin` was removed.

## Grid scrolling
- `show_grid_rows` multiplies `smooth_scroll_delta.y` by `ScrollBoost::factor` while the pointer is over the grid: `GRID_SCROLL_BASE` (3x) ramping to `GRID_SCROLL_MAX` (12x) over `GRID_SCROLL_RAMP` of continuous scrolling; resets after a `GRID_SCROLL_GAP` pause or a direction change.

## Known caveats / possible follow-ups
- Photos without any EXIF data are re-read each frame while `show_exif` is on and `exif` is empty (cheap failure, but could be guarded with a "loaded" flag).
- Optionally build a fuller tag ID -> name table from `EXIF Tags.html` if kamadak-exif names are insufficient (many MakerNote/unknown tags show as "Unknown EXIF tag 0xXXXX" and fall in "Others").
