mod grid;

use eframe::egui::{
    self, ColorImage, Key, Modifiers, PointerButton, TextureHandle, TextureOptions, Vec2,
};
use flate2::read::ZlibDecoder;
use grid::{group_ranges, GridItemKind, GridLayout, GridMetrics};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::{ImageDecoder, ImageReader};
use jpeg_decoder::{Decoder as JpegDecoder, PixelFormat as JpegPixelFormat};
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::UNIX_EPOCH;
use url::Url;
use walkdir::WalkDir;

const RAW_EXTENSIONS: &[&str] = &[
    "arw", "cr2", "cr3", "dng", "nef", "nrw", "orf", "pef", "raf", "rw2", "srw",
];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp"];
const MAX_TEXTURE_BYTES: usize = 256 * 1024 * 1024;
const MAX_TEXTURE_MEMORY: usize = 512 * 1024 * 1024;
const VIEW_PREVIEW: (u32, u32) = (320, 240);
const ACTIVE_VIEW_PREVIEW_PRIORITY: u8 = 250;
const PREFETCH_PRIORITY: u8 = 80;
const PREFETCH_MIN_PRIORITY: u8 = 60;
const PREFETCH_AHEAD: usize = 3;
const PREFETCH_BEHIND: usize = 2;
const ACTIVE_VIEW_IMAGE_PRIORITY: u8 = 240;
const MAX_TEXTURE_CACHE_BYTES: usize = 256 * 1024 * 1024;
const MAX_PREVIEW_QUEUE: usize = 512;
const GRID_VISIBLE_PRIORITY: u8 = 50;
const GRID_NEAR_PRIORITY: u8 = 30;
const DATETIME_WORKERS: usize = 3;
const MAX_UPLOADS_PER_FRAME: usize = 6;
const GRID_PREFETCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const GRID_PREFETCH_LIMIT: usize = 400;
const KEEP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Debug)]
struct Photo {
    display: PathBuf,
    files: Vec<PathBuf>,
    subfolder: PathBuf,
    datetime: Option<String>,
    exif: Vec<(exif::Tag, String, String)>,
    exif_loaded: bool,
    datetime_loaded: bool,
    kept: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ActionKind {
    Keep,
    Delete,
}

#[derive(Clone, Debug)]
struct FileMove {
    original: PathBuf,
    destination: PathBuf,
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    kind: ActionKind,
    files: Vec<FileMove>,
    photos: Vec<Photo>,
    kept_before: HashSet<PathBuf>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TextureKey {
    path: PathBuf,
    width: u32,
    height: u32,
    fingerprint: u64,
}

enum PreviewOutcome {
    Ready(ColorImage),
    Failed,
    Expired,
}

struct TextureEntry {
    key: TextureKey,
    handle: TextureHandle,
    bytes: usize,
    last_used: u64,
}

type DatetimeBatch = (usize, Vec<(PathBuf, Option<String>)>);

#[derive(Default)]
struct DatetimeState {
    urgent: Vec<PathBuf>,
    background: Vec<PathBuf>,
    taken: HashSet<PathBuf>,
}

/// Background scan reading only the capture date of every photo. Photos near the viewport are
/// served first (`urgent`), the rest follows in folder order.
struct DatetimeScan {
    state: std::sync::Mutex<DatetimeState>,
    cancelled: std::sync::atomic::AtomicBool,
}

impl DatetimeScan {
    fn next(&self) -> Option<PathBuf> {
        let mut state = self.state.lock().ok()?;
        loop {
            let path = state.urgent.pop().or_else(|| state.background.pop())?;
            if state.taken.insert(path.clone()) {
                return Some(path);
            }
        }
    }

    /// `paths` are ordered from the most to the least urgent.
    fn prioritize(&self, mut paths: Vec<PathBuf>) {
        if let Ok(mut state) = self.state.lock() {
            paths.retain(|path| !state.taken.contains(path));
            paths.reverse();
            state.urgent = paths;
        }
    }
}

#[derive(Clone)]
struct PreviewJob {
    key: TextureKey,
    size: (u32, u32),
    priority: u8,
    repaint: egui::Context,
    deadline: std::time::Instant,
}

#[derive(Default)]
struct PreviewQueue {
    jobs: Vec<PreviewJob>,
    active: HashSet<TextureKey>,
}

struct PreviewPool {
    queue: std::sync::Mutex<PreviewQueue>,
    ready: std::sync::Condvar,
    cache_size: AtomicUsize,
}

struct OperationResult {
    kind: ActionKind,
    history: HistoryEntry,
    completed: HashSet<PathBuf>,
    photo_files: Vec<(PathBuf, Vec<PathBuf>)>,
    errors: Vec<String>,
    is_undo: bool,
    restored_photos: Vec<Photo>,
}

enum OperationMessage {
    Progress(String),
    Complete(OperationResult),
}

fn default_show_datetime() -> bool {
    true
}
fn default_show_filenames() -> bool {
    true
}

fn loading_indicator(ui: &mut egui::Ui, rect: egui::Rect, label: &str) {
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                ui.spinner();
                ui.label(label);
            });
        });
    });
}

fn status_text(notice: &str) -> egui::RichText {
    let is_error = [
        "Choose a source folder",
        "The destination folder",
        "Keep and delete destinations",
        "Select at least one image",
        "Undo incomplete:",
        "Some files could not be processed:",
    ]
    .iter()
    .any(|prefix| notice.starts_with(prefix));
    let text = egui::RichText::new(notice);
    if is_error {
        text.strong().color(egui::Color32::RED)
    } else {
        text
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    keep_folder: String,
    delete_folder: String,
    keep_shortcut: String,
    leave_shortcut: String,
    delete_shortcut: String,
    show_exif: bool,
    #[serde(default = "default_show_datetime")]
    show_datetime: bool,
    #[serde(default = "default_show_filenames")]
    show_filenames: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Grid,
    Viewer,
}

struct SorterApp {
    settings: Settings,
    settings_snapshot: Option<Settings>,
    root: Option<PathBuf>,
    settings_open: bool,
    pending_action: Option<ActionKind>,
    keep_scan_receiver: Receiver<(usize, HashSet<PathBuf>)>,
    keep_scan_sender: Sender<(usize, HashSet<PathBuf>)>,
    keep_scan_generation: usize,
    keep_scan_pending: bool,
    next_keep_scan: std::time::Instant,
    keep_watcher: Option<notify::RecommendedWatcher>,
    keep_watch_path: Option<PathBuf>,
    keep_dirty: Arc<std::sync::atomic::AtomicBool>,
    photos: Vec<Photo>,
    selection: HashSet<usize>,
    anchor: Option<usize>,
    current: usize,
    mode: ViewMode,
    textures: HashMap<PathBuf, Vec<TextureEntry>>,
    texture_bytes: usize,
    texture_clock: u64,
    failed_textures: HashSet<TextureKey>,
    photos_version: u64,
    photo_index: HashMap<PathBuf, usize>,
    photo_index_version: u64,
    dates_version: u64,
    grid_layout: Option<GridLayout>,
    grid_window: (usize, usize),
    grid_last_offset: f32,
    grid_scroll_down: bool,
    grid_prefetch_at: std::time::Instant,
    datetime_receiver: Receiver<DatetimeBatch>,
    datetime_sender: Sender<DatetimeBatch>,
    datetime_scan: Option<Arc<DatetimeScan>>,
    history: Vec<HistoryEntry>,
    notice: String,
    zoom: f32,
    pan: Vec2,
    previous_folder: Option<PathBuf>,
    scan_receiver: Receiver<(usize, PathBuf, Vec<Photo>)>,
    scan_sender: Sender<(usize, PathBuf, Vec<Photo>)>,
    exif_receiver: Receiver<(usize, PathBuf, Vec<(exif::Tag, String, String)>)>,
    exif_sender: Sender<(usize, PathBuf, Vec<(exif::Tag, String, String)>)>,
    source_info_receiver: Receiver<(PathBuf, (u32, u32), u64)>,
    source_info_sender: Sender<(PathBuf, (u32, u32), u64)>,
    pending_source_info: HashSet<PathBuf>,
    pending_exif: HashSet<PathBuf>,
    scan_generation: usize,
    texture_receiver: Receiver<(TextureKey, TextureKey, PreviewOutcome)>,
    queued_textures: HashMap<TextureKey, u8>,
    preview_pool: Arc<PreviewPool>,
    operation_sender: Sender<OperationMessage>,
    operation_receiver: Receiver<OperationMessage>,
    operation_pending: bool,
    action_origin: Option<ViewMode>,
    action_next: Option<PathBuf>,
    last_viewer_image: Option<PathBuf>,
    scroll_to_selection: bool,
    grid_columns: usize,
    source_sizes: HashMap<PathBuf, (u32, u32)>,
    texture_fingerprints: HashMap<PathBuf, u64>,
    expanded_exif: HashSet<String>,
}

impl Default for SorterApp {
    fn default() -> Self {
        let settings = fs::read_to_string(config_path())
            .ok()
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or(Settings {
                keep_folder: String::new(),
                delete_folder: String::new(),
                keep_shortcut: "k".into(),
                leave_shortcut: "l".into(),
                delete_shortcut: "Backspace".into(),
                show_exif: false,
                show_datetime: true,
                show_filenames: true,
            });
        let (scan_sender, scan_receiver) = mpsc::channel();
        let (exif_sender, exif_receiver) = mpsc::channel();
        let (source_info_sender, source_info_receiver) = mpsc::channel();
        let (keep_scan_sender, keep_scan_receiver) = mpsc::channel();
        let (texture_sender, texture_receiver) = mpsc::channel();
        let (datetime_sender, datetime_receiver) = mpsc::channel();
        let (operation_sender, operation_receiver) = mpsc::channel();
        let preview_pool = Arc::new(PreviewPool {
            queue: std::sync::Mutex::new(PreviewQueue::default()),
            ready: std::sync::Condvar::new(),
            cache_size: AtomicUsize::new(0),
        });
        let preview_cache = preview_cache_path();
        let worker_count = std::thread::available_parallelism()
            .map_or(4, usize::from)
            .clamp(4, 6);
        for _ in 0..worker_count {
            let pool = Arc::clone(&preview_pool);
            let sender = texture_sender.clone();
            let cache = preview_cache.clone();
            thread::spawn(move || preview_worker(pool, sender, cache));
        }
        Self {
            settings,
            settings_snapshot: None,
            root: None,
            settings_open: false,
            pending_action: None,
            keep_scan_receiver,
            keep_scan_sender,
            keep_scan_generation: 0,
            keep_scan_pending: false,
            next_keep_scan: std::time::Instant::now(),
            keep_watcher: None,
            keep_watch_path: None,
            keep_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            photos: Vec::new(),
            selection: HashSet::new(),
            anchor: None,
            current: 0,
            mode: ViewMode::Grid,
            textures: HashMap::new(),
            texture_bytes: 0,
            texture_clock: 0,
            failed_textures: HashSet::new(),
            photos_version: 0,
            photo_index: HashMap::new(),
            photo_index_version: 0,
            dates_version: 0,
            grid_layout: None,
            grid_window: (usize::MAX, usize::MAX),
            grid_last_offset: 0.0,
            grid_scroll_down: true,
            grid_prefetch_at: std::time::Instant::now(),
            datetime_receiver,
            datetime_sender,
            datetime_scan: None,
            history: Vec::new(),
            notice: "Choose a folder to begin.".into(),
            zoom: 1.0,
            pan: Vec2::ZERO,
            previous_folder: None,
            scan_receiver,
            scan_sender,
            exif_receiver,
            exif_sender,
            source_info_receiver,
            source_info_sender,
            pending_source_info: HashSet::new(),
            pending_exif: HashSet::new(),
            scan_generation: 0,
            texture_receiver,
            queued_textures: HashMap::new(),
            preview_pool,
            operation_sender,
            operation_receiver,
            operation_pending: false,
            action_origin: None,
            action_next: None,
            last_viewer_image: None,
            scroll_to_selection: false,
            grid_columns: 1,
            source_sizes: HashMap::new(),
            texture_fingerprints: HashMap::new(),
            expanded_exif: HashSet::new(),
        }
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("image_sorter")
        .join("settings.json")
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn stem_key(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string()
}

fn move_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(rename_error) => match fs::copy(source, destination) {
            Ok(_) => match fs::remove_file(source) {
                Ok(()) => Ok(()),
                Err(remove_error) => {
                    let _ = fs::remove_file(destination);
                    Err(remove_error)
                }
            },
            Err(_) => Err(rename_error),
        },
    }
}

const HEAD_BYTES: usize = 256 * 1024;
const BYTES_CACHE_BUDGET: usize = 128 * 1024 * 1024;
const BYTES_CACHE_MAX_ENTRY: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct CachedBytes {
    data: Arc<Vec<u8>>,
    complete: bool,
}

static BYTES_CACHE: std::sync::Mutex<Vec<(PathBuf, CachedBytes)>> =
    std::sync::Mutex::new(Vec::new());

fn bytes_cache_get(path: &Path) -> Option<CachedBytes> {
    let mut cache = BYTES_CACHE.lock().ok()?;
    let position = cache.iter().position(|(cached, _)| cached == path)?;
    let entry = cache.remove(position);
    let value = entry.1.clone();
    cache.push(entry);
    Some(value)
}

fn bytes_cache_put(path: &Path, value: &CachedBytes) {
    if value.data.len() > BYTES_CACHE_MAX_ENTRY {
        return;
    }
    if let Ok(mut cache) = BYTES_CACHE.lock() {
        cache.retain(|(cached, _)| cached != path);
        cache.push((path.to_path_buf(), value.clone()));
        let mut total: usize = cache.iter().map(|(_, entry)| entry.data.len()).sum();
        while total > BYTES_CACHE_BUDGET && cache.len() > 1 {
            total -= cache.remove(0).1.data.len();
        }
    }
}

fn bytes_cache_clear() {
    if let Ok(mut cache) = BYTES_CACHE.lock() {
        cache.clear();
    }
}

fn is_jpeg(path: &Path) -> bool {
    matches!(extension(path).as_str(), "jpg" | "jpeg")
}

/// Reads the first bytes of a file with a single open and one large read: network
/// filesystems pay a round trip per open and per small read.
fn load_prefix(path: &Path) -> Option<CachedBytes> {
    if let Some(cached) = bytes_cache_get(path) {
        return Some(cached);
    }
    let mut data = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(HEAD_BYTES as u64)
        .read_to_end(&mut data)
        .ok()?;
    let value = CachedBytes {
        complete: data.len() < HEAD_BYTES,
        data: Arc::new(data),
    };
    bytes_cache_put(path, &value);
    Some(value)
}

fn load_full(path: &Path) -> Option<Arc<Vec<u8>>> {
    if let Some(cached) = bytes_cache_get(path) {
        if cached.complete {
            return Some(cached.data);
        }
    }
    let value = CachedBytes {
        data: Arc::new(fs::read(path).ok()?),
        complete: true,
    };
    bytes_cache_put(path, &value);
    Some(value.data)
}

fn is_rw2(path: &Path) -> bool {
    extension(path) == "rw2"
}

fn is_raw(path: &Path) -> bool {
    RAW_EXTENSIONS.contains(&extension(path).as_str())
}

/// Full-resolution RAW decoding (demosaic, white balance, colour matrix, sRGB curve) is memory
/// hungry (about 0.5 GB for a 16 MP image): allow only a couple at a time.
const MAX_RAW_DECODES: usize = 2;
static RAW_SLOTS: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());
/// Largest side handed to the GPU (typical OpenGL limit is 16384, older drivers 8192).
const MAX_RAW_SIDE: usize = 8192;

struct RawSlot;

impl RawSlot {
    fn acquire() -> Option<Self> {
        let (lock, condvar) = &RAW_SLOTS;
        let mut running = lock.lock().ok()?;
        while *running >= MAX_RAW_DECODES {
            running = condvar.wait(running).ok()?;
        }
        *running += 1;
        Some(Self)
    }
}

impl Drop for RawSlot {
    fn drop(&mut self) {
        let (lock, condvar) = &RAW_SLOTS;
        if let Ok(mut running) = lock.lock() {
            *running = running.saturating_sub(1);
        }
        condvar.notify_one();
    }
}

