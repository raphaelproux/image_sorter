I'll research this across several angles in parallel: Gwenview/Dolphin internals, the thumbnail spec, and fast decoding techniques.

I have enough for the main report, but Gwenview's own source (how it loads the full view) would add a lot, so I'll check it now.# Report: Making image display fast in a Linux photo app (what Gwenview, Dolphin and libvips do)

**Sources:** Gwenview source (`loadingdocumentimpl.cpp`, `thumbnailgenerator.cpp`), the KIO `filepreviewjob.cpp`, the freedesktop thumbnail spec, the Qt docs, libjpeg-turbo and libvips docs.
**Caveat:** The fetched Gwenview files were truncated, so I only saw the beginning of `loadingdocumentimpl.cpp` and of `thumbnailgenerator.cpp`. Items marked *[from my own knowledge]* are not confirmed by the pages I read.

---

## 1. Summary: the 8 biggest levers

1. **Never decode a full-resolution image just to show a thumbnail.** Use decode-time downscaling (JPEG DCT scaling, `QImageReader::setScaledSize`, `vips_thumbnail`).
2. **Use the embedded EXIF thumbnail** for JPEG and RAW files when it is large enough.
3. **Use the shared freedesktop thumbnail cache** (`~/.cache/thumbnails`). Dolphin and Gwenview already populate it, so your app gets hits for free.
4. **Decode off the UI thread**, with cancellable jobs and a bounded thread pool.
5. **Show a low-resolution image first in the large view**, then replace it with the full-resolution one.
6. **Prefetch the neighbouring images** and keep decoded results in an LRU cache with a memory limit.
7. **Read metadata and size without decoding pixels** (`QImageReader::size()`, a header-only read).
8. **Give the decoder a format hint** so it does not probe every plugin.

---

## 2. Thumbnails in the folder grid

### 2.1 freedesktop Thumbnail Managing Standard ([spec](https://specifications.freedesktop.org/thumbnail/latest-single/))
- **Location:** `$XDG_CACHE_HOME/thumbnails/` (default `~/.cache/thumbnails`).
- **Size directories:** `normal` (128px), `large` (256px), `x-large` (512px), `xx-large` (1024px), plus `fail/`.
  - The x-large and xx-large sizes were added in spec v0.9.0 (2020).
- **File name:** `md5(canonical file:// URI).png`. The hash is of the URI string, not of the file contents.
- **PNG text metadata:**
  - `Thumb::URI`
  - `Thumb::MTime` (must verify)
  - `Thumb::Size` (should verify)
  - `Thumb::Mimetype` (optional)
- **Validity check:** compare the stored `MTime` and `Size` with the current `stat` of the original file.
- **Writing:** write to a temporary file, then `rename()` it, so readers never see a partial file.
- **Fail directory:** store markers for files that could not be thumbnailed, so you do not retry them on every folder open.
- **Using other sizes:** load the next larger cached size and downscale it. Do not regenerate from the original.
- **Agent action:** look up the cache before decoding anything. Pick the smallest size that is at least the display size. On a miss, generate, save, and display.