/// Size of the developed RAW image (sensor area after the camera's crop), oriented like the
/// preview. Cheap: no demosaicing, but the whole file is read.
fn raw_dimensions(path: &Path) -> Option<(u32, u32)> {
    let raw = rawloader::decode_dummy(&mut fs::File::open(path).ok()?).ok()?;
    let width = raw.width.checked_sub(raw.crops[1] + raw.crops[3])?;
    let height = raw.height.checked_sub(raw.crops[0] + raw.crops[2])?;
    let (width, height) = (width as u32, height as u32);
    let (transpose, _, _) = raw.orientation.to_flips();
    Some(if transpose { (height, width) } else { (width, height) })
}

/// Develops the RAW file itself at (at most) `size`, to show real sensor detail when zooming.
fn decode_raw(path: &Path, size: (u32, u32)) -> Option<ColorImage> {
    let _slot = RawSlot::acquire()?;
    let developed = std::panic::catch_unwind(|| {
        let mut pipeline = imagepipe::Pipeline::new_from_file(path).ok()?;
        pipeline.globals.settings.maxwidth = (size.0 as usize).min(MAX_RAW_SIDE);
        pipeline.globals.settings.maxheight = (size.1 as usize).min(MAX_RAW_SIDE);
        pipeline.output_8bit(None).ok()
    })
    .ok()??;
    let (width, height) = (developed.width, developed.height);
    if width.checked_mul(height)?.checked_mul(4)? > MAX_TEXTURE_BYTES
        || developed.data.len() != width * height * 3
    {
        return None;
    }
    Some(ColorImage::from_rgb([width, height], &developed.data))
}

/// The embedded JPEG is quick and carries the camera's own look; the RAW is only developed
/// when more pixels than the preview holds are asked for (zooming).
fn rw2_needs_raw(embedded: (u32, u32), requested: (u32, u32)) -> bool {
    requested.0 > embedded.0 || requested.1 > embedded.1
}

fn is_rw2_data(bytes: &[u8]) -> bool {
    bytes.starts_with(b"II\x55\x00")
}

/// Location of the embedded full-size JPEG preview of a Panasonic RW2 (IFD0 tag 0x2E) and the
/// orientation (tag 0x112), read from the start of the file. RW2 is TIFF-like with the magic
/// number 0x55 instead of 0x2A, which TIFF/EXIF readers reject.
fn rw2_locate(head: &[u8]) -> Option<(usize, usize, u32)> {
    if !is_rw2_data(head) {
        return None;
    }
    let u16_at = |offset: usize| {
        head.get(offset..offset.checked_add(2)?)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let u32_at = |offset: usize| {
        head.get(offset..offset.checked_add(4)?)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    let mut jpeg = None;
    let mut orientation = 1;
    for index in 0..count {
        let entry = ifd + 2 + index * 12;
        match u16_at(entry)? {
            0x2e => jpeg = Some((u32_at(entry + 8)? as usize, u32_at(entry + 4)? as usize)),
            0x112 => orientation = u16_at(entry + 8)? as u32,
            _ => {}
        }
    }
    let (offset, length) = jpeg?;
    let orientation = if (1..=8).contains(&orientation) {
        orientation
    } else {
        1
    };
    Some((offset, length, orientation))
}

/// Embedded JPEG preview of an RW2 and its orientation. Only the needed byte range is read,
/// not the whole (about 20 MB) raw file.
fn rw2_embedded_jpeg(path: &Path) -> Option<(Vec<u8>, u32)> {
    const MAX_PREVIEW_BYTES: usize = 64 * 1024 * 1024;
    let prefix = load_prefix(path)?;
    let (offset, length, orientation) = rw2_locate(&prefix.data)?;
    if length < 4 || length > MAX_PREVIEW_BYTES {
        return None;
    }
    let end = offset.checked_add(length)?;
    let jpeg = if end <= prefix.data.len() {
        prefix.data[offset..end].to_vec()
    } else {
        use std::io::{Seek, SeekFrom};
        let mut file = fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(offset as u64)).ok()?;
        let mut data = Vec::with_capacity(length);
        file.take(length as u64).read_to_end(&mut data).ok()?;
        data
    };
    (jpeg.len() == length && jpeg.starts_with(&[0xff, 0xd8])).then_some((jpeg, orientation))
}

fn parse_exif(bytes: &[u8]) -> Option<exif::Exif> {
    if is_rw2_data(bytes) {
        let mut tiff = bytes.to_vec();
        tiff[2] = 0x2a;
        return exif::Reader::new().read_raw(tiff).ok();
    }
    exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()
}

fn orientation_of(exif: Option<&exif::Exif>) -> u32 {
    exif.and_then(|exif| {
        exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|field| field.value.get_uint(0))
    })
    .filter(|orientation| (1..=8).contains(orientation))
    .unwrap_or(1)
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut decoder = JpegDecoder::new(std::io::Cursor::new(bytes));
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    Some((info.width as u32, info.height as u32))
}

fn image_dimensions(path: &Path, bytes: &[u8]) -> Option<(u32, u32)> {
    if is_jpeg(path) {
        jpeg_dimensions(bytes)
    } else {
        let reader = ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        Some(reader.into_decoder().ok()?.dimensions())
    }
}

fn oriented_dimensions(dimensions: (u32, u32), orientation: u32) -> (u32, u32) {
    if matches!(orientation, 5..=8) {
        (dimensions.1, dimensions.0)
    } else {
        dimensions
    }
}

fn apply_exif_orientation(image: image::DynamicImage, orientation: u32) -> image::DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate90().flipv(),
        8 => image.rotate270(),
        _ => image,
    }
}

fn embedded_thumbnail(exif: &exif::Exif, orientation: u32) -> Option<ColorImage> {
    let offset = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let length = exif
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let end = offset.checked_add(length)?;
    let jpeg = exif.buf().get(offset..end)?;
    let image = apply_exif_orientation(
        decode_jpeg(std::io::Cursor::new(jpeg), Some((320, 320)))?,
        orientation,
    )
    .to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

fn decode_jpeg<R: Read>(reader: R, target: Option<(u32, u32)>) -> Option<image::DynamicImage> {
    let mut decoder = JpegDecoder::new(reader);
    decoder.set_max_decoding_buffer_size(MAX_TEXTURE_BYTES);
    if let Some(target) = target {
        decoder
            .scale(
                target.0.min(u16::MAX as u32) as u16,
                target.1.min(u16::MAX as u32) as u16,
            )
            .ok()?;
    }
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (width, height) = (info.width as u32, info.height as u32);
    match info.pixel_format {
        JpegPixelFormat::RGB24 => {
            image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(width, height, pixels)
                .map(image::DynamicImage::ImageRgb8)
        }
        JpegPixelFormat::L8 => {
            image::ImageBuffer::<image::Luma<u8>, _>::from_raw(width, height, pixels)
                .map(image::DynamicImage::ImageLuma8)
        }
        JpegPixelFormat::CMYK32 => {
            let rgb = pixels
                .chunks_exact(4)
                .flat_map(|p| [0, 1, 2].map(|i| 255_u8.saturating_sub(p[i]).saturating_sub(p[3])))
                .collect();
            image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(width, height, rgb)
                .map(image::DynamicImage::ImageRgb8)
        }
        JpegPixelFormat::L16 => None,
    }
}

fn exif_tag_name(tag: exif::Tag) -> String {
    tag.description()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Unknown EXIF tag 0x{:04X}", tag.number()))
}

const EXIF_CATEGORIES: [&str; 4] = [
    "Camera and optics",
    "Date, time and position",
    "Image",
    "Others",
];

fn exif_category(tag: exif::Tag) -> &'static str {
    use exif::Tag;
    const DATE_TIME: &[Tag] = &[
        Tag::DateTime,
        Tag::DateTimeOriginal,
        Tag::DateTimeDigitized,
        Tag::SubSecTime,
        Tag::SubSecTimeOriginal,
        Tag::SubSecTimeDigitized,
        Tag::OffsetTime,
        Tag::OffsetTimeOriginal,
        Tag::OffsetTimeDigitized,
    ];
    const CAMERA: &[Tag] = &[
        Tag::Make,
        Tag::Model,
        Tag::LensMake,
        Tag::LensModel,
        Tag::LensSpecification,
        Tag::LensSerialNumber,
        Tag::BodySerialNumber,
        Tag::CameraOwnerName,
        Tag::FNumber,
        Tag::ExposureTime,
        Tag::ExposureProgram,
        Tag::ExposureMode,
        Tag::ExposureBiasValue,
        Tag::PhotographicSensitivity,
        Tag::SensitivityType,
        Tag::ISOSpeed,
        Tag::FocalLength,
        Tag::FocalLengthIn35mmFilm,
        Tag::Flash,
        Tag::MeteringMode,
        Tag::WhiteBalance,
        Tag::MaxApertureValue,
        Tag::ApertureValue,
        Tag::ShutterSpeedValue,
        Tag::BrightnessValue,
        Tag::SubjectDistance,
        Tag::SubjectDistanceRange,
        Tag::LightSource,
        Tag::SensingMethod,
        Tag::SceneCaptureType,
        Tag::DigitalZoomRatio,
        Tag::GainControl,
        Tag::Contrast,
        Tag::Saturation,
        Tag::Sharpness,
        Tag::CustomRendered,
        Tag::SubjectArea,
    ];
    const IMAGE: &[Tag] = &[
        Tag::ImageWidth,
        Tag::ImageLength,
        Tag::PixelXDimension,
        Tag::PixelYDimension,
        Tag::Orientation,
        Tag::XResolution,
        Tag::YResolution,
        Tag::ResolutionUnit,
        Tag::ColorSpace,
        Tag::BitsPerSample,
        Tag::Compression,
        Tag::PhotometricInterpretation,
        Tag::SamplesPerPixel,
        Tag::YCbCrPositioning,
        Tag::ComponentsConfiguration,
        Tag::CompressedBitsPerPixel,
        Tag::PlanarConfiguration,
        Tag::ImageDescription,
        Tag::Software,
        Tag::Artist,
        Tag::Copyright,
    ];
    if tag.context() == exif::Context::Gps || DATE_TIME.contains(&tag) {
        "Date, time and position"
    } else if CAMERA.contains(&tag) {
        "Camera and optics"
    } else if IMAGE.contains(&tag) {
        "Image"
    } else {
        "Others"
    }
}

fn show_exif_fields(ui: &mut egui::Ui, photo: &Photo, expanded: &mut HashSet<String>) {
    let style = ui.style_mut();
    for font_id in style.text_styles.values_mut() {
        font_id.size *= 0.9;
    }
    style.spacing.item_spacing.y = 0.0;
    style.spacing.item_spacing.x = 3.0;
    let line_height = style.text_styles[&egui::TextStyle::Body].size * 1.25;
    if photo.exif.is_empty() {
        ui.label("No EXIF data");
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let content_width = ui.available_width();
            ui.set_max_width(content_width);
            for category in EXIF_CATEGORIES {
                let fields: Vec<_> = photo
                    .exif
                    .iter()
                    .filter(|(tag, _, _)| exif_category(*tag) == category)
                    .collect();
                if fields.is_empty() {
                    continue;
                }
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(category)
                        .strong()
                        .size(14.0)
                        .line_height(Some(line_height * 0.9))
                        .color(ui.visuals().strong_text_color()),
                );
                ui.separator();
                for (tag, name, value) in fields {
                    let key = format!("{}:{:?}", photo.display.display(), tag);
                    let is_expanded = expanded.contains(&key);
                    let long = value.chars().count() > 50;
                    let display_value = if long && !is_expanded {
                        format!("{}…", value.chars().take(50).collect::<String>())
                    } else {
                        value.clone()
                    };
                    ui.scope(|ui| {
                        ui.spacing_mut().interact_size.y = line_height + 4.0;
                        ui.horizontal_wrapped(|ui| {
                            let mut row = egui::text::LayoutJob::default();
                            egui::RichText::new(name)
                                .strong()
                                .line_height(Some(line_height))
                                .append_to(
                                    &mut row,
                                    ui.style(),
                                    egui::FontSelection::Default,
                                    egui::Align::BOTTOM,
                                );
                            egui::RichText::new(format!(" {display_value}"))
                                .line_height(Some(line_height))
                                .append_to(
                                    &mut row,
                                    ui.style(),
                                    egui::FontSelection::Default,
                                    egui::Align::BOTTOM,
                                );
                            row.wrap.break_anywhere = true;
                            ui.add(egui::Label::new(row).wrap());
                            if long && !is_expanded && ui.small_button("Expand").clicked() {
                                expanded.insert(key.clone());
                            } else if long && is_expanded && ui.small_button("Collapse").clicked() {
                                expanded.remove(&key);
                            }
                        });
                    });
                }
            }
        });
}

fn read_exif(path: &Path) -> Vec<(exif::Tag, String, String)> {
    let Some(prefix) = load_prefix(path) else {
        return Vec::new();
    };
    let mut exif = parse_exif(&prefix.data);
    if exif.is_none() && !prefix.complete && !is_jpeg(path) {
        exif = load_full(path).and_then(|data| parse_exif(&data));
    }
    let Some(exif) = exif else {
        return Vec::new();
    };
    exif.fields()
        .map(|field| {
            (
                field.tag,
                exif_tag_name(field.tag),
                field.display_value().to_string(),
            )
        })
        .collect()
}

/// Capture date of a photo, read from the first bytes of the file only (no full EXIF dump and
/// no shared cache pollution), so thousands of photos can be dated quickly in the background.
fn read_datetime(path: &Path) -> Option<String> {
    let find = |exif: &exif::Exif| {
        [
            exif::Tag::DateTimeOriginal,
            exif::Tag::DateTimeDigitized,
            exif::Tag::DateTime,
        ]
        .iter()
        .find_map(|tag| exif.get_field(*tag, exif::In::PRIMARY))
        .map(|field| field.display_value().to_string())
    };
    if is_jpeg(path) {
        let mut data = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(128 * 1024)
            .read_to_end(&mut data)
            .ok()?;
        parse_exif(&data).as_ref().and_then(find)
    } else {
        exif_datetime(&read_exif(path))
    }
}

fn photo_date(photo: &Photo) -> Option<&str> {
    let date = photo.datetime.as_deref()?.get(..10)?;
    let bytes = date.as_bytes();
    let valid = bytes.iter().enumerate().all(|(i, b)| {
        if i == 4 || i == 7 {
            matches!(b, b'-' | b':')
        } else {
            b.is_ascii_digit()
        }
    });
    valid.then_some(date)
}

fn date_range_label(photos: &[Photo]) -> Option<String> {
    let dates = photos
        .iter()
        .filter_map(photo_date)
        .map(|date| date.replace(':', "-"));
    let (first, last) = dates.fold((None::<String>, None::<String>), |(first, last), date| {
        (
            Some(first.map_or(date.clone(), |current| current.min(date.clone()))),
            Some(last.map_or(date.clone(), |current| current.max(date))),
        )
    });
    match (first, last) {
        (Some(first), Some(last)) if first == last => Some(first),
        (Some(first), Some(last)) => Some(format!("{first} – {last}")),
        _ => None,
    }
}

fn exif_datetime(fields: &[(exif::Tag, String, String)]) -> Option<String> {
    [
        exif::Tag::DateTimeOriginal,
        exif::Tag::DateTimeDigitized,
        exif::Tag::DateTime,
    ]
    .iter()
    .find_map(|tag| {
        fields
            .iter()
            .find(|(field_tag, _, _)| field_tag == tag)
            .map(|(_, _, value)| value.clone())
    })
}

fn process_files(
    kind: ActionKind,
    root: PathBuf,
    destination: PathBuf,
    files: Vec<PathBuf>,
    photo_files: Vec<(PathBuf, Vec<PathBuf>)>,
    photos: Vec<Photo>,
    sender: Sender<OperationMessage>,
) -> OperationResult {
    let kept_before = photos
        .iter()
        .filter(|photo| photo.kept)
        .flat_map(|photo| photo.files.iter().cloned())
        .collect();
    let mut history = HistoryEntry {
        kind,
        files: Vec::new(),
        photos,
        kept_before,
    };
    if let Err(error) = fs::create_dir_all(&destination) {
        return OperationResult {
            kind,
            history,
            completed: HashSet::new(),
            photo_files,
            errors: vec![format!("{}: {error}", destination.display())],
            is_undo: false,
            restored_photos: Vec::new(),
        };
    }
    if let (Ok(root_absolute), Ok(destination_absolute)) =
        (fs::canonicalize(&root), fs::canonicalize(&destination))
    {
        if destination_absolute.starts_with(&root_absolute) {
            return OperationResult {
                kind,
                history,
                completed: HashSet::new(),
                photo_files,
                errors: vec![
                    "Keep and delete destinations must be outside the scanned folder.".into(),
                ],
                is_undo: false,
                restored_photos: Vec::new(),
            };
        }
    }
    let total = files.len();
    let mut errors = Vec::new();
    let mut completed = HashSet::new();
    for (position, source) in files.into_iter().enumerate() {
        if !source.exists() || completed.contains(&source) {
            continue;
        }
        let relative = source.strip_prefix(&root).unwrap_or(&source);
        let mut target = destination.join(relative);
        if let Some(parent) = target.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                errors.push(format!("{}: {error}", parent.display()));
                continue;
            }
        }
        if target.exists() {
            let stem = target
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("image")
                .to_string();
            let ext = target
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            let mut suffix = 1;
            loop {
                let candidate = target.with_file_name(if ext.is_empty() {
                    format!("{stem}_{suffix}")
                } else {
                    format!("{stem}_{suffix}.{ext}")
                });
                if !candidate.exists() {
                    target = candidate;
                    break;
                }
                suffix += 1;
            }
        }
        let result = match kind {
            ActionKind::Keep => fs::copy(&source, &target).map(|_| ()),
            ActionKind::Delete => move_file(&source, &target),
        };
        match result {
            Ok(()) => {
                completed.insert(source.clone());
                history.files.push(FileMove {
                    original: source,
                    destination: target,
                });
            }
            Err(error) => errors.push(format!("{}: {error}", source.display())),
        }
        if position % 8 == 0 {
            let _ = sender.send(OperationMessage::Progress(format!(
                "{} {}/{} files…",
                if kind == ActionKind::Keep {
                    "Copying"
                } else {
                    "Moving"
                },
                position + 1,
                total
            )));
        }
    }
    OperationResult {
        kind,
        history,
        completed,
        photo_files,
        errors,
        is_undo: false,
        restored_photos: Vec::new(),
    }
}

fn preview_cache_path() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("image_sorter")
        .join("previews")
}

fn path_fingerprint(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    2u8.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
fn texture_key(path: &Path, size: (u32, u32)) -> TextureKey {
    TextureKey {
        path: path.to_path_buf(),
        width: size.0,
        height: size.1,
        fingerprint: path_fingerprint(path),
    }
}

fn texture_fingerprint(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    if let Ok(metadata) = fs::metadata(path) {
        metadata.len().hash(&mut hasher);
        metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .hash(&mut hasher);
    }
    2u8.hash(&mut hasher);
    hasher.finish()
}

fn viewer_decode_size(path: &Path) -> Option<(u32, u32)> {
    if is_raw(path) {
        if let Some(size) = raw_dimensions(path) {
            return Some(size);
        }
        if is_rw2(path) {
            let (jpeg, orientation) = rw2_embedded_jpeg(path)?;
            return Some(oriented_dimensions(jpeg_dimensions(&jpeg)?, orientation));
        }
        return None;
    }
    let prefix = load_prefix(path)?;
    let mut data = prefix.data.clone();
    let mut dimensions = image_dimensions(path, &data);
    if dimensions.is_none() && !prefix.complete {
        data = load_full(path)?;
        dimensions = image_dimensions(path, &data);
    }
    let orientation = orientation_of(parse_exif(&data).as_ref());
    Some(oriented_dimensions(dimensions?, orientation))
}

fn viewer_request_size(source: (u32, u32), viewport: (u32, u32), zoom: f32) -> (u32, u32) {
    let scale = if zoom > 1.0 {
        zoom
    } else {
        (viewport.0 as f32 / source.0 as f32).min(viewport.1 as f32 / source.1 as f32)
    };
    (
        ((source.0 as f32 * scale).round() as u32).clamp(1, source.0),
        ((source.1 as f32 * scale).round() as u32).clamp(1, source.1),
    )
}

fn texture_name(key: &TextureKey) -> String {
    format!(
        "{}-{}x{}-{}",
        key.path.to_string_lossy(),
        key.width,
        key.height,
        key.fingerprint
    )
}

fn cache_file(cache: &Path, key: &TextureKey) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    cache.join(format!("{:016x}.rgba", hasher.finish()))
}

fn read_cached_preview(cache: &Path, key: &TextureKey) -> Option<ColorImage> {
    let mut file = fs::File::open(cache_file(cache, key)).ok()?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header).ok()?;
    let width = u32::from_le_bytes(header[0..4].try_into().ok()?);
    let height = u32::from_le_bytes(header[4..8].try_into().ok()?);
    let fingerprint = u64::from_le_bytes(header[8..16].try_into().ok()?);
    if fingerprint != key.fingerprint || width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if expected > MAX_TEXTURE_BYTES {
        return None;
    }
    let mut decoder = ZlibDecoder::new(file);
    let mut rgba = Vec::with_capacity(expected);
    (&mut decoder)
        .take(expected as u64 + 1)
        .read_to_end(&mut rgba)
        .ok()?;
    if rgba.len() != expected {
        return None;
    }
    Some(ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba,
    ))
}

fn write_cached_preview(cache: &Path, key: &TextureKey, image: &ColorImage, pool: &PreviewPool) {
    if fs::create_dir_all(cache).is_err() {
        return;
    }
    let path = cache_file(cache, key);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let Ok(mut file) = fs::File::create(&temporary) else {
        return;
    };
    let width = (image.size[0] as u32).to_le_bytes();
    let height = (image.size[1] as u32).to_le_bytes();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    if encoder
        .write_all(
            &image
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect::<Vec<_>>(),
        )
        .is_err()
    {
        let _ = fs::remove_file(temporary);
        return;
    }
    let Ok(compressed) = encoder.finish() else {
        let _ = fs::remove_file(temporary);
        return;
    };
    if file.write_all(&width).is_err()
        || file.write_all(&height).is_err()
        || file.write_all(&key.fingerprint.to_le_bytes()).is_err()
        || file.write_all(&compressed).is_err()
    {
        let _ = fs::remove_file(temporary);
        return;
    }
    let _ = fs::rename(temporary, path);
    if pool
        .cache_size
        .fetch_add(compressed.len(), std::sync::atomic::Ordering::Relaxed)
        .saturating_add(compressed.len())
        > MAX_TEXTURE_CACHE_BYTES
    {
        trim_preview_cache(cache, pool);
    }
}

fn trim_preview_cache(cache: &Path, pool: &PreviewPool) {
    let Ok(entries) = fs::read_dir(cache) else {
        return;
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some((entry.path(), metadata.len(), metadata.modified().ok()?))
        })
        .collect::<Vec<_>>();
    let mut total = files.iter().map(|(_, size, _)| *size).sum::<u64>();
    pool.cache_size.store(
        total.min(usize::MAX as u64) as usize,
        std::sync::atomic::Ordering::Relaxed,
    );
    if total <= MAX_TEXTURE_CACHE_BYTES as u64 {
        return;
    }
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in files {
        if total <= MAX_TEXTURE_CACHE_BYTES as u64 {
            break;
        }
        if fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    pool.cache_size.store(
        total.min(usize::MAX as u64) as usize,
        std::sync::atomic::Ordering::Relaxed,
    );
}

fn job_timeout(priority: u8) -> std::time::Duration {
    if priority >= ACTIVE_VIEW_PREVIEW_PRIORITY {
        std::time::Duration::from_secs(30)
    } else {
        std::time::Duration::from_secs(90)
    }
}

fn enqueue_preview(
    pool: &PreviewPool,
    key: &TextureKey,
    size: (u32, u32),
    priority: u8,
    repaint: egui::Context,
) -> bool {
    let Ok(mut queue) = pool.queue.lock() else {
        return false;
    };
    if let Some(job) = queue.jobs.iter_mut().find(|job| job.key == *key) {
        // Already waiting: only promote it (a thumbnail that scrolled into view).
        if priority > job.priority {
            job.priority = priority;
        }
        job.repaint = repaint;
        job.deadline = std::time::Instant::now() + job_timeout(job.priority);
        return true;
    }
    if queue.active.contains(key) {
        // A worker is already decoding it.
        return false;
    }
    if priority >= 90 {
        let mut index = 0;
        while index < queue.jobs.len() {
            if queue.jobs[index].priority >= 90 && queue.jobs[index].key.path != key.path {
                let removed = queue.jobs.swap_remove(index);
                queue.active.remove(&removed.key);
            } else {
                index += 1;
            }
        }
    }
    if queue.jobs.len() >= MAX_PREVIEW_QUEUE {
        let lowest = queue
            .jobs
            .iter()
            .enumerate()
            .min_by_key(|(_, job)| job.priority)
            .map(|(index, job)| (index, job.priority));
        match lowest {
            Some((index, lowest)) if lowest < priority || priority >= 90 => {
                let removed = queue.jobs.swap_remove(index);
                queue.active.remove(&removed.key);
            }
            _ => return false,
        }
    }
    queue.active.insert(key.clone());
    queue.jobs.push(PreviewJob {
        key: key.clone(),
        size,
        priority,
        repaint,
        deadline: std::time::Instant::now() + job_timeout(priority),
    });
    drop(queue);
    pool.ready.notify_one();
    true
}

/// Removes and returns the most urgent job; equally urgent jobs run in request order.
fn take_best_job(jobs: &mut Vec<PreviewJob>) -> Option<PreviewJob> {
    let mut best: Option<usize> = None;
    for (index, job) in jobs.iter().enumerate() {
        if best.is_none_or(|current| job.priority > jobs[current].priority) {
            best = Some(index);
        }
    }
    best.map(|index| jobs.remove(index))
}

fn preview_worker(
    pool: Arc<PreviewPool>,
    sender: Sender<(TextureKey, TextureKey, PreviewOutcome)>,
    cache: PathBuf,
) {
    loop {
        let job = {
            let Ok(mut queue) = pool.queue.lock() else {
                return;
            };
            loop {
                if let Some(job) = take_best_job(&mut queue.jobs) {
                    break job;
                }
                let Ok(next) = pool.ready.wait(queue) else {
                    return;
                };
                queue = next;
            }
        };
        let actual_key = TextureKey {
            fingerprint: texture_fingerprint(&job.key.path),
            ..job.key.clone()
        };
        let outcome = if job.deadline <= std::time::Instant::now() {
            PreviewOutcome::Expired
        } else {
            let cacheable = job.size == VIEW_PREVIEW;
            let cached = if cacheable {
                read_cached_preview(&cache, &actual_key)
            } else {
                None
            };
            let image = cached.or_else(|| {
                let image = decode_preview(&job.key.path, job.size)?;
                if cacheable {
                    write_cached_preview(&cache, &actual_key, &image, &pool);
                }
                Some(image)
            });
            match image {
                Some(image) => PreviewOutcome::Ready(image),
                None => PreviewOutcome::Failed,
            }
        };
        if sender.send((job.key.clone(), actual_key, outcome)).is_err() {
            return;
        }
        job.repaint.request_repaint();
    }
}

fn system_thumbnail(path: &Path) -> Option<ColorImage> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("thumbnails");
    system_thumbnail_at(path, &cache)
}

fn png_text_matches(data: &[u8], wanted_key: &str, wanted_value: &str) -> bool {
    if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return false;
    }
    let mut offset = 8;
    while offset + 12 <= data.len() {
        let length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        let end = match offset
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
        {
            Some(end) if end <= data.len() => end,
            _ => return false,
        };
        let chunk = &data[offset + 8..offset + 8 + length];
        if &data[offset + 4..offset + 8] == b"tEXt" {
            if let Some(separator) = chunk.iter().position(|byte| *byte == 0) {
                if &chunk[..separator] == wanted_key.as_bytes()
                    && &chunk[separator + 1..] == wanted_value.as_bytes()
                {
                    return true;
                }
            }
        } else if &data[offset + 4..offset + 8] == b"IEND" {
            break;
        }
        offset = end;
    }
    false
}

fn system_thumbnail_at(path: &Path, cache: &Path) -> Option<ColorImage> {
    let uri = Url::from_file_path(path).ok()?.to_string();
    let name = format!("{:x}.png", md5::compute(uri.as_bytes()));
    for size in ["xx-large", "x-large", "large", "normal"] {
        let thumbnail = cache.join(size).join(&name);
        let Ok(data) = fs::read(thumbnail) else {
            continue;
        };
        if data.len() > 16 * 1024 * 1024
            || !data.windows(uri.len()).any(|value| value == uri.as_bytes())
        {
            continue;
        }
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs()
            .to_string();
        let source_size = metadata.len().to_string();
        if !png_text_matches(&data, "Thumb::MTime", &modified)
            || !png_text_matches(&data, "Thumb::Size", &source_size)
        {
            continue;
        }
        if let Some(image) = decode_cached_thumbnail_png(&data) {
            let thumbnail = image.thumbnail(320, 240).to_rgba8();
            return Some(ColorImage::from_rgba_unmultiplied(
                [thumbnail.width() as usize, thumbnail.height() as usize],
                thumbnail.as_raw(),
            ));
        }
    }
    None
}

fn decode_cached_thumbnail_png(data: &[u8]) -> Option<image::DynamicImage> {
    if data.len() <= 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
        return None;
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_ignore_text_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let width = reader.info().width;
    let height = reader.info().height;
    let buffer_size = reader.output_buffer_size()?;
    if width == 0 || height == 0 || buffer_size > MAX_TEXTURE_BYTES {
        return None;
    }
    let mut pixels = vec![0; buffer_size];
    let output = reader.next_frame(&mut pixels).ok()?;
    let rgba = match output.color_type {
        png::ColorType::Rgba => pixels[..output.buffer_size()].to_vec(),
        png::ColorType::Rgb => pixels[..output.buffer_size()]
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => pixels[..output.buffer_size()]
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        png::ColorType::Grayscale => pixels[..output.buffer_size()]
            .iter()
            .flat_map(|value| [*value, *value, *value, 255])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    let decoded = image::RgbaImage::from_raw(output.width, output.height, rgba)?;
    Some(image::DynamicImage::ImageRgba8(decoded))
}

fn quick_preview(
    path: &Path,
    exif: Option<&exif::Exif>,
    orientation: u32,
    allow_system: bool,
) -> Option<ColorImage> {
    if allow_system && orientation == 1 {
        if let Some(thumbnail) = system_thumbnail(path) {
            return Some(thumbnail);
        }
    }
    embedded_thumbnail(exif?, orientation)
}

/// Decodes JPEG bytes at (at most) `size`, applying the orientation. A thumbnail request
/// (`VIEW_PREVIEW`) is decoded straight to thumbnail size using the JPEG's DCT scaling.
fn decode_jpeg_bytes(data: &[u8], orientation: u32, size: (u32, u32)) -> Option<image::DynamicImage> {
    let thumbnail = size == VIEW_PREVIEW;
    let requested = if thumbnail {
        if matches!(orientation, 5..=8) {
            (size.1, size.0)
        } else {
            size
        }
    } else {
        jpeg_dimensions(data)
            .map(|source| oriented_dimensions(source, orientation))
            .map_or(size, |source| (size.0.min(source.0), size.1.min(source.1)))
    };
    let decode_target = if matches!(orientation, 5..=8) {
        (requested.1, requested.0)
    } else {
        requested
    };
    let image = apply_exif_orientation(
        decode_jpeg(std::io::Cursor::new(data), Some(decode_target))?,
        orientation,
    );
    Some(if image.width() > requested.0 || image.height() > requested.1 {
        image.thumbnail(requested.0, requested.1)
    } else {
        image
    })
}

fn color_image(decoded: image::DynamicImage) -> Option<ColorImage> {
    let width = decoded.width() as usize;
    let height = decoded.height() as usize;
    if width.checked_mul(height)?.checked_mul(4)? > MAX_TEXTURE_BYTES {
        return None;
    }
    let rgba = decoded.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied(
        [width, height],
        rgba.as_raw(),
    ))
}