### 2.2 How Dolphin does it (KIO `PreviewJob` / `FilePreviewJob`, [source](https://github.com/KDE/kio/blob/master/src/gui/filepreviewjob.cpp))
- It stats the file, builds the MD5 thumbnail name, and loads the cached PNG with `QtConcurrent::run(ThumbnailCache::load, ...)`, so the load is off the main thread.
- It validates the cache (`isCacheValid`: mtime, size, size tier, device pixel ratio, thumbnailer version).
- On a miss, it runs a thumbnailer plugin selected by MIME type, through the `thumbnail` KIO worker.
- It uses a 5 s single-shot timeout per job so a slow file does not block the queue.
- Plugins can opt out of caching (`CacheThumbnail`).
- For remote files, it copies them locally or uses KIOFuse first.
- It takes the device pixel ratio into account when choosing the cache tier.
- Docs: [KIO::PreviewJob](https://api.kde.org/kio-previewjob.html), [thumbnail worker](https://docs.kde.org/stable_kf6/en/kio-extras/kioworker6/thumbnail/).
- **Agent action:** follow the same pipeline of cache lookup, validate, generate, save, emit. Run each stage asynchronously and add per-job timeouts.

### 2.3 How Gwenview generates thumbnails (`ThumbnailContext::load`, [source](https://github.com/KDE/gwenview/blob/master/lib/thumbnailprovider/thumbnailgenerator.cpp))
In order of preference:

1. **RAW files:** use the embedded preview via KDcraw (`loadEmbeddedPreview`). If that fails, use `loadHalfPreview`. Otherwise, for Qt's `raw` handler, use `reader.setQuality(1)` for a faster, half-size decode.
2. **JPEG files:** read the EXIF-embedded thumbnail through its `JpegContent`. It is used if its longest side is ≥ the requested size.
   - In "low resource usage mode" it accepts any embedded thumbnail, even a smaller one, because it is faster ([docs](https://docs.kde.org/trunk_kf6/en/gwenview/gwenview/configadvanced.html)).
3. **Otherwise:** decode with `QImageReader`.
   - Call `reader.size()` first, which is cheap and header-only.
   - If `reader.supportsOption(QImageIOHandler::ScaledSize)` is true and the image is larger than the target, compute the aspect-preserving size and call `reader.setScaledSize()`. The decoder then scales while decoding.
   - Call `reader.setAutoTransform(true)` for EXIF orientation.
   - Do a final `QImage::scaled(...)` only if the decoder did not already scale.
4. **Small images:** if the original is already ≤ the thumbnail size, use it directly. PNGs are not written to the cache in this case.
5. **Thread model:** a dedicated `ThumbnailGenerator` QThread waits on a condition variable. Work is handed over under a mutex, with a cancel flag checked between steps, and it is cancelled and joined on `aboutToQuit`.

**Agent action:** copy this decision order exactly. The two big wins are the embedded thumbnail and scaled decoding.

---

## 3. Decode-time downscaling (the main speed trick)

### 3.1 libjpeg-turbo ([README](https://github.com/libjpeg-turbo/libjpeg-turbo/blob/main/README.md))
- It supports IDCT scaling in the decompressor via `cinfo.scale_num / scale_denom`.
  - Supported factors include 1/8, 1/4, 3/8, 1/2, 5/8, 3/4, 7/8 and others.
  - Only 1/4 and 1/2 are SIMD-accelerated.
- Scaling during the IDCT skips most of the decode work and memory.
- Set the scale before `jpeg_start_decompress()`.
- **Do not rely on DCT scaling in the compressor.** It is silently ignored.
- **Practical rule** ([libvips shrinking guide](https://github.com/libvips/libvips/wiki/HOWTO----Image-shrinking)): use JPEG shrink-on-load to get to about 2x the final size, then do a better-quality resize. A bare block shrink to the exact size causes aliasing.

### 3.2 Qt `QImageReader` ([docs](https://doc.qt.io/qt-6/qimagereader.html))
- `setScaledSize()` and `setClipRect()` / `setScaledClipRect()` can "save memory and speed up loading", depending on the format's support.
- If the format does not support scaling, Qt falls back to `QImage::scale()` with smooth scaling, which is slower ([Qt 6.11 docs](https://doc.qt.io/qt-6.11/qimagereader.html)).
- `setQuality()` trades quality for speed in some handlers.
- `read(QImage*)` can reuse an existing `QImage` of the same format and size, which saves allocation when loading many images of equal size.
- `size()` returns the image size without reading the content.
- Gwenview also passes a format hint (the MIME `preferredSuffix`) so the reader tries the matching plugin first. This also avoids misdetection bugs.
- **Agent action:** always do `QImageReader r(path); r.setAutoTransform(true); r.size(); r.setScaledSize(...)`. Check `supportsOption(ScaledSize)`. Never use `QPixmap(path)` or `QImage(path)` for large images.

### 3.3 libvips ([developer checklist](https://kleisauke.github.io/libvips/API/current/developer-checklist.html), [shrinking guide](https://github.com/libvips/libvips/wiki/HOWTO----Image-shrinking))
- **`vips_thumbnail()`** combines load and resize and uses shrink-on-load. In the libvips docs' own test, thumbnailing a JPEG took 0.08 s and 68 MB, against 0.23 s and 124 MB for load plus resize.
- **`vips_thumbnail_image()`** gets no shrink-on-load, so it is not faster than `vips_resize()`.
- **Shrink-on-load formats:**
  - JPEG (1/2, 1/4 and 1/8).
  - WebP (a general scale).
  - Vector formats (SVG and PDF): render directly at the target size.
- **Streaming:** libvips is demand-driven and threaded, with low memory use.
- **Speed comparison:** in one GitHub discussion libvips was about 17x faster than ImageMagick 6 on a thumbnail test, and about 2–3x faster than the alternatives in another user's workload ([discussion](https://github.com/libvips/libvips/discussions/4710)).
- **Agent action:** if you are not tied to Qt, libvips is a strong option for both thumbnails and big-image decode.

---

## 4. The large (single-image) view

*Gwenview's loader structure, from the part of `loadingdocumentimpl.cpp` I could read ([source](https://github.com/KDE/gwenview/blob/master/lib/document/loadingdocumentimpl.cpp)):*
- **Header sniffing:** it reads only the first 256 bytes (`HEADER_SIZE`) to determine the MIME type and "kind" (raster, SVG, video). It does not decode anything at this stage.
- **Staged loading:**
  1. Load metadata (`loadMetaInfo`) in a `QtConcurrent` job. This reads the Exiv2 metadata, image size and format, with a format hint.
  2. Then load the image data in a separate async job (`loadImageData`).
- **Downsampled loading:** there is an `mImageDataInvertedZoom` field ("load an image at zoom = 1/N") and an `mDownSampledImageLoaded` flag, so a reduced-size image can be loaded first and the full one later.
- **RAW files:** it uses the embedded preview (`KDcraw::loadEmbeddedPreview`).
- **Results:** futures plus `QFutureWatcher` deliver results to the UI thread.

*[From my own knowledge, not confirmed in the fetched excerpt]* Gwenview also keeps recently used documents in a cache and preloads the adjacent image in the list.

**Techniques to implement:**
1. **Instant first paint:** show the cached thumbnail (for example the 256 or 512px one) scaled up immediately. Decode the screen-sized version next, and the full image only if the user zooms in.
2. **Decode to the screen size.** Use `setScaledSize` for the viewport size times the device pixel ratio. A 24 MP JPEG shown on a 1920×1080 screen needs no more than the display size. Decode full resolution lazily, only when zooming to 100%.
3. **Tile or clip for very large images.** `setScaledClipRect` and `setClipRect` allow region-of-interest decoding where the format supports it.
4. **Prefetch the next and previous N images** (for example ±2) at screen size, in low-priority threads. Cancel the prefetches when the user jumps elsewhere.
5. **Bounded LRU cache of decoded images.** `QPixmapCache` defaults to 10 MB and is main-thread only ([docs](https://doc.qt.io/qt-6/qpixmapcache.html)). For big images use your own `QCache<QString, QImage>` with a cost in bytes (width × height × 4). Raise the limit to hundreds of MB if you want smooth browsing.
6. **Keep metadata separate.** Read EXIF, orientation and size on a separate path so it does not delay the first paint.

---

## 5. Concurrency and responsiveness
- **Never decode on the UI thread.** Use `QtConcurrent::run` with `QFutureWatcher` (as Gwenview and KIO do) or a dedicated worker thread with a condition variable.
- **Cancellation:** check a cancel flag between steps. When the user scrolls or switches folders, drop the stale jobs.
- **Priorities:** order jobs by visibility, with visible thumbnails first, then prefetch.
- **Bounded concurrency:** about the number of cores for decoding. Disk I/O on a spinning disk may prefer fewer threads.
- **Timeouts:** KIO uses 5 s per preview, so one pathological file cannot stall the rest.
- **Batch UI updates:** emit results as they arrive and update only the affected cells.
- **Agent action:** use a priority queue with cancellation tokens, and update items incrementally.

---

## 6. Other optimisations
- **Image format plugins:** Dolphin's thumbnails depend on separate packages for formats such as `kimageformats`, `qt6-imageformats` and `kdegraphics-thumbnailers` ([Arch wiki](https://wiki.archlinux.org/title/Dolphin)). Make sure you use the fast libjpeg-turbo-backed handler and not a slow generic one.
- **RAW:** use the embedded JPEG preview first, then a half-size decode, and the full decode only on demand.
- **HEIC, AVIF and similar:** these decode slowly. Rely heavily on cached thumbnails and embedded previews.
- **Remote or FUSE files:** copy locally first, as KIO does, and do not thumbnail remote directories automatically.
- **Directory listing:** do not stat or decode everything up front. Show the names immediately and fill in thumbnails lazily for the visible range.
- **Orientation:** apply it via `setAutoTransform(true)` during decode and cache the already rotated result.

---

## 7. Profiling checklist for your app
1. Time each stage separately: file open, header read, decode, scale, convert to pixmap, and paint.
2. Check that no stage runs full-size decode for a thumbnail. Compare the thumbnail time against `vipsthumbnail` or against Gwenview on the same folder.
3. Check the `QImage` → `QPixmap` conversion. Do it once, off the paint path, or draw `QImage` directly.
4. Check that the thumbnail cache is hit on the second open of a folder.
5. Check that scrolling fast does not queue thousands of obsolete jobs.
6. Check memory growth with a cache limit set.
7. Test with large (24+ MP) JPEGs, PNGs and RAW files, because they behave differently.

---

## 8. Suggested implementation order (highest impact first)
1. Move all decoding off the UI thread, with cancellation.
2. Switch to `QImageReader` with `setScaledSize` and `setAutoTransform`, or to `vips_thumbnail`.
3. Add the embedded EXIF thumbnail path for JPEG and RAW files.
4. Read and write the freedesktop thumbnail cache.
5. Use the thumbnail as the instant placeholder in the large view, then decode at screen size.
6. Add prefetching of neighbours plus an LRU cache with a byte limit.
7. Add visible-first prioritisation and fail markers.

---

## 9. Gaps
- I did not find Gwenview's exact rules for prefetching neighbouring images or for its document cache. Read `lib/document/documentfactory.cpp` and the viewer code on GitHub to confirm.
- I could not read the rest of `loadImageData`, so how Gwenview chooses the downsample factor is unconfirmed.
- I found no benchmark numbers for Dolphin or Gwenview themselves. The only figures above are libvips' own.