fn decode_preview(path: &Path, size: (u32, u32)) -> Option<ColorImage> {
    if is_raw(path) && !is_rw2(path) {
        // Other RAW formats have no embedded-preview reader here: reuse the system thumbnail
        // for grid cells when there is one, otherwise develop the RAW at the requested size.
        if size == VIEW_PREVIEW {
            if let Some(thumbnail) = system_thumbnail(path) {
                return Some(thumbnail);
            }
        }
        return decode_raw(path, size);
    }
    if is_rw2(path) {
        let embedded = rw2_embedded_jpeg(path);
        if size != VIEW_PREVIEW {
            let preview_size = embedded.as_ref().and_then(|(jpeg, orientation)| {
                jpeg_dimensions(jpeg).map(|dimensions| oriented_dimensions(dimensions, *orientation))
            });
            if preview_size.is_none_or(|preview| rw2_needs_raw(preview, size)) {
                if let Some(image) = decode_raw(path, size) {
                    return Some(image);
                }
            }
        }
        let (jpeg, orientation) = embedded?;
        return color_image(decode_jpeg_bytes(&jpeg, orientation, size)?);
    }
    let prefix = if size == VIEW_PREVIEW {
        load_prefix(path)?
    } else {
        CachedBytes {
            data: load_full(path)?,
            complete: true,
        }
    };
    let mut exif = parse_exif(&prefix.data);
    let mut orientation = orientation_of(exif.as_ref());
    if size == VIEW_PREVIEW {
        if let Some(thumbnail) = quick_preview(path, exif.as_ref(), orientation, true) {
            return Some(thumbnail);
        }
    }
    let data = if prefix.complete {
        prefix.data
    } else {
        load_full(path)?
    };
    if !prefix.complete && !is_jpeg(path) {
        exif = parse_exif(&data);
        orientation = orientation_of(exif.as_ref());
        if size == VIEW_PREVIEW {
            if let Some(thumbnail) = quick_preview(path, exif.as_ref(), orientation, false) {
                return Some(thumbnail);
            }
        }
    }
    let output_target = if size == VIEW_PREVIEW && matches!(orientation, 5..=8) {
        (size.1, size.0)
    } else {
        size
    };
    let decoded = if is_jpeg(path) {
        let requested = if size == VIEW_PREVIEW {
            output_target
        } else {
            image_dimensions(path, &data)
                .map(|source| oriented_dimensions(source, orientation))
                .map_or(size, |source| (size.0.min(source.0), size.1.min(source.1)))
        };
        let decode_target = if matches!(orientation, 5..=8) {
            (requested.1, requested.0)
        } else {
            requested
        };
        let image = apply_exif_orientation(
            decode_jpeg(std::io::Cursor::new(&data[..]), Some(decode_target))?,
            orientation,
        );
        if image.width() > requested.0 || image.height() > requested.1 {
            image.thumbnail(requested.0, requested.1)
        } else {
            image
        }
    } else {
        let mut reader = ImageReader::new(std::io::Cursor::new(&data[..]))
            .with_guessed_format()
            .ok()?;
        reader.no_limits();
        let decoder = reader.into_decoder().ok()?;
        let dimensions = decoder.dimensions();
        if (dimensions.0 as usize)
            .saturating_mul(dimensions.1 as usize)
            .saturating_mul(4)
            > MAX_TEXTURE_BYTES
        {
            return None;
        }
        let pixels = apply_exif_orientation(
            image::DynamicImage::from_decoder(decoder).ok()?,
            orientation,
        );
        let target = if size == VIEW_PREVIEW {
            size
        } else {
            oriented_dimensions(dimensions, orientation)
        };
        if pixels.width() <= target.0 && pixels.height() <= target.1 && size != VIEW_PREVIEW {
            pixels
        } else {
            pixels.thumbnail(target.0, target.1)
        }
    };
    let width = decoded.width() as usize;
    let height = decoded.height() as usize;
    if width.checked_mul(height)?.checked_mul(4)? > MAX_TEXTURE_BYTES {
        return None;
    }
    let rgba = decoded.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied(
        [width, height],
        rgba.as_raw(),
    ))
}

fn scan_keep_folder(root: &Path) -> HashSet<PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

fn is_kept(photo: &Photo, root: &Path, keep_folder: &Path, keep_files: &HashSet<PathBuf>) -> bool {
    let Ok(relative) = photo.display.strip_prefix(root) else {
        return false;
    };
    let kept_path = keep_folder.join(relative);
    let equivalent = |left: &Path, right: &Path| {
        let Ok(left) = fs::canonicalize(left) else {
            return false;
        };
        fs::canonicalize(right).is_ok_and(|right| left == right)
    };
    keep_files.contains(&kept_path)
        || equivalent(&kept_path, &photo.display)
        || photo.files.iter().any(|file| {
            file.strip_prefix(root)
                .ok()
                .map(|relative| keep_folder.join(relative))
                .is_some_and(|kept| keep_files.contains(&kept) || equivalent(&kept, file))
        })
}

fn scan_folder(root: &Path) -> Vec<Photo> {
    let mut folders: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = extension(entry.path());
        if IMAGE_EXTENSIONS.contains(&ext.as_str()) || RAW_EXTENSIONS.contains(&ext.as_str()) {
            folders
                .entry(entry.path().parent().unwrap_or(root).to_path_buf())
                .or_default()
                .push(entry.path().to_path_buf());
        }
    }

    let mut photos = Vec::new();
    for (folder, mut files) in folders {
        files.sort();
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for file in files {
            groups
                .entry(stem_key(&file).to_lowercase())
                .or_default()
                .push(file);
        }
        for mut group in groups.into_values() {
            group.sort();
            let jpeg = group
                .iter()
                .position(|path| matches!(extension(path).as_str(), "jpg" | "jpeg"));
            let display_index = jpeg
                .or_else(|| {
                    group
                        .iter()
                        .position(|path| IMAGE_EXTENSIONS.contains(&extension(path).as_str()))
                })
                .or_else(|| {
                    group
                        .iter()
                        .position(|path| RAW_EXTENSIONS.contains(&extension(path).as_str()))
                });
            let Some(display_index) = display_index else {
                continue;
            };
            let display = group[display_index].clone();
            let relative = folder.strip_prefix(root).unwrap_or(&folder).to_path_buf();
            photos.push(Photo {
                display,
                files: group,
                subfolder: relative,
                datetime: None,
                exif: Vec::new(),
                exif_loaded: false,
                datetime_loaded: false,
                kept: false,
            });
        }
    }
    photos.sort_by(|a, b| {
        a.subfolder
            .cmp(&b.subfolder)
            .then_with(|| a.display.file_name().cmp(&b.display.file_name()))
    });
    photos
}

impl SorterApp {
    fn save_settings(&mut self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(value) = serde_json::to_string_pretty(&self.settings) {
            let _ = fs::write(path, value);
        }
    }

    fn refresh_kept_status(&mut self, ctx: &egui::Context) {
        let (Some(_root), Some(keep_folder)) = (self.root.clone(), {
            let folder = self.settings.keep_folder.trim();
            (!folder.is_empty()).then(|| PathBuf::from(folder))
        }) else {
            for photo in &mut self.photos {
                photo.kept = false;
            }
            return;
        };
        self.keep_scan_generation = self.keep_scan_generation.wrapping_add(1);
        let generation = self.keep_scan_generation;
        let sender = self.keep_scan_sender.clone();
        let repaint = ctx.clone();
        self.keep_scan_pending = true;
        self.next_keep_scan = std::time::Instant::now() + KEEP_POLL_INTERVAL;
        let root = self.root.clone().unwrap_or_default();
        let snapshot: Vec<Photo> = self
            .photos
            .iter()
            .map(|photo| Photo {
                display: photo.display.clone(),
                files: photo.files.clone(),
                subfolder: PathBuf::new(),
                datetime: None,
                exif: Vec::new(),
                exif_loaded: true,
                datetime_loaded: true,
                kept: false,
            })
            .collect();
        thread::spawn(move || {
            let files = scan_keep_folder(&keep_folder);
            let kept: HashSet<PathBuf> = snapshot
                .iter()
                .filter(|photo| is_kept(photo, &root, &keep_folder, &files))
                .map(|photo| photo.display.clone())
                .collect();
            let _ = sender.send((generation, kept));
            repaint.request_repaint();
        });
    }

    fn sync_keep_watcher(&mut self, ctx: &egui::Context) {
        use notify::Watcher;
        let wanted = (!self.settings.keep_folder.trim().is_empty())
            .then(|| PathBuf::from(self.settings.keep_folder.trim()));
        if wanted == self.keep_watch_path {
            return;
        }
        self.keep_watcher = None;
        self.keep_watch_path = wanted.clone();
        let Some(path) = wanted else { return };
        let dirty = self.keep_dirty.clone();
        let repaint = ctx.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok_and(|event| !event.kind.is_access()) {
                dirty.store(true, std::sync::atomic::Ordering::SeqCst);
                repaint.request_repaint();
            }
        });
        if let Ok(mut watcher) = watcher {
            if watcher
                .watch(&path, notify::RecursiveMode::Recursive)
                .is_ok()
            {
                self.keep_watcher = Some(watcher);
            }
        }
        self.keep_dirty
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn poll_kept_status(&mut self, ctx: &egui::Context) {
        self.sync_keep_watcher(ctx);
        if self.settings.keep_folder.trim().is_empty() {
            for photo in &mut self.photos {
                photo.kept = false;
            }
            return;
        }
        let mut latest = None;
        while let Ok(result) = self.keep_scan_receiver.try_recv() {
            latest = Some(result);
        }
        if let Some((generation, kept)) = latest {
            self.keep_scan_pending = false;
            self.next_keep_scan = std::time::Instant::now() + KEEP_POLL_INTERVAL;
            if generation == self.keep_scan_generation {
                for photo in &mut self.photos {
                    photo.kept = kept.contains(&photo.display);
                }
            }
        } else if self.keep_scan_pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        } else if self
            .keep_dirty
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            // The keep folder changed on disk: rescan shortly (debounces bursts of events).
            let soon = std::time::Instant::now() + std::time::Duration::from_millis(300);
            if soon < self.next_keep_scan {
                self.next_keep_scan = soon;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        } else if std::time::Instant::now() >= self.next_keep_scan {
            self.next_keep_scan = std::time::Instant::now() + KEEP_POLL_INTERVAL;
            self.refresh_kept_status(ctx);
        }
    }

    fn choose_folder(&mut self, ctx: &egui::Context) {
        let mut dialog = native_dialog::FileDialog::new();
        if let Some(previous) = &self.previous_folder {
            dialog = dialog.set_location(previous);
        }
        if let Ok(Some(folder)) = dialog.show_open_single_dir() {
            self.previous_folder = folder.parent().map(Path::to_path_buf);
            self.root = Some(folder.clone());
            self.photos.clear();
            self.photos_changed();
            self.selection.clear();
            self.source_sizes.clear();
            self.texture_fingerprints.clear();
            self.pending_source_info.clear();
            self.pending_exif.clear();
            bytes_cache_clear();
            self.clear_textures();
            self.cancel_datetime_scan();
            self.current = 0;
            self.mode = ViewMode::Grid;
            self.scan_generation = self.scan_generation.wrapping_add(1);
            let generation = self.scan_generation;
            let folder_label = folder.display().to_string();
            let sender = self.scan_sender.clone();
            let repaint = ctx.clone();
            thread::spawn(move || {
                let photos = scan_folder(&folder);
                let _ = sender.send((generation, folder, photos));
                repaint.request_repaint();
            });
            self.notice = format!("Scanning {folder_label}…");
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn photos_changed(&mut self) {
        self.photos_version = self.photos_version.wrapping_add(1);
    }

    fn ensure_photo_index(&mut self) {
        if self.photo_index_version == self.photos_version && self.photo_index.len() == self.photos.len() {
            return;
        }
        self.photo_index = self
            .photos
            .iter()
            .enumerate()
            .map(|(index, photo)| (photo.display.clone(), index))
            .collect();
        self.photo_index_version = self.photos_version;
    }

    fn cancel_datetime_scan(&mut self) {
        if let Some(scan) = self.datetime_scan.take() {
            scan.cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn start_datetime_scan(&mut self, ctx: &egui::Context) {
        self.cancel_datetime_scan();
        let mut background: Vec<PathBuf> = self
            .photos
            .iter()
            .filter(|photo| !photo.datetime_loaded)
            .map(|photo| photo.display.clone())
            .collect();
        background.reverse();
        if background.is_empty() {
            return;
        }
        let scan = Arc::new(DatetimeScan {
            state: std::sync::Mutex::new(DatetimeState {
                urgent: Vec::new(),
                background,
                taken: HashSet::new(),
            }),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        });
        for _ in 0..DATETIME_WORKERS {
            let scan = Arc::clone(&scan);
            let sender = self.datetime_sender.clone();
            let repaint = ctx.clone();
            let generation = self.scan_generation;
            thread::spawn(move || {
                let mut batch = Vec::new();
                let mut last_flush = std::time::Instant::now();
                while !scan.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                    let Some(path) = scan.next() else { break };
                    let datetime = read_datetime(&path);
                    batch.push((path, datetime));
                    if batch.len() >= 64
                        || last_flush.elapsed() >= std::time::Duration::from_millis(150)
                    {
                        if sender
                            .send((generation, std::mem::take(&mut batch)))
                            .is_err()
                        {
                            return;
                        }
                        repaint.request_repaint();
                        last_flush = std::time::Instant::now();
                    }
                }
                if !batch.is_empty() {
                    let _ = sender.send((generation, batch));
                    repaint.request_repaint();
                }
            });
        }
        self.datetime_scan = Some(scan);
    }

    /// Dates the given photos before the rest of the folder (those around the viewport).
    fn prioritize_datetimes(&self, paths: Vec<PathBuf>) {
        if let Some(scan) = &self.datetime_scan {
            scan.prioritize(paths);
        }
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let mut latest = None;
        while let Ok(result) = self.scan_receiver.try_recv() {
            latest = Some(result);
        }
        if let Some((generation, folder, photos)) = latest {
            if generation == self.scan_generation && self.root.as_ref() == Some(&folder) {
                self.notice = format!("{} images found in {}", photos.len(), folder.display());
                self.photos = photos;
                self.photos_changed();
                self.refresh_kept_status(ctx);
                self.clear_textures();
                self.source_sizes.clear();
                self.texture_fingerprints.clear();
                self.pending_source_info.clear();
                self.pending_exif.clear();
                self.selection.clear();
                self.current = 0;
                self.dates_version = self.dates_version.wrapping_add(1);
                self.start_datetime_scan(ctx);
            }
        } else if self.root.is_some() && self.notice.starts_with("Scanning ") {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut source_infos = Vec::new();
        while let Ok(info) = self.source_info_receiver.try_recv() {
            source_infos.push(info);
        }
        let mut datetimes = Vec::new();
        while let Ok(batch) = self.datetime_receiver.try_recv() {
            datetimes.push(batch);
        }
        let mut exif_updates = Vec::new();
        while let Ok(update) = self.exif_receiver.try_recv() {
            exif_updates.push(update);
        }
        if source_infos.is_empty() && datetimes.is_empty() && exif_updates.is_empty() {
            return;
        }
        self.ensure_photo_index();
        for (path, size, fingerprint) in source_infos {
            self.pending_source_info.remove(&path);
            if self.photo_index.contains_key(&path) {
                self.source_sizes.insert(path.clone(), size);
                self.texture_fingerprints.insert(path, fingerprint);
            }
        }
        let mut dates_changed = false;
        for (generation, batch) in datetimes {
            if generation != self.scan_generation {
                continue;
            }
            for (path, datetime) in batch {
                if let Some(photo) = self
                    .photo_index
                    .get(&path)
                    .and_then(|index| self.photos.get_mut(*index))
                {
                    if !photo.datetime_loaded {
                        photo.datetime_loaded = true;
                        if datetime.is_some() {
                            photo.datetime = datetime;
                            dates_changed = true;
                        }
                    }
                }
            }
        }
        for (generation, path, fields) in exif_updates {
            self.pending_exif.remove(&path);
            if generation != self.scan_generation {
                continue;
            }
            if let Some(photo) = self
                .photo_index
                .get(&path)
                .and_then(|index| self.photos.get_mut(*index))
            {
                photo.datetime = exif_datetime(&fields).or_else(|| photo.datetime.take());
                photo.datetime_loaded = true;
                photo.exif = fields;
                photo.exif_loaded = true;
                dates_changed = true;
            }
        }
        if dates_changed {
            self.dates_version = self.dates_version.wrapping_add(1);
        }
    }

    fn clear_textures(&mut self) {
        self.textures.clear();
        self.texture_bytes = 0;
        self.queued_textures.clear();
        self.failed_textures.clear();
    }

    /// Frees the textures of photos that no longer exist (after a delete).
    fn forget_missing_textures(&mut self, available: &HashSet<PathBuf>) {
        self.textures.retain(|path, _| available.contains(path));
        self.texture_bytes = self
            .textures
            .values()
            .flatten()
            .map(|entry| entry.bytes)
            .sum();
        self.queued_textures
            .retain(|key, _| available.contains(&key.path));
        self.failed_textures
            .retain(|key| available.contains(&key.path));
    }

    fn touch_texture(&mut self, path: &Path, index: usize) {
        self.texture_clock = self.texture_clock.wrapping_add(1);
        if let Some(entry) = self
            .textures
            .get_mut(path)
            .and_then(|entries| entries.get_mut(index))
        {
            entry.last_used = self.texture_clock;
        }
    }

    fn evict_textures(&mut self) {
        while self.texture_bytes > MAX_TEXTURE_MEMORY {
            let oldest = self
                .textures
                .iter()
                .flat_map(|(path, entries)| {
                    entries
                        .iter()
                        .enumerate()
                        .map(move |(index, entry)| (entry.last_used, path, index))
                })
                .min_by_key(|(last_used, _, _)| *last_used)
                .map(|(_, path, index)| (path.clone(), index));
            let Some((path, index)) = oldest else {
                self.texture_bytes = 0;
                break;
            };
            if let Some(entries) = self.textures.get_mut(&path) {
                let removed = entries.swap_remove(index);
                self.texture_bytes = self.texture_bytes.saturating_sub(removed.bytes);
                if entries.is_empty() {
                    self.textures.remove(&path);
                }
            }
        }
    }

    fn poll_textures(&mut self, ctx: &egui::Context) {
        // Each texture costs a GPU upload: cap the number per frame and let the next frame
        // continue, so scrolling stays smooth while thumbnails stream in.
        let mut uploaded = 0usize;
        let mut finished = Vec::new();
        let mut pending_more = false;
        while let Ok((requested_key, key, outcome)) = self.texture_receiver.try_recv() {
            finished.push(requested_key.clone());
            match outcome {
                PreviewOutcome::Ready(image) => {
                    self.ensure_photo_index();
                    if self.photo_index.contains_key(&key.path) {
                        self.texture_fingerprints
                            .insert(key.path.clone(), key.fingerprint);
                        let bytes = image.width().saturating_mul(image.height()).saturating_mul(4);
                        let handle =
                            ctx.load_texture(texture_name(&key), image, TextureOptions::LINEAR);
                        self.texture_clock = self.texture_clock.wrapping_add(1);
                        let entries = self.textures.entry(key.path.clone()).or_default();
                        if let Some(old) = entries.iter().position(|entry| entry.key == key) {
                            let removed = entries.swap_remove(old);
                            self.texture_bytes = self.texture_bytes.saturating_sub(removed.bytes);
                        }
                        entries.push(TextureEntry {
                            key,
                            handle,
                            bytes,
                            last_used: self.texture_clock,
                        });
                        self.texture_bytes = self.texture_bytes.saturating_add(bytes);
                        uploaded += 1;
                    }
                }
                PreviewOutcome::Failed => {
                    self.failed_textures.insert(requested_key);
                }
                PreviewOutcome::Expired => {}
            }
            if uploaded >= MAX_UPLOADS_PER_FRAME {
                pending_more = true;
                break;
            }
        }
        if !finished.is_empty() {
            for key in &finished {
                self.queued_textures.remove(key);
            }
            if let Ok(mut queue) = self.preview_pool.queue.lock() {
                for key in &finished {
                    queue.active.remove(key);
                }
            }
        }
        self.evict_textures();
        if pending_more {
            ctx.request_repaint();
        }
    }

    fn retain_prefetch(&mut self, wanted: &HashSet<PathBuf>) {
        let removed = if let Ok(mut queue) = self.preview_pool.queue.lock() {
            let mut removed = Vec::new();
            let mut retained = Vec::new();
            for job in std::mem::take(&mut queue.jobs) {
                let is_prefetch = (PREFETCH_MIN_PRIORITY..=PREFETCH_PRIORITY)
                    .contains(&job.priority)
                    && job.size != VIEW_PREVIEW;
                if is_prefetch && !wanted.contains(&job.key.path) {
                    queue.active.remove(&job.key);
                    removed.push(job.key);
                } else {
                    retained.push(job);
                }
            }
            queue.jobs = retained;
            removed
        } else {
            Vec::new()
        };
        for key in removed {
            self.queued_textures.remove(&key);
        }
    }

    /// Cancels queued grid thumbnails of photos that are no longer near the viewport.
    /// Thumbnails that were on screen but no longer are fall back to the neighbour priority.
    fn retain_grid_thumbnails(&mut self, wanted: &HashSet<PathBuf>, visible: &HashSet<PathBuf>) {
        let mut demoted = Vec::new();
        let removed = if let Ok(mut queue) = self.preview_pool.queue.lock() {
            let mut removed = Vec::new();
            let mut retained = Vec::new();
            for mut job in std::mem::take(&mut queue.jobs) {
                let is_grid_thumbnail =
                    job.size == VIEW_PREVIEW && job.priority < PREFETCH_MIN_PRIORITY;
                if is_grid_thumbnail && !wanted.contains(&job.key.path) {
                    queue.active.remove(&job.key);
                    removed.push(job.key);
                } else {
                    if is_grid_thumbnail
                        && job.priority > GRID_NEAR_PRIORITY
                        && !visible.contains(&job.key.path)
                    {
                        job.priority = GRID_NEAR_PRIORITY;
                        demoted.push(job.key.clone());
                    }
                    retained.push(job);
                }
            }
            queue.jobs = retained;
            removed
        } else {
            Vec::new()
        };
        for key in removed {
            self.queued_textures.remove(&key);
        }
        for key in demoted {
            if let Some(priority) = self.queued_textures.get_mut(&key) {
                *priority = GRID_NEAR_PRIORITY;
            }
        }
    }

    fn prioritize_viewer_image(&mut self, path: &Path) {
        let removed = if let Ok(mut queue) = self.preview_pool.queue.lock() {
            let mut removed = Vec::new();
            let mut retained = Vec::new();
            for job in std::mem::take(&mut queue.jobs) {
                if job.priority < ACTIVE_VIEW_IMAGE_PRIORITY || job.key.path == path {
                    retained.push(job);
                } else {
                    queue.active.remove(&job.key);
                    removed.push(job.key);
                }
            }
            queue.jobs = retained;
            removed
        } else {
            Vec::new()
        };
        for key in removed {
            self.queued_textures.remove(&key);
        }
    }

    fn request_exif(&mut self, ctx: &egui::Context, path: &Path, generation: usize) {
        if !self.pending_exif.insert(path.to_path_buf()) {
            return;
        }
        let path = path.to_path_buf();
        let sender = self.exif_sender.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let fields = read_exif(&path);
            let _ = sender.send((generation, path, fields));
            repaint.request_repaint();
        });
    }

    fn request_source_info(&mut self, ctx: &egui::Context, path: &Path) {
        if !self.pending_source_info.insert(path.to_path_buf()) {
            return;
        }
        let path = path.to_path_buf();
        let sender = self.source_info_sender.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let size = viewer_decode_size(&path).unwrap_or(VIEW_PREVIEW);
            let fingerprint = texture_fingerprint(&path);
            let _ = sender.send((path, size, fingerprint));
            repaint.request_repaint();
        });
    }

    fn cached_texture_key(&mut self, path: &Path, size: (u32, u32)) -> TextureKey {
        let fingerprint = *self
            .texture_fingerprints
            .entry(path.to_path_buf())
            .or_insert_with(|| path_fingerprint(path));
        TextureKey {
            path: path.to_path_buf(),
            width: size.0,
            height: size.1,
            fingerprint,
        }
    }

    fn texture(&mut self, ctx: &egui::Context, path: &Path, size: (u32, u32), priority: u8) {
        let key = self.cached_texture_key(path, size);
        if let Some(index) = self.textures.get(path).and_then(|entries| {
            entries.iter().position(|existing| {
                existing.key.fingerprint == key.fingerprint
                    && existing.key.width >= key.width
                    && existing.key.height >= key.height
            })
        }) {
            self.touch_texture(path, index);
            return;
        }
        if self.failed_textures.contains(&key) {
            return;
        }
        if let Some(&queued_priority) = self.queued_textures.get(&key) {
            let still_active = self
                .preview_pool
                .queue
                .lock()
                .map(|queue| queue.active.contains(&key))
                .unwrap_or(false);
            if still_active {
                if priority > queued_priority
                    && enqueue_preview(&self.preview_pool, &key, size, priority, ctx.clone())
                {
                    self.queued_textures.insert(key, priority);
                }
                return;
            }
            self.queued_textures.remove(&key);
        }
        if enqueue_preview(&self.preview_pool, &key, size, priority, ctx.clone()) {
            self.queued_textures.insert(key, priority);
        }
    }

    /// Grid thumbnail: returns it when loaded, otherwise (re)requests it with `priority`.
    fn grid_thumbnail(
        &mut self,
        ctx: &egui::Context,
        path: &Path,
        priority: u8,
    ) -> Option<TextureHandle> {
        if let Some(texture) = self.viewer_texture(path, VIEW_PREVIEW) {
            return Some(texture);
        }
        self.texture(ctx, path, VIEW_PREVIEW, priority);
        None
    }

    fn source_size(&mut self, ctx: &egui::Context, path: &Path) -> (u32, u32) {
        if let Some(size) = self.source_sizes.get(path) {
            return *size;
        }
        self.request_source_info(ctx, path);
        VIEW_PREVIEW
    }

    fn viewer_texture(&mut self, path: &Path, target: (u32, u32)) -> Option<TextureHandle> {
        let entries = self.textures.get(path)?;
        let fingerprint = self
            .texture_fingerprints
            .get(path)
            .copied()
            .unwrap_or_else(|| path_fingerprint(path));
        let (index, handle) = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.key.fingerprint == fingerprint)
            .min_by_key(|(_, entry)| {
                entry.key.width.abs_diff(target.0) as u64
                    + entry.key.height.abs_diff(target.1) as u64
            })
            .map(|(index, entry)| (index, entry.handle.clone()))?;
        self.touch_texture(path, index);
        Some(handle)
    }

    fn action_indices(&self) -> Vec<usize> {
        if self.mode == ViewMode::Viewer {
            vec![self.current]
        } else {
            let mut selected: Vec<_> = self.selection.iter().copied().collect();
            selected.sort_unstable();
            selected
        }
    }

    fn sort_photos(&mut self, kind: ActionKind, ctx: &egui::Context) {
        if self.operation_pending {
            return;
        }
        let destination = match kind {
            ActionKind::Keep => self.settings.keep_folder.trim(),
            ActionKind::Delete => self.settings.delete_folder.trim(),
        };
        if destination.is_empty() {
            self.pending_action = Some(kind);
            self.open_settings();
            return;
        }
        let destination = PathBuf::from(destination);
        let Some(root) = self.root.clone() else {
            self.notice = "Choose a source folder first.".into();
            return;
        };
        if fs::create_dir_all(&destination).is_err() {
            self.notice = "The destination folder could not be created.".into();
            return;
        }
        if let (Ok(root_absolute), Ok(destination_absolute)) =
            (fs::canonicalize(&root), fs::canonicalize(&destination))
        {
            if destination_absolute.starts_with(&root_absolute) {
                self.notice =
                    "Keep and delete destinations must be outside the scanned folder.".into();
                return;
            }
        }
        let indices = self.action_indices();
        if indices.is_empty() {
            self.notice = "Select at least one image first.".into();
            return;
        }
        let photo_files: Vec<_> = indices
            .iter()
            .filter_map(|index| {
                self.photos
                    .get(*index)
                    .map(|photo| (photo.display.clone(), photo.files.clone()))
            })
            .collect();
        let files = photo_files
            .iter()
            .flat_map(|(_, files)| files.iter().cloned())
            .collect::<Vec<_>>();
        let photos = indices
            .iter()
            .filter_map(|index| self.photos.get(*index).cloned())
            .collect();
        let sender = self.operation_sender.clone();
        let progress_sender = sender.clone();
        self.operation_pending = true;
        self.action_origin = Some(self.mode);
        self.action_next = self
            .photos
            .iter()
            .skip(self.current + 1)
            .map(|photo| photo.display.clone())
            .next()
            .or_else(|| {
                self.photos
                    .iter()
                    .take(self.current)
                    .map(|photo| photo.display.clone())
                    .next()
            });
        self.notice = format!(
            "{} {} file(s)…",
            if kind == ActionKind::Keep {
                "Copying"
            } else {
                "Moving"
            },
            files.len()
        );
        let repaint = ctx.clone();
        thread::spawn(move || {
            let result = process_files(
                kind,
                root,
                destination,
                files,
                photo_files,
                photos,
                progress_sender,
            );
            let _ = sender.send(OperationMessage::Complete(result));
            repaint.request_repaint();
        });
    }

    fn poll_operations(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.operation_receiver.try_recv() {
            match message {
                OperationMessage::Progress(notice) => {
                    self.notice = notice;
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                OperationMessage::Complete(result) => {
                    self.operation_pending = false;
                    if result.is_undo {
                        if result.kind == ActionKind::Delete && !result.restored_photos.is_empty() {
                            let restored = result.restored_photos;
                            self.photos.extend(restored.clone());
                            self.photos_changed();
                            self.photos.sort_by(|left, right| {
                                left.subfolder.cmp(&right.subfolder).then_with(|| {
                                    left.display.file_name().cmp(&right.display.file_name())
                                })
                            });
                            self.selection.clear();
                            if let Some(index) = self
                                .photos
                                .iter()
                                .position(|photo| photo.display == restored[0].display)
                            {
                                self.current = index;
                                self.mode = ViewMode::Viewer;
                                let display = self.photos[index].display.clone();
                                self.texture(
                                    ctx,
                                    &display,
                                    VIEW_PREVIEW,
                                    ACTIVE_VIEW_PREVIEW_PRIORITY,
                                );
                            }
                        } else if result.kind == ActionKind::Keep {
                            for photo in &mut self.photos {
                                if photo.files.iter().any(|file| {
                                    result
                                        .history
                                        .photos
                                        .iter()
                                        .any(|before| before.files.contains(file))
                                }) {
                                    photo.kept =
                                        result.history.kept_before.contains(&photo.display);
                                }
                            }
                        }
                        self.notice = if result.errors.is_empty() {
                            "Last keep/delete operation undone.".into()
                        } else {
                            format!("Undo incomplete: {}", result.errors.join("; "))
                        };
                    } else {
                        if !result.history.files.is_empty() {
                            self.history.push(result.history);
                        }
                        self.notice = if result.errors.is_empty() {
                            format!(
                                "{} file(s) {}",
                                result.completed.len(),
                                if result.kind == ActionKind::Keep {
                                    "copied"
                                } else {
                                    "moved"
                                }
                            )
                        } else {
                            format!(
                                "Some files could not be processed: {}",
                                result.errors.join("; ")
                            )
                        };
                        let done: HashSet<_> = result.completed;
                        let successful_photos: HashSet<_> = result
                            .photo_files
                            .into_iter()
                            .filter(|(_, files)| files.iter().all(|file| done.contains(file)))
                            .map(|(display, _)| display)
                            .collect();
                        let current_display = self
                            .photos
                            .get(self.current)
                            .map(|photo| photo.display.clone());
                        if result.kind == ActionKind::Keep {
                            for photo in &mut self.photos {
                                if successful_photos.contains(&photo.display) {
                                    photo.kept = true;
                                }
                            }
                        } else {
                            self.photos
                                .retain(|photo| !successful_photos.contains(&photo.display));
                            self.photos_changed();
                        }
                        self.selection.clear();
                        let available: HashSet<_> = self
                            .photos
                            .iter()
                            .map(|photo| photo.display.clone())
                            .collect();
                        self.forget_missing_textures(&available);
                        if let Some(next) = self.action_next.take().or(current_display) {
                            if let Some(index) =
                                self.photos.iter().position(|photo| photo.display == next)
                            {
                                self.current = index;
                            } else {
                                self.current =
                                    self.current.min(self.photos.len().saturating_sub(1));
                            }
                        } else {
                            self.current = self.current.min(self.photos.len().saturating_sub(1));
                        }
                        if self.photos.is_empty() {
                            self.mode = ViewMode::Grid;
                            self.current = 0;
                        } else if self.action_origin == Some(ViewMode::Viewer) {
                            self.mode = ViewMode::Viewer;
                        }
                    }
                    if result.kind == ActionKind::Keep || result.kind == ActionKind::Delete {
                        self.refresh_kept_status(ctx);
                    }
                    self.action_origin = None;
                }
            }
        }
        if self.operation_pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn undo(&mut self, ctx: &egui::Context) {
        if self.operation_pending {
            return;
        }
        let Some(entry) = self.history.pop() else {
            self.notice = "Nothing to undo.".into();
            return;
        };
        self.operation_pending = true;
        self.action_origin = Some(self.mode);
        self.action_next = None;
        self.notice = "Undoing last operation…".into();
        let sender = self.operation_sender.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let mut errors = Vec::new();
            for item in entry.files.iter().rev() {
                let result = match entry.kind {
                    ActionKind::Keep => fs::remove_file(&item.destination),
                    ActionKind::Delete => {
                        if let Some(parent) = item.original.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        move_file(&item.destination, &item.original)
                    }
                };
                if let Err(error) = result {
                    errors.push(format!("{}: {error}", item.destination.display()));
                }
            }
            let restored_photos = if entry.kind == ActionKind::Delete {
                entry
                    .photos
                    .iter()
                    .cloned()
                    .filter(|photo| photo.files.iter().all(|file| file.exists()))
                    .collect()
            } else {
                Vec::new()
            };
            let result = OperationResult {
                kind: entry.kind,
                history: entry,
                completed: HashSet::new(),
                photo_files: Vec::new(),
                errors,
                is_undo: true,
                restored_photos,
            };
            let _ = sender.send(OperationMessage::Complete(result));
            repaint.request_repaint();
        });
    }

    fn open_settings(&mut self) {
        if !self.settings_open {
            self.settings_snapshot = Some(self.settings.clone());
        }
        self.settings_open = true;
    }

    fn cancel_settings(&mut self) {
        if let Some(settings) = self.settings_snapshot.take() {
            self.settings = settings;
        }
        self.settings_open = false;
        self.pending_action = None;
    }

    fn show_grid(&mut self) {
        if self.mode == ViewMode::Viewer && self.current < self.photos.len() {
            self.selection.clear();
            self.selection.insert(self.current);
            self.scroll_to_selection = true;
        }
        self.mode = ViewMode::Grid;
    }

    fn next_photo(&mut self) {
        if !self.photos.is_empty() {
            self.current = (self.current + 1) % self.photos.len();
        }
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    fn previous_photo(&mut self) {
        if !self.photos.is_empty() {
            self.current = if self.current == 0 {
                self.photos.len() - 1
            } else {
                self.current - 1
            };
        }
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    fn leave_photo(&mut self) {
        if self.mode == ViewMode::Viewer {
            self.next_photo();
        } else if !self.photos.is_empty() {
            let current = self
                .anchor
                .or_else(|| self.selection.iter().next().copied())
                .unwrap_or(0);
            let next = (current + 1) % self.photos.len();
            self.select(next, Modifiers::default());
            self.current = next;
            self.scroll_to_selection = true;
        }
    }

    fn select(&mut self, index: usize, modifiers: Modifiers) {
        if modifiers.shift {
            if let Some(anchor) = self.anchor {
                self.selection.clear();
                for value in anchor.min(index)..=anchor.max(index) {
                    self.selection.insert(value);
                }
            } else {
                self.selection.insert(index);
                self.anchor = Some(index);
            }
        } else if modifiers.ctrl || modifiers.command {
            if !self.selection.insert(index) {
                self.selection.remove(&index);
            }
            self.anchor = Some(index);
        } else {
            self.selection.clear();
            self.selection.insert(index);
            self.anchor = Some(index);
        }
    }

    fn shortcut_matches(value: &str, key: Key) -> bool {
        let value = value.trim();
        match value.to_ascii_lowercase().as_str() {
            "backspace" => key == Key::Backspace,
            "delete" => key == Key::Delete,
            "space" => key == Key::Space,
            "enter" => key == Key::Enter,
            "left" => key == Key::ArrowLeft,
            "right" => key == Key::ArrowRight,
            "up" => key == Key::ArrowUp,
            "down" => key == Key::ArrowDown,
            _ => value.eq_ignore_ascii_case(&format!("{key:?}")),
        }
    }
}

impl SorterApp {
    /// Paints only the rows that intersect the viewport and streams thumbnails in around it:
    /// the cost of a frame depends on the screen size, not on the number of photos.
    fn show_grid_rows(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
    ) -> Option<(usize, Modifiers)> {
        use egui::text::{LayoutJob, TextWrapping};
        let mut clicked = None;
        let spacing = ui.spacing().item_spacing;
        let name_font = egui::TextStyle::Body.resolve(ui.style());
        let date_font = egui::FontId::proportional(10.0);
        let title_font = egui::FontId::proportional(20.0);
        let range_font = egui::FontId::proportional(13.0);
        let (name_height, date_height, title_height) = ui.fonts(|fonts| {
            (
                fonts.row_height(&name_font),
                fonts.row_height(&date_font),
                fonts.row_height(&title_font),
            )
        });
        let text_color = ui.visuals().text_color();
        let strong_color = ui.visuals().strong_text_color();
        let separator = ui.visuals().widgets.noninteractive.bg_stroke;
        let show_filenames = self.settings.show_filenames;
        let show_datetime = self.settings.show_datetime;
        let root_name = self
            .root
            .as_ref()
            .and_then(|root| root.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".into());
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                let width = ui.available_width();
                let metrics = GridMetrics {
                    width,
                    gap_x: spacing.x,
                    gap_y: spacing.y,
                    name_height: name_height + 2.0,
                    date_height: date_height + 2.0,
                    header_height: 8.0 + title_height + spacing.y * 2.0 + 1.0,
                };
                let mut layout = match self.grid_layout.take() {
                    Some(layout)
                        if layout.matches(
                            self.photos_version,
                            &metrics,
                            show_filenames,
                            show_datetime,
                        ) =>
                    {
                        layout
                    }
                    _ => {
                        let groups = group_ranges(&self.photos, |photo| &photo.subfolder);
                        GridLayout::build(
                            &groups,
                            self.photos.len(),
                            self.photos_version,
                            metrics,
                            show_filenames,
                            show_datetime,
                        )
                    }
                };
                self.grid_columns = layout.columns;
                let origin = ui.cursor().min;
                ui.allocate_exact_size(
                    egui::vec2(width, layout.total_height),
                    egui::Sense::hover(),
                );
                if viewport.min.y != self.grid_last_offset {
                    self.grid_scroll_down = viewport.min.y > self.grid_last_offset;
                    self.grid_last_offset = viewport.min.y;
                }
                if self.scroll_to_selection && self.selection.len() == 1 {
                    if let Some(&index) = self.selection.iter().next() {
                        if index < self.photos.len() {
                            let item = layout.items[layout.photo_item[index] as usize];
                            let inside = item.top >= viewport.min.y
                                && item.top + item.height <= viewport.max.y;
                            if !inside {
                                let rect = egui::Rect::from_min_size(
                                    egui::pos2(origin.x, origin.y + item.top),
                                    egui::vec2(width, item.height),
                                );
                                ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                ctx.request_repaint();
                            }
                        }
                    }
                }
                let visible = layout.visible_items(viewport.min.y, viewport.max.y);
                for item_index in visible.clone() {
                    let item = layout.items[item_index];
                    let top = origin.y + item.top;
                    if item.kind == GridItemKind::Header {
                        let folder = &self.photos[item.start].subfolder;
                        let title = if folder.as_os_str().is_empty() {
                            root_name.clone()
                        } else {
                            folder.display().to_string()
                        };
                        let range = match layout.ranges.get(&item.start) {
                            Some((version, label)) if *version == self.dates_version => {
                                label.clone()
                            }
                            _ => {
                                let label = date_range_label(&self.photos[item.start..item.end]);
                                layout
                                    .ranges
                                    .insert(item.start, (self.dates_version, label.clone()));
                                label
                            }
                        };
                        let painter = ui.painter();
                        let title_rect = painter.text(
                            egui::pos2(origin.x, top + 8.0),
                            egui::Align2::LEFT_TOP,
                            title,
                            title_font.clone(),
                            strong_color,
                        );
                        if let Some(range) = range {
                            painter.text(
                                egui::pos2(title_rect.right() + 48.0, title_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                range,
                                range_font.clone(),
                                strong_color,
                            );
                        }
                        let line_y = top + item.height - spacing.y - 0.5;
                        painter.hline(origin.x..=origin.x + width, line_y, separator);
                        continue;
                    }
                    for photo_index in item.start..item.end {
                        let column = photo_index - item.start;
                        let x = origin.x + column as f32 * (layout.cell_width + spacing.x);
                        let cell_rect = egui::Rect::from_min_size(
                            egui::pos2(x, top),
                            egui::vec2(layout.cell_width, layout.cell_height),
                        );
                        let thumb_rect = egui::Rect::from_min_size(
                            egui::pos2(x, top + layout.text_height),
                            egui::vec2(layout.cell_width, layout.thumb_height),
                        );
                        let path = self.photos[photo_index].display.clone();
                        let kept = self.photos[photo_index].kept;
                        let texture = self.grid_thumbnail(ctx, &path, GRID_VISIBLE_PRIORITY);
                        let selected = self.selection.contains(&photo_index);
                        let mut text_top = top;
                        let painter = ui.painter();
                        if show_filenames {
                            let kept_width = if kept {
                                let galley = painter.layout_no_wrap(
                                    "Kept".into(),
                                    name_font.clone(),
                                    text_color,
                                );
                                let kept_width = galley.size().x;
                                painter.galley(
                                    egui::pos2(cell_rect.right() - kept_width, text_top),
                                    galley,
                                    text_color,
                                );
                                kept_width + spacing.x
                            } else {
                                0.0
                            };
                            let name = path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            let mut job =
                                LayoutJob::simple_singleline(name, name_font.clone(), text_color);
                            job.wrap = TextWrapping::truncate_at_width(
                                (layout.cell_width - kept_width).max(10.0),
                            );
                            let galley = painter.layout_job(job);
                            painter.galley(egui::pos2(x, text_top), galley, text_color);
                            text_top += metrics.name_height;
                        }
                        if show_datetime {
                            let photo = &self.photos[photo_index];
                            let label = match photo.datetime.as_deref() {
                                Some(label) => label,
                                None if photo.datetime_loaded => "No EXIF datetime",
                                None => "…",
                            };
                            painter.text(
                                egui::pos2(x, text_top),
                                egui::Align2::LEFT_TOP,
                                label,
                                date_font.clone(),
                                text_color,
                            );
                        }
                        if let Some(texture) = texture {
                            let image_size = texture.size_vec2();
                            let fit = (thumb_rect.width() / image_size.x)
                                .min(thumb_rect.height() / image_size.y);
                            painter.image(
                                texture.id(),
                                egui::Rect::from_center_size(thumb_rect.center(), image_size * fit),
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        } else {
                            painter.rect_filled(thumb_rect, 2.0, egui::Color32::from_gray(48));
                            let key = self.cached_texture_key(&path, VIEW_PREVIEW);
                            if self.failed_textures.contains(&key) {
                                ui.painter().text(
                                    thumb_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "Preview unavailable",
                                    egui::FontId::default(),
                                    egui::Color32::WHITE,
                                );
                            }
                        }
                        if selected {
                            ui.painter().rect_stroke(
                                thumb_rect,
                                2.0,
                                egui::Stroke::new(3.0_f32, egui::Color32::LIGHT_BLUE),
                                egui::StrokeKind::Inside,
                            );
                        }
                        let id = ui.id().with(("grid_cell", photo_index));
                        let mut response = ui.interact(cell_rect, id, egui::Sense::click());
                        if kept {
                            response = response.on_hover_text("Copied to keep folder");
                        }
                        if response.clicked() {
                            clicked = Some((photo_index, ui.input(|input| input.modifiers)));
                        }
                        if response.double_clicked() {
                            self.current = photo_index;
                            self.mode = ViewMode::Viewer;
                            self.last_viewer_image = None;
                        }
                    }
                }
                self.stream_grid_neighbours(ctx, &layout, visible, viewport.height());
                self.grid_layout = Some(layout);
            });
        clicked
    }

    /// Loads what is around the viewport (more in the scrolling direction) and cancels what
    /// is far away, so scrolling reveals thumbnails that are already there.
    fn stream_grid_neighbours(
        &mut self,
        ctx: &egui::Context,
        layout: &GridLayout,
        visible: std::ops::Range<usize>,
        viewport_height: f32,
    ) {
        if layout.items.is_empty() {
            return;
        }
        let top = layout.items[visible.start.min(layout.items.len() - 1)].top;
        let bottom = layout.items[visible.end.saturating_sub(1).min(layout.items.len() - 1)].top;
        let (before, after) = if self.grid_scroll_down {
            (1.0, 3.0)
        } else {
            (3.0, 1.0)
        };
        let near = layout.visible_items(
            top - before * viewport_height,
            bottom + after * viewport_height,
        );
        let window = (near.start, near.end);
        if window == self.grid_window && self.grid_prefetch_at.elapsed() < GRID_PREFETCH_INTERVAL {
            return;
        }
        let window_changed = window != self.grid_window;
        self.grid_window = window;
        self.grid_prefetch_at = std::time::Instant::now();
        // Items ordered by usefulness: visible, then ahead of the scroll, then behind.
        let ahead: Vec<usize> = if self.grid_scroll_down {
            (visible.end..near.end).collect()
        } else {
            (near.start..visible.start).rev().collect()
        };
        let behind: Vec<usize> = if self.grid_scroll_down {
            (near.start..visible.start).rev().collect()
        } else {
            (visible.end..near.end).collect()
        };
        let mut wanted: HashSet<PathBuf> = HashSet::new();
        let mut on_screen: HashSet<PathBuf> = HashSet::new();
        let mut dates: Vec<PathBuf> = Vec::new();
        let mut requests: Vec<(PathBuf, u8)> = Vec::new();
        let groups = [
            (visible.clone().collect::<Vec<_>>(), GRID_VISIBLE_PRIORITY),
            (ahead, GRID_NEAR_PRIORITY),
            (behind, GRID_NEAR_PRIORITY - 10),
        ];
        'collect: for (items, priority) in groups {
            for item_index in items {
                let item = layout.items[item_index];
                if item.kind != GridItemKind::Row {
                    continue;
                }
                for photo in &self.photos[item.start..item.end] {
                    if wanted.len() >= GRID_PREFETCH_LIMIT {
                        break 'collect;
                    }
                    wanted.insert(photo.display.clone());
                    if priority == GRID_VISIBLE_PRIORITY {
                        on_screen.insert(photo.display.clone());
                    }
                    requests.push((photo.display.clone(), priority));
                    if !photo.datetime_loaded {
                        dates.push(photo.display.clone());
                    }
                }
            }
        }
        if window_changed {
            self.retain_grid_thumbnails(&wanted, &on_screen);
        }
        for (path, priority) in requests {
            self.grid_thumbnail(ctx, &path, priority);
        }
        if !dates.is_empty() {
            self.prioritize_datetimes(dates);
        }
    }
}

impl eframe::App for SorterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan(ctx);
        self.poll_kept_status(ctx);
        self.poll_textures(ctx);
        self.poll_operations(ctx);
        let keep_shortcut = self.settings.keep_shortcut.clone();
        let leave_shortcut = self.settings.leave_shortcut.clone();
        let delete_shortcut = self.settings.delete_shortcut.clone();
        let open_settings =
            ctx.input(|input| input.key_pressed(Key::Comma) && input.modifiers.ctrl);
        if open_settings {
            self.open_settings();
        }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(8.0);
                if ui.button("Open folder").clicked() {
                    self.choose_folder(ctx);
                }
                if ui.button("Grid").clicked() {
                    self.show_grid();
                }
                if ui
                    .checkbox(&mut self.settings.show_filenames, "Filenames")
                    .changed()
                {
                    self.save_settings();
                }
                if ui
                    .checkbox(&mut self.settings.show_datetime, "Date/time")
                    .changed()
                {
                    self.save_settings();
                }
                if ui.checkbox(&mut self.settings.show_exif, "EXIF").changed() {
                    self.save_settings();
                }
                if ui.button(format!("Keep ({})", keep_shortcut)).clicked() {
                    self.sort_photos(ActionKind::Keep, ctx);
                }
                if ui.button(format!("Leave ({})", leave_shortcut)).clicked() {
                    self.leave_photo();
                }
                if ui.button(format!("Delete ({})", delete_shortcut)).clicked() {
                    self.sort_photos(ActionKind::Delete, ctx);
                }
                if ui.button("Undo (Ctrl+Z)").clicked() {
                    self.undo(ctx);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.open_settings();
                    }
                });
            });
            ui.add_space(4.0);
        });
        if self.mode == ViewMode::Grid {
            egui::TopBottomPanel::bottom("grid_actions").show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    if ui.button("Keep selected").clicked() {
                        self.sort_photos(ActionKind::Keep, ctx);
                    }
                    if ui.button("Delete selected").clicked() {
                        self.sort_photos(ActionKind::Delete, ctx);
                    }
                    ui.label(format!("{} selected", self.selection.len()));
                    ui.label(status_text(&self.notice));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
                    });
                });
                ui.add_space(4.0);
            });
        } else {
            egui::TopBottomPanel::bottom("viewer_status").show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(status_text(&self.notice));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
                    });
                });
                ui.add_space(4.0);
            });
        }
        if !ctx.wants_keyboard_input() && !self.settings_open {
            let (pressed, right, left, direction, modifiers) = ctx.input(|input| {
                let pressed = input
                    .events
                    .iter()
                    .filter_map(|event| match event {
                        egui::Event::Key {
                            key, pressed: true, ..
                        } => Some(*key),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let direction = if input.key_pressed(Key::ArrowLeft) {
                    Some(-1isize)
                } else if input.key_pressed(Key::ArrowRight) {
                    Some(1)
                } else if input.key_pressed(Key::ArrowUp) {
                    Some(-(self.grid_columns as isize))
                } else if input.key_pressed(Key::ArrowDown) {
                    Some(self.grid_columns as isize)
                } else {
                    None
                };
                (
                    pressed,
                    input.key_pressed(Key::ArrowRight),
                    input.key_pressed(Key::ArrowLeft),
                    direction,
                    input.modifiers,
                )
            });
            let keep_pressed = pressed
                .iter()
                .any(|key| Self::shortcut_matches(&self.settings.keep_shortcut, *key));
            let leave_pressed = pressed
                .iter()
                .any(|key| Self::shortcut_matches(&self.settings.leave_shortcut, *key));
            let delete_pressed = pressed
                .iter()
                .any(|key| Self::shortcut_matches(&self.settings.delete_shortcut, *key));
            let open_pressed = pressed
                .iter()
                .any(|key| matches!(key, Key::Enter | Key::Space));
            if self.mode == ViewMode::Grid {
                if keep_pressed {
                    self.sort_photos(ActionKind::Keep, ctx);
                } else if leave_pressed {
                    self.leave_photo();
                } else if delete_pressed {
                    self.sort_photos(ActionKind::Delete, ctx);
                } else if open_pressed {
                    if !self.photos.is_empty() {
                        self.current = self
                            .anchor
                            .or_else(|| self.selection.iter().next().copied())
                            .unwrap_or(self.current)
                            .min(self.photos.len() - 1);
                        self.mode = ViewMode::Viewer;
                        self.last_viewer_image = None;
                    }
                } else if let Some(direction) = direction {
                    if !self.photos.is_empty() {
                        let current = self
                            .anchor
                            .or_else(|| self.selection.iter().next().copied())
                            .unwrap_or(0)
                            .min(self.photos.len() - 1);
                        let target = (current as isize + direction)
                            .clamp(0, self.photos.len() as isize - 1)
                            as usize;
                        self.select(target, modifiers);
                        self.current = target;
                        self.scroll_to_selection = true;
                    }
                }
            } else if keep_pressed {
                self.sort_photos(ActionKind::Keep, ctx);
            } else if leave_pressed {
                self.leave_photo();
            } else if delete_pressed {
                self.sort_photos(ActionKind::Delete, ctx);
            } else {
                if ctx.input(|input| {
                    input.key_pressed(Key::Escape) || input.key_pressed(Key::Space)
                }) {
                    self.show_grid();
                }
                if right {
                    self.next_photo();
                }
                if left {
                    self.previous_photo();
                }
            }
        }
        let undo_pressed = ctx.input(|input| input.key_pressed(Key::Z) && input.modifiers.ctrl);
        if undo_pressed {
            self.undo(ctx);
        }
        if self.settings_open {
            let response = egui::Modal::new(egui::Id::new("settings_modal")).show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.heading("Settings");
                ui.label("Keep destination folder");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.settings.keep_folder);
                    if ui.button("Browse…").clicked() {
                        if let Ok(Some(path)) =
                            native_dialog::FileDialog::new().show_open_single_dir()
                        {
                            self.settings.keep_folder = path.display().to_string();
                        }
                    }
                });
                ui.label("Delete destination folder");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.settings.delete_folder);
                    if ui.button("Browse…").clicked() {
                        if let Ok(Some(path)) =
                            native_dialog::FileDialog::new().show_open_single_dir()
                        {
                            self.settings.delete_folder = path.display().to_string();
                        }
                    }
                });
                ui.separator();
                ui.label("Viewer shortcuts");
                ui.horizontal(|ui| {
                    ui.label("Keep");
                    ui.text_edit_singleline(&mut self.settings.keep_shortcut);
                });
                ui.horizontal(|ui| {
                    ui.label("Leave");
                    ui.text_edit_singleline(&mut self.settings.leave_shortcut);
                });
                ui.horizontal(|ui| {
                    ui.label("Delete");
                    ui.text_edit_singleline(&mut self.settings.delete_shortcut);
                });
                ui.horizontal(|ui| {
                    if self.pending_action.is_some_and(|kind| match kind {
                        ActionKind::Keep => !self.settings.keep_folder.trim().is_empty(),
                        ActionKind::Delete => !self.settings.delete_folder.trim().is_empty(),
                    }) && ui.button("Continue").clicked()
                    {
                        let kind = self.pending_action.take().unwrap();
                        self.save_settings();
                        self.settings_snapshot = None;
                        self.refresh_kept_status(ctx);
                        self.settings_open = false;
                        self.sort_photos(kind, ctx);
                    }
                    if ui.button("Save").clicked() {
                        self.save_settings();
                        self.settings_snapshot = None;
                        self.settings_open = false;
                        self.refresh_kept_status(ctx);
                        if self.pending_action.is_some_and(|kind| match kind {
                            ActionKind::Keep => !self.settings.keep_folder.trim().is_empty(),
                            ActionKind::Delete => !self.settings.delete_folder.trim().is_empty(),
                        }) {
                            let kind = self.pending_action.take().unwrap();
                            self.sort_photos(kind, ctx);
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.cancel_settings();
                    }
                });
            });
            if response.should_close() {
                self.cancel_settings();
            }
        }

        if self.mode == ViewMode::Viewer {
            egui::CentralPanel::default().show(ctx, |ui| {
                if self.photos.is_empty() {
                    ui.centered_and_justified(|ui| ui.label("No images to display."));
                    return;
                }
                let photo = self.photos[self.current].clone();
                if self.settings.show_filenames || self.settings.show_datetime {
                    egui::TopBottomPanel::top("viewer_info").show_inside(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if self.settings.show_filenames {
                                ui.label(
                                    photo
                                        .display
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy(),
                                );
                            }
                            if self.settings.show_datetime {
                                ui.label(match photo.datetime.as_deref() {
                                    Some(label) => label,
                                    None if photo.datetime_loaded => "No EXIF datetime",
                                    None => "…",
                                });
                            }
                            if photo.kept {
                                ui.label("Kept").on_hover_text(
                                    "This photo has been copied to the keep folder.",
                                );
                            }
                        });
                    });
                }
                if self.last_viewer_image.as_ref() != Some(&photo.display) {
                    self.prioritize_viewer_image(&photo.display);
                    self.last_viewer_image = Some(photo.display.clone());
                }
                if self.settings.show_exif && !photo.exif_loaded {
                    self.request_exif(ctx, &photo.display, self.scan_generation);
                }
                if !photo.datetime_loaded {
                    self.prioritize_datetimes(vec![photo.display.clone()]);
                }
                egui::TopBottomPanel::bottom("viewer_actions").show_inside(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Previous").clicked() {
                            self.previous_photo();
                        }
                        ui.label(format!("{} / {}", self.current + 1, self.photos.len()));
                        if ui.button("Next").clicked() {
                            self.next_photo();
                        }
                        if ui.button("Reset zoom").clicked() {
                            self.zoom = 1.0;
                            self.pan = Vec2::ZERO;
                        }
                        if ui
                            .button(format!("Keep ({})", self.settings.keep_shortcut))
                            .clicked()
                        {
                            self.sort_photos(ActionKind::Keep, ctx);
                        }
                        if ui
                            .button(format!("Leave ({})", self.settings.leave_shortcut))
                            .clicked()
                        {
                            self.next_photo();
                        }
                        if ui
                            .button(format!("Delete ({})", self.settings.delete_shortcut))
                            .clicked()
                        {
                            self.sort_photos(ActionKind::Delete, ctx);
                        }
                    });
                });
                egui::SidePanel::right("exif_panel")
                    .resizable(true)
                    .default_width(340.0)
                    .show_animated_inside(ui, self.settings.show_exif, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.heading("EXIF");
                        show_exif_fields(ui, &photo, &mut self.expanded_exif);
                    });
                let available = ui.available_size();
                let (rect, response) =
                    ui.allocate_exact_size(available, egui::Sense::click_and_drag());
                if response.double_clicked() {
                    self.show_grid();
                }
                if response.middle_clicked() {
                    self.zoom = 1.0;
                    self.pan = Vec2::ZERO;
                }
                let scroll = if response.hovered() {
                    ctx.input(|input| input.raw_scroll_delta.y)
                } else {
                    0.0
                };
                if scroll != 0.0 {
                    self.zoom = (self.zoom * (scroll / 300.0).exp()).clamp(0.1, 8.0);
                }
                let source_size = self.source_size(ctx, &photo.display);
                let viewport = (
                    (rect.width() * ctx.pixels_per_point()).round().max(1.0) as u32,
                    (rect.height() * ctx.pixels_per_point()).round().max(1.0) as u32,
                );
                let requested_size = if source_size == VIEW_PREVIEW && !self.source_sizes.contains_key(&photo.display) {
                    VIEW_PREVIEW
                } else {
                    viewer_request_size(source_size, viewport, self.zoom)
                };
                if self.zoom <= 1.0 && self.photos.len() > 1 {
                    let count = self.photos.len();
                    let mut neighbours: Vec<(usize, u8)> = Vec::new();
                    for distance in 1..=PREFETCH_AHEAD.max(PREFETCH_BEHIND) {
                        let step = (distance - 1) as u8 * 8;
                        if distance <= PREFETCH_AHEAD {
                            neighbours.push((
                                (self.current + distance) % count,
                                PREFETCH_PRIORITY.saturating_sub(step),
                            ));
                        }
                        if distance <= PREFETCH_BEHIND {
                            neighbours.push((
                                (self.current + count - distance % count) % count,
                                (PREFETCH_PRIORITY - 4).saturating_sub(step),
                            ));
                        }
                    }
                    let mut seen = HashSet::from([self.current]);
                    neighbours.retain(|(index, _)| seen.insert(*index));
                    let wanted: HashSet<PathBuf> = neighbours
                        .iter()
                        .map(|(index, _)| self.photos[*index].display.clone())
                        .collect();
                    self.retain_prefetch(&wanted);
                    for (neighbour, priority) in neighbours {
                        let path = self.photos[neighbour].display.clone();
                        if !self.source_sizes.contains_key(&path) {
                            self.request_source_info(ctx, &path);
                            continue;
                        }
                        let source = self.source_size(ctx, &path);
                        self.texture(
                            ctx,
                            &path,
                            viewer_request_size(source, viewport, 1.0),
                            priority.max(PREFETCH_MIN_PRIORITY),
                        );
                    }
                }
                let fit = (rect.width() / requested_size.0 as f32)
                    .min(rect.height() / requested_size.1 as f32);
                let display_size =
                    Vec2::new(requested_size.0 as f32, requested_size.1 as f32) * fit * self.zoom;
                if response.dragged_by(PointerButton::Primary) {
                    let delta = response.drag_delta();
                    self.pan += delta;
                    let margin = (display_size * 0.1).min(rect.size() * 0.25);
                    let max_pan = Vec2::new(
                        (rect.width() + display_size.x) * 0.5 - margin.x,
                        (rect.height() + display_size.y) * 0.5 - margin.y,
                    )
                    .max(Vec2::ZERO);
                    self.pan.x = self.pan.x.clamp(-max_pan.x, max_pan.x);
                    self.pan.y = self.pan.y.clamp(-max_pan.y, max_pan.y);
                }
                self.texture(
                    ctx,
                    &photo.display,
                    requested_size,
                    ACTIVE_VIEW_IMAGE_PRIORITY,
                );
                self.texture(
                    ctx,
                    &photo.display,
                    VIEW_PREVIEW,
                    ACTIVE_VIEW_PREVIEW_PRIORITY,
                );
                let texture = self.viewer_texture(&photo.display, requested_size);
                let loading_image = self.queued_textures.keys().any(|key| {
                    key.path == photo.display
                        && key.width >= requested_size.0
                        && key.height >= requested_size.1
                });
                if let Some(texture) = texture {
                    let image_size = texture.size_vec2();
                    let fit = (rect.width() / image_size.x).min(rect.height() / image_size.y);
                    let size = image_size * fit * self.zoom;
                    let margin = (size * 0.1).min(rect.size() * 0.25);
                    let max_pan = Vec2::new(
                        (rect.width() + size.x) * 0.5 - margin.x,
                        (rect.height() + size.y) * 0.5 - margin.y,
                    )
                    .max(Vec2::ZERO);
                    self.pan.x = self.pan.x.clamp(-max_pan.x, max_pan.x);
                    self.pan.y = self.pan.y.clamp(-max_pan.y, max_pan.y);
                    let center = rect.center() + self.pan;
                    let image_rect = egui::Rect::from_center_size(center, size);
                    ui.painter().with_clip_rect(rect).image(
                        texture.id(),
                        image_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                    if loading_image {
                        let indicator_rect = egui::Rect::from_center_size(
                            rect.center(),
                            egui::vec2(180.0, 60.0),
                        );
                        loading_indicator(ui, indicator_rect, "Loading photo…");
                    }
                } else if loading_image {
                    loading_indicator(ui, rect, "Loading photo…");
                } else {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Could not load image preview",
                        egui::FontId::default(),
                        egui::Color32::WHITE,
                    );
                }
            });
        } else {
            egui::CentralPanel::default().show(ctx, |ui| {
                if self.photos.is_empty() {
                    if self.notice.starts_with("Scanning ") {
                        let rect = ui.max_rect();
                        loading_indicator(ui, rect, "Scanning folder…");
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label("Choose a folder to scan images recursively.")
                        });
                    }
                    return;
                }
                let mut pending_selection = None;
                let available = ui.available_size();
                let (grid_rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
                ui.scope_builder(egui::UiBuilder::new().max_rect(grid_rect), |ui| {
                    if self.settings.show_exif {
                        egui::SidePanel::right("exif_panel")
                            .resizable(true)
                            .default_width(340.0)
                            .show_inside(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.heading("Selected image EXIF");
                                let selected = if self.selection.len() == 1 {
                                    self.selection.iter().next().copied()
                                } else {
                                    None
                                };
                                if let Some(index) = selected {
                                    if self
                                        .photos
                                        .get(index)
                                        .is_some_and(|photo| !photo.exif_loaded)
                                    {
                                        let path = self.photos[index].display.clone();
                                        self.request_exif(ctx, &path, self.scan_generation);
                                    }
                                    if let Some(photo) = self.photos.get(index).cloned() {
                                        show_exif_fields(ui, &photo, &mut self.expanded_exif);
                                    }
                                } else {
                                    ui.label("Select one image to view its EXIF data.");
                                }
                            });
                    }
                    pending_selection = self.show_grid_rows(ui, ctx);
                });
                self.scroll_to_selection = false;
                if let Some((index, modifiers)) = pending_selection {
                    self.select(index, modifiers);
                }
            });
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

fn main() -> eframe::Result {
    let icon = image::load_from_memory(include_bytes!("../icon/icon.png"))
        .expect("failed to load embedded app icon")
        .to_rgba8();
    let (width, height) = icon.dimensions();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("image_sorter")
            .with_inner_size(egui::vec2(1440.0, 900.0))
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "Image Sorter",
        options,
        Box::new(|creation_context| {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "Figtree".into(),
                egui::FontData::from_static(include_bytes!("../assets/fonts/Figtree.ttf")).into(),
            );
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "Figtree".into());
            creation_context.egui_ctx.set_fonts(fonts);
            let mut style = (*creation_context.egui_ctx.style()).clone();
            for font_id in style.text_styles.values_mut() {
                font_id.size *= 1.08;
            }
            style.spacing.button_padding = egui::vec2(11.0, 4.0);
            style.spacing.interact_size = egui::vec2(40.0, 24.0);
            creation_context.egui_ctx.set_style(style);
            Ok(Box::<SorterApp>::default())
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_orientation_rotates_and_mirrors_images() {
        let pixels = image::RgbImage::from_fn(2, 3, |x, y| image::Rgb([(y * 2 + x) as u8, 0, 0]));
        let rotated =
            apply_exif_orientation(image::DynamicImage::ImageRgb8(pixels.clone()), 6).to_rgb8();
        assert_eq!(rotated.dimensions(), (3, 2));
        assert_eq!(rotated.get_pixel(2, 0)[0], 0);
        assert_eq!(rotated.get_pixel(0, 1)[0], 5);
        assert_eq!(oriented_dimensions((2, 3), 6), (3, 2));
        assert_eq!(oriented_dimensions((2, 3), 8), (3, 2));
        assert_eq!(oriented_dimensions((2, 3), 4), (2, 3));
    }

    #[test]
    fn shortcuts_match_configured_single_and_named_keys() {
        assert!(SorterApp::shortcut_matches("k", Key::K));
        assert!(SorterApp::shortcut_matches("F1", Key::F1));
        assert!(SorterApp::shortcut_matches("Backspace", Key::Backspace));
        assert!(SorterApp::shortcut_matches("up", Key::ArrowUp));
        assert!(!SorterApp::shortcut_matches("F2", Key::F1));
    }

    #[test]
    fn kept_status_matches_relative_file_path_and_pair_member() {
        let root = PathBuf::from("/photos/source");
        let keep = PathBuf::from("/photos/keep");
        let photo = Photo {
            display: root.join("trip/photo.JPG"),
            files: vec![root.join("trip/photo.JPG"), root.join("trip/photo.ARW")],
            subfolder: PathBuf::from("trip"),
            datetime: None,
            exif: Vec::new(),
            exif_loaded: false,
            datetime_loaded: false,
            kept: false,
        };
        let raw_copy = keep.join("trip/photo.ARW");
        assert!(is_kept(&photo, &root, &keep, &HashSet::from([raw_copy])));
        assert!(!is_kept(&photo, &root, &keep, &HashSet::new()));
    }

    #[test]
    fn known_exif_tags_have_human_readable_names() {
        assert_eq!(
            exif_tag_name(exif::Tag::DateTimeOriginal),
            "Date and time of original data generation"
        );
        assert_eq!(exif_tag_name(exif::Tag::FNumber), "F number");
        assert!(exif_tag_name(exif::Tag(exif::Context::Exif, 0xC7A1)).contains("0xC7A1"));
    }

    #[test]
    fn folder_date_range_shows_single_date_or_span() {
        let photo = |datetime: Option<&str>| Photo {
            display: PathBuf::from("a.jpg"),
            files: Vec::new(),
            subfolder: PathBuf::new(),
            datetime: datetime.map(str::to_owned),
            exif: Vec::new(),
            exif_loaded: false,
            datetime_loaded: false,
            kept: false,
        };
        let same = [
            photo(Some("2024-05-01 10:00:00")),
            photo(Some("2024-05-01 18:30:00")),
            photo(None),
        ];
        assert_eq!(date_range_label(&same).as_deref(), Some("2024-05-01"));
        let span = [
            photo(Some("2024-05-03 10:00:00")),
            photo(Some("2024-04-28 18:30:00")),
        ];
        assert_eq!(
            date_range_label(&span).as_deref(),
            Some("2024-04-28 – 2024-05-03")
        );
        assert_eq!(date_range_label(&[photo(None)]), None);
    }

    #[test]
    fn long_exif_values_are_truncated_at_character_boundary() {
        let value = "é".repeat(60);
        let short: String = value.chars().take(50).collect();
        assert_eq!(short.chars().count(), 50);
        assert!(value.chars().count() > 50);
    }

    #[test]
    fn exif_names_have_no_id_and_are_categorized() {
        let field = exif_tag_name(exif::Tag::DateTimeOriginal);
        assert_eq!(field, "Date and time of original data generation");
        assert!(!field.contains("0x"));
        assert_eq!(exif_category(exif::Tag::FNumber), "Camera and optics");
        assert_eq!(
            exif_category(exif::Tag::DateTimeOriginal),
            "Date, time and position"
        );
        assert_eq!(
            exif_category(exif::Tag::GPSLatitude),
            "Date, time and position"
        );
        assert_eq!(exif_category(exif::Tag::PixelXDimension), "Image");
    }

    #[test]
    fn preview_queue_promotes_waiting_jobs_and_runs_most_urgent_first() {
        let pool = PreviewPool {
            queue: std::sync::Mutex::new(PreviewQueue::default()),
            ready: std::sync::Condvar::new(),
            cache_size: AtomicUsize::new(0),
        };
        let ctx = egui::Context::default();
        let key = |name: &str| texture_key(Path::new(name), VIEW_PREVIEW);
        assert!(enqueue_preview(&pool, &key("a"), VIEW_PREVIEW, 10, ctx.clone()));
        assert!(enqueue_preview(&pool, &key("b"), VIEW_PREVIEW, 20, ctx.clone()));
        assert!(enqueue_preview(&pool, &key("c"), VIEW_PREVIEW, 15, ctx.clone()));
        // "a" scrolls into view: it must jump ahead instead of being ignored.
        assert!(enqueue_preview(&pool, &key("a"), VIEW_PREVIEW, 50, ctx.clone()));
        let mut queue = pool.queue.lock().unwrap();
        assert_eq!(queue.jobs.len(), 3);
        let order: Vec<_> = std::iter::from_fn(|| take_best_job(&mut queue.jobs))
            .map(|job| job.key.path)
            .collect();
        assert_eq!(
            order,
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")]
        );
    }

    #[test]
    fn datetime_scan_serves_urgent_photos_first_and_never_twice() {
        let scan = DatetimeScan {
            state: std::sync::Mutex::new(DatetimeState {
                urgent: Vec::new(),
                background: vec!["c".into(), "b".into(), "a".into()],
                taken: HashSet::new(),
            }),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        };
        scan.prioritize(vec!["c".into(), "b".into()]);
        assert_eq!(scan.next(), Some(PathBuf::from("c")));
        assert_eq!(scan.next(), Some(PathBuf::from("b")));
        assert_eq!(scan.next(), Some(PathBuf::from("a")));
        assert_eq!(scan.next(), None);
        scan.prioritize(vec!["a".into()]);
        assert_eq!(scan.next(), None);
    }

    #[test]
    fn rw2_without_jpeg_uses_embedded_preview_with_orientation() {
        // Panasonic RW2: TIFF-like ("II" + 0x55), IFD0 tag 0x2E points at a full JPEG.
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(64, 32, image::Rgb([200, 30, 30])))
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg_offset = 4096u32;
        let mut file = vec![0u8; jpeg_offset as usize];
        file[..4].copy_from_slice(b"II\x55\x00");
        file[4..8].copy_from_slice(&24u32.to_le_bytes());
        file[24..26].copy_from_slice(&2u16.to_le_bytes());
        let entries = [
            (0x2eu16, 7u16, jpeg.len() as u32, jpeg_offset),
            (0x112, 3, 1, 6), // orientation: rotate 90 degrees
        ];
        for (index, (tag, kind, count, value)) in entries.iter().enumerate() {
            let at = 26 + index * 12;
            file[at..at + 2].copy_from_slice(&tag.to_le_bytes());
            file[at + 2..at + 4].copy_from_slice(&kind.to_le_bytes());
            file[at + 4..at + 8].copy_from_slice(&count.to_le_bytes());
            file[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
        }
        file.extend_from_slice(&jpeg);
        // Larger than the 256 KB head read, so the preview must be fetched by range.
        file.resize(file.len() + 400 * 1024, 7);
        let root = std::env::temp_dir().join(format!("image_sorter_rw2_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("P1.RW2");
        fs::write(&path, &file).unwrap();
        assert_eq!(rw2_locate(&file), Some((4096, jpeg.len(), 6)));
        assert_eq!(rw2_locate(b"II*\x00"), None);
        let thumbnail = decode_preview(&path, VIEW_PREVIEW).expect("RW2 thumbnail");
        assert_eq!(thumbnail.size, [32, 64], "rotated by orientation 6");
        assert_eq!(viewer_decode_size(&path), Some((32, 64)));
        let full = decode_preview(&path, (1920, 1440)).expect("RW2 full preview");
        assert_eq!(full.size, [32, 64]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn raw_is_only_developed_when_zooming_past_the_embedded_preview() {
        assert!(!rw2_needs_raw((1920, 1440), (1463, 1100)));
        assert!(!rw2_needs_raw((1920, 1440), (1920, 1440)));
        assert!(rw2_needs_raw((1920, 1440), (1921, 1440)));
        assert!(rw2_needs_raw((1920, 1440), (4608, 3464)));
        assert!(is_raw(Path::new("a.ARW")) && !is_raw(Path::new("a.jpg")));
    }

    #[test]
    fn scan_pairs_raw_and_jpeg_and_keeps_similar_stems_separate() {
        let root = std::env::temp_dir().join(format!("image_sorter_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let folder = root.join("trip");
        fs::create_dir_all(&folder).unwrap();
        for name in ["Photo.ARW", "photo.JPG", "other.NEF", "notes.txt"] {
            fs::write(folder.join(name), []).unwrap();
        }
        let photos = scan_folder(&root);
        assert_eq!(photos.len(), 2);
        let jpeg = photos
            .iter()
            .find(|photo| extension(&photo.display) == "jpg")
            .unwrap();
        assert_eq!(jpeg.files.len(), 2);
        assert_eq!(jpeg.display.file_name().unwrap(), "photo.JPG");
        assert!(jpeg.exif.is_empty());
        let raw = photos
            .iter()
            .find(|photo| extension(&photo.display) == "nef")
            .unwrap();
        assert_eq!(raw.files.len(), 1);
        assert_eq!(raw.subfolder, PathBuf::from("trip"));
        assert!(!jpeg.kept);

        let history = HistoryEntry {
            kind: ActionKind::Delete,
            files: vec![FileMove {
                original: raw.display.clone(),
                destination: PathBuf::from("deleted/other.NEF"),
            }],
            photos: vec![raw.clone()],
            kept_before: HashSet::new(),
        };
        let restored = history.photos.clone();
        assert_eq!(restored[0].display, raw.display);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_decode_outputs_bounded_thumbnail_and_cache_roundtrips() {
        let root =
            std::env::temp_dir().join(format!("image_sorter_preview_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("preview.png");
        let source_image = image::RgbaImage::from_pixel(800, 600, image::Rgba([20, 80, 140, 255]));
        source_image.save(&source).unwrap();
        let preview = decode_preview(&source, VIEW_PREVIEW).unwrap();
        assert_eq!(
            preview.size,
            [VIEW_PREVIEW.0 as usize, VIEW_PREVIEW.1 as usize]
        );
        let key = texture_key(&source, VIEW_PREVIEW);
        let cache_pool = PreviewPool {
            queue: std::sync::Mutex::new(PreviewQueue::default()),
            ready: std::sync::Condvar::new(),
            cache_size: AtomicUsize::new(0),
        };
        write_cached_preview(&root.join("cache"), &key, &preview, &cache_pool);
        let cached = read_cached_preview(&root.join("cache"), &key).unwrap();
        assert_eq!(
            cached.size,
            [VIEW_PREVIEW.0 as usize, VIEW_PREVIEW.1 as usize]
        );
        assert_eq!(cached.pixels, preview.pixels);

        let jpeg_path = root.join("big.jpg");
        let mut jpeg_bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, 90)
            .encode_image(&image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(
                1200,
                800,
                |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]),
            )))
            .unwrap();
        fs::write(&jpeg_path, &jpeg_bytes).unwrap();
        assert_eq!(viewer_decode_size(&jpeg_path), Some((1200, 800)));
        let full = decode_preview(&jpeg_path, (1200, 800)).unwrap();
        assert_eq!(full.size, [1200, 800]);
        let half = decode_preview(&jpeg_path, (600, 400)).unwrap();
        assert_eq!(half.size, [600, 400]);
        assert_ne!(
            texture_key(&jpeg_path, (1200, 800)),
            texture_key(&jpeg_path, VIEW_PREVIEW)
        );
        assert_eq!(
            viewer_request_size((6000, 4000), (1200, 800), 1.0),
            (1200, 800)
        );
        assert_eq!(
            viewer_request_size((6000, 4000), (1200, 800), 2.0),
            (6000, 4000)
        );
        assert_eq!(
            viewer_request_size((1000, 800), (1600, 1000), 1.0),
            (1000, 800)
        );

        let uri = Url::from_file_path(&source).unwrap().to_string();
        let thumbnails = root.join("thumbnails/normal");
        fs::create_dir_all(&thumbnails).unwrap();
        let thumbnail_name = format!("{:x}.png", md5::compute(uri.as_bytes()));
        let thumbnail_path = thumbnails.join(thumbnail_name);
        let thumbnail_file = fs::File::create(&thumbnail_path).unwrap();
        let thumbnail = image::RgbaImage::from_pixel(32, 24, image::Rgba([20, 80, 140, 255]));
        let mut encoder = png::Encoder::new(thumbnail_file, thumbnail.width(), thumbnail.height());
        encoder
            .add_text_chunk("Thumb::URI".into(), uri.clone())
            .unwrap();
        encoder
            .add_text_chunk(
                "Thumb::MTime".into(),
                fs::metadata(&source)
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    .to_string(),
            )
            .unwrap();
        encoder
            .add_text_chunk(
                "Thumb::Size".into(),
                fs::metadata(&source).unwrap().len().to_string(),
            )
            .unwrap();
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(thumbnail.as_raw()).unwrap();
        drop(writer);
        assert_eq!(
            system_thumbnail_at(&source, &root.join("thumbnails"))
                .unwrap()
                .size,
            [320, 240]
        );
        fs::remove_dir_all(root).unwrap();
    }
}
