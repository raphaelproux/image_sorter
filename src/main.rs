use eframe::egui::{self, ColorImage, Key, Modifiers, PointerButton, TextureHandle, TextureOptions, Vec2};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::{ImageDecoder, ImageReader};
use jpeg_decoder::{Decoder as JpegDecoder, PixelFormat as JpegPixelFormat};
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use url::Url;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::thread;
use walkdir::WalkDir;

const RAW_EXTENSIONS: &[&str] = &["arw", "cr2", "cr3", "dng", "nef", "nrw", "orf", "pef", "raf", "rw2", "srw"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp"];
const MAX_TEXTURE_BYTES: usize = 256 * 1024 * 1024;
const MAX_TEXTURE_MEMORY: usize = 512 * 1024 * 1024;
const VIEW_PREVIEW: (u32, u32) = (320, 240);
const ACTIVE_VIEW_PREVIEW_PRIORITY: u8 = 250;
const PREFETCH_PRIORITY: u8 = 80;
const ACTIVE_VIEW_IMAGE_PRIORITY: u8 = 240;
const MAX_TEXTURE_CACHE_BYTES: usize = 256 * 1024 * 1024;
const MAX_PREVIEW_QUEUE: usize = 512;

#[derive(Clone, Debug)]
struct Photo {
    display: PathBuf,
    files: Vec<PathBuf>,
    subfolder: PathBuf,
    datetime: Option<String>,
    exif: Vec<(exif::Tag, String, String)>,
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

fn default_show_datetime() -> bool { true }
fn default_show_filenames() -> bool { true }

#[derive(Default, Serialize, Deserialize)]
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
    textures: HashMap<TextureKey, TextureHandle>,
    texture_last_used: HashMap<TextureKey, u64>,
    texture_clock: u64,
    history: Vec<HistoryEntry>,
    notice: String,
    zoom: f32,
    pan: Vec2,
    previous_folder: Option<PathBuf>,
    scan_receiver: Receiver<(usize, PathBuf, Vec<Photo>)>,
    scan_sender: Sender<(usize, PathBuf, Vec<Photo>)>,
    scan_generation: usize,
    texture_receiver: Receiver<(TextureKey, Option<ColorImage>)>,
    queued_textures: HashSet<TextureKey>,
    preview_pool: Arc<PreviewPool>,
    operation_sender: Sender<OperationMessage>,
    operation_receiver: Receiver<OperationMessage>,
    operation_pending: bool,
    action_origin: Option<ViewMode>,
    action_next: Option<PathBuf>,
    last_viewer_image: Option<PathBuf>,
    scroll_to_selection: bool,
    source_sizes: HashMap<PathBuf, (u32, u32)>,
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
        let (keep_scan_sender, keep_scan_receiver) = mpsc::channel();
        let (texture_sender, texture_receiver) = mpsc::channel();
        let (operation_sender, operation_receiver) = mpsc::channel();
        let preview_pool = Arc::new(PreviewPool {
            queue: std::sync::Mutex::new(PreviewQueue::default()),
            ready: std::sync::Condvar::new(),
            cache_size: AtomicUsize::new(0),
        });
        let preview_cache = preview_cache_path();
        let worker_count = std::thread::available_parallelism().map_or(2, usize::from).clamp(2, 3);
        for _ in 0..worker_count {
            let pool = Arc::clone(&preview_pool);
            let sender = texture_sender.clone();
            let cache = preview_cache.clone();
            thread::spawn(move || preview_worker(pool, sender, cache));
        }
        Self {
            settings,
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
            texture_last_used: HashMap::new(),
            texture_clock: 0,
            history: Vec::new(),
            notice: "Choose a folder to begin.".into(),
            zoom: 1.0,
            pan: Vec2::ZERO,
            previous_folder: None,
            scan_receiver,
            scan_sender,
            scan_generation: 0,
            texture_receiver,
            queued_textures: HashSet::new(),
            preview_pool,
            operation_sender,
            operation_receiver,
            operation_pending: false,
            action_origin: None,
            action_next: None,
            last_viewer_image: None,
            scroll_to_selection: false,
            source_sizes: HashMap::new(),
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

fn embedded_thumbnail(path: &Path) -> Option<ColorImage> {
    let file = fs::File::open(path).ok()?;
    let exif = exif::Reader::new().read_from_container(&mut BufReader::new(file)).ok()?;
    let offset = exif.get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?.value.get_uint(0)? as usize;
    let length = exif.get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?.value.get_uint(0)? as usize;
    let end = offset.checked_add(length)?;
    let jpeg = exif.buf().get(offset..end)?;
    let image = decode_jpeg(std::io::Cursor::new(jpeg), Some((320, 240)))?.thumbnail(320, 240).to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([image.width() as usize, image.height() as usize], image.as_raw()))
}

fn decode_jpeg<R: Read>(reader: R, target: Option<(u32, u32)>) -> Option<image::DynamicImage> {
    let mut decoder = JpegDecoder::new(reader);
    decoder.set_max_decoding_buffer_size(MAX_TEXTURE_BYTES);
    if let Some(target) = target {
        decoder.scale(target.0.min(u16::MAX as u32) as u16, target.1.min(u16::MAX as u32) as u16).ok()?;
    }
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (width, height) = (info.width as u32, info.height as u32);
    match info.pixel_format {
        JpegPixelFormat::RGB24 => image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(width, height, pixels).map(image::DynamicImage::ImageRgb8),
        JpegPixelFormat::L8 => image::ImageBuffer::<image::Luma<u8>, _>::from_raw(width, height, pixels).map(image::DynamicImage::ImageLuma8),
        JpegPixelFormat::CMYK32 => {
            let rgb = pixels.chunks_exact(4).flat_map(|p| [0, 1, 2].map(|i| 255_u8.saturating_sub(p[i]).saturating_sub(p[3]))).collect();
            image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(width, height, rgb).map(image::DynamicImage::ImageRgb8)
        }
        JpegPixelFormat::L16 => None,
    }
}

fn jpeg_dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut decoder = JpegDecoder::new(BufReader::new(fs::File::open(path).ok()?));
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    Some((info.width as u32, info.height as u32))
}

fn exif_tag_name(tag: exif::Tag) -> String {
    tag.description()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Unknown EXIF tag 0x{:04X}", tag.number()))
}

const EXIF_CATEGORIES: [&str; 4] = ["Camera and optics", "Date, time and position", "Image", "Others"];

fn exif_category(tag: exif::Tag) -> &'static str {
    use exif::Tag;
    const DATE_TIME: &[Tag] = &[
        Tag::DateTime, Tag::DateTimeOriginal, Tag::DateTimeDigitized,
        Tag::SubSecTime, Tag::SubSecTimeOriginal, Tag::SubSecTimeDigitized,
        Tag::OffsetTime, Tag::OffsetTimeOriginal, Tag::OffsetTimeDigitized,
    ];
    const CAMERA: &[Tag] = &[
        Tag::Make, Tag::Model, Tag::LensMake, Tag::LensModel, Tag::LensSpecification, Tag::LensSerialNumber,
        Tag::BodySerialNumber, Tag::CameraOwnerName, Tag::FNumber, Tag::ExposureTime, Tag::ExposureProgram,
        Tag::ExposureMode, Tag::ExposureBiasValue, Tag::PhotographicSensitivity, Tag::SensitivityType,
        Tag::ISOSpeed, Tag::FocalLength, Tag::FocalLengthIn35mmFilm, Tag::Flash, Tag::MeteringMode,
        Tag::WhiteBalance, Tag::MaxApertureValue, Tag::ApertureValue, Tag::ShutterSpeedValue,
        Tag::BrightnessValue, Tag::SubjectDistance, Tag::SubjectDistanceRange, Tag::LightSource,
        Tag::SensingMethod, Tag::SceneCaptureType, Tag::DigitalZoomRatio, Tag::GainControl,
        Tag::Contrast, Tag::Saturation, Tag::Sharpness, Tag::CustomRendered, Tag::SubjectArea,
    ];
    const IMAGE: &[Tag] = &[
        Tag::ImageWidth, Tag::ImageLength, Tag::PixelXDimension, Tag::PixelYDimension, Tag::Orientation,
        Tag::XResolution, Tag::YResolution, Tag::ResolutionUnit, Tag::ColorSpace, Tag::BitsPerSample,
        Tag::Compression, Tag::PhotometricInterpretation, Tag::SamplesPerPixel, Tag::YCbCrPositioning,
        Tag::ComponentsConfiguration, Tag::CompressedBitsPerPixel, Tag::PlanarConfiguration,
        Tag::ImageDescription, Tag::Software, Tag::Artist, Tag::Copyright,
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
    if photo.exif.is_empty() {
        ui.label("No EXIF data");
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for category in EXIF_CATEGORIES {
            let fields: Vec<_> = photo.exif.iter().filter(|(tag, _, _)| exif_category(*tag) == category).collect();
            if fields.is_empty() { continue; }
            ui.add_space(6.0);
            ui.label(egui::RichText::new(category).strong().size(15.0).color(ui.visuals().strong_text_color()));
            ui.separator();
            for (tag, name, value) in fields {
                let key = format!("{}:{:?}", photo.display.display(), tag);
                let is_expanded = expanded.contains(&key);
                let long = value.chars().count() > 50;
                ui.horizontal_wrapped(|ui| {
                    ui.strong(name);
                    if long && !is_expanded {
                        let shortened: String = value.chars().take(50).collect();
                        ui.label(format!("{shortened}…"));
                        if ui.small_button("Expand").clicked() { expanded.insert(key.clone()); }
                    } else {
                        ui.label(value);
                        if long && ui.small_button("Collapse").clicked() { expanded.remove(&key); }
                    }
                });
            }
        }
    });
}

fn read_exif(path: &Path) -> Vec<(exif::Tag, String, String)> {
    let Ok(file) = fs::File::open(path) else { return Vec::new() };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(file)) else { return Vec::new() };
    exif.fields()
        .map(|field| (field.tag, exif_tag_name(field.tag), field.display_value().to_string()))
        .collect()
}

fn photo_date(photo: &Photo) -> Option<&str> {
    let date = photo.datetime.as_deref()?.get(..10)?;
    let bytes = date.as_bytes();
    let valid = bytes.iter().enumerate().all(|(i, b)| if i == 4 || i == 7 { matches!(b, b'-' | b':') } else { b.is_ascii_digit() });
    valid.then_some(date)
}

fn date_range_label(photos: &[Photo]) -> Option<String> {
    let dates = photos.iter().filter_map(photo_date).map(|date| date.replace(':', "-"));
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
    [exif::Tag::DateTimeOriginal, exif::Tag::DateTimeDigitized, exif::Tag::DateTime]
        .iter()
        .find_map(|tag| fields.iter().find(|(field_tag, _, _)| field_tag == tag).map(|(_, _, value)| value.clone()))
}

fn process_files(kind: ActionKind, root: PathBuf, destination: PathBuf, files: Vec<PathBuf>, photo_files: Vec<(PathBuf, Vec<PathBuf>)>, photos: Vec<Photo>, sender: Sender<OperationMessage>) -> OperationResult {
    let kept_before = photos.iter().filter(|photo| photo.kept).flat_map(|photo| photo.files.iter().cloned()).collect();
    let mut history = HistoryEntry { kind, files: Vec::new(), photos, kept_before };
    if let Err(error) = fs::create_dir_all(&destination) {
        return OperationResult { kind, history, completed: HashSet::new(), photo_files, errors: vec![format!("{}: {error}", destination.display())], is_undo: false, restored_photos: Vec::new() };
    }
    if let (Ok(root_absolute), Ok(destination_absolute)) = (fs::canonicalize(&root), fs::canonicalize(&destination)) {
        if destination_absolute.starts_with(&root_absolute) {
            return OperationResult { kind, history, completed: HashSet::new(), photo_files, errors: vec!["Keep and delete destinations must be outside the scanned folder.".into()], is_undo: false, restored_photos: Vec::new() };
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
            let stem = target.file_stem().and_then(|value| value.to_str()).unwrap_or("image").to_string();
            let ext = target.extension().and_then(|value| value.to_str()).unwrap_or_default().to_string();
            let mut suffix = 1;
            loop {
                let candidate = target.with_file_name(if ext.is_empty() { format!("{stem}_{suffix}") } else { format!("{stem}_{suffix}.{ext}") });
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
                history.files.push(FileMove { original: source, destination: target });
            }
            Err(error) => errors.push(format!("{}: {error}", source.display())),
        }
        if position % 8 == 0 {
            let _ = sender.send(OperationMessage::Progress(format!(
                "{} {}/{} files…",
                if kind == ActionKind::Keep { "Copying" } else { "Moving" },
                position + 1,
                total
            )));
        }
    }
    OperationResult { kind, history, completed, photo_files, errors, is_undo: false, restored_photos: Vec::new() }
}

fn preview_cache_path() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("image_sorter")
        .join("previews")
}

fn texture_key(path: &Path, size: (u32, u32)) -> TextureKey {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    if let Ok(metadata) = fs::metadata(path) {
        metadata.len().hash(&mut hasher);
        metadata.modified().ok().and_then(|value| value.duration_since(UNIX_EPOCH).ok()).map(|value| value.as_nanos()).hash(&mut hasher);
    }
    TextureKey { path: path.to_path_buf(), width: size.0, height: size.1, fingerprint: hasher.finish() }
}

fn viewer_decode_size(path: &Path) -> Option<(u32, u32)> {
    if matches!(extension(path).as_str(), "jpg" | "jpeg") {
        return jpeg_dimensions(path);
    }
    let reader = ImageReader::open(path).ok()?;
    let decoder = reader.into_decoder().ok()?;
    Some(decoder.dimensions())
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
    format!("{}-{}x{}-{}", key.path.to_string_lossy(), key.width, key.height, key.fingerprint)
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
    if fingerprint != key.fingerprint || width == 0 || height == 0 { return None; }
    let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if expected > MAX_TEXTURE_BYTES { return None; }
    let mut decoder = ZlibDecoder::new(file);
    let mut rgba = Vec::with_capacity(expected);
    (&mut decoder).take(expected as u64 + 1).read_to_end(&mut rgba).ok()?;
    if rgba.len() != expected { return None; }
    Some(ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba))
}

fn write_cached_preview(cache: &Path, key: &TextureKey, image: &ColorImage, pool: &PreviewPool) {
    if fs::create_dir_all(cache).is_err() { return; }
    let path = cache_file(cache, key);
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let Ok(mut file) = fs::File::create(&temporary) else { return };
    let width = (image.size[0] as u32).to_le_bytes();
    let height = (image.size[1] as u32).to_le_bytes();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    if encoder.write_all(&image.pixels.iter().flat_map(|pixel| pixel.to_array()).collect::<Vec<_>>()).is_err() {
        let _ = fs::remove_file(temporary);
        return;
    }
    let Ok(compressed) = encoder.finish() else { let _ = fs::remove_file(temporary); return };
    if file.write_all(&width).is_err()
        || file.write_all(&height).is_err()
        || file.write_all(&key.fingerprint.to_le_bytes()).is_err()
        || file.write_all(&compressed).is_err()
    {
        let _ = fs::remove_file(temporary);
        return;
    }
    let _ = fs::rename(temporary, path);
    if pool.cache_size.fetch_add(compressed.len(), std::sync::atomic::Ordering::Relaxed).saturating_add(compressed.len()) > MAX_TEXTURE_CACHE_BYTES {
        trim_preview_cache(cache, pool);
    }
}

fn trim_preview_cache(cache: &Path, pool: &PreviewPool) {
    let Ok(entries) = fs::read_dir(cache) else { return };
    let mut files = entries.filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() { return None; }
            Some((entry.path(), metadata.len(), metadata.modified().ok()?))
        })
        .collect::<Vec<_>>();
    let mut total = files.iter().map(|(_, size, _)| *size).sum::<u64>();
    pool.cache_size.store(total.min(usize::MAX as u64) as usize, std::sync::atomic::Ordering::Relaxed);
    if total <= MAX_TEXTURE_CACHE_BYTES as u64 { return; }
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in files {
        if total <= MAX_TEXTURE_CACHE_BYTES as u64 { break; }
        if fs::remove_file(path).is_ok() { total = total.saturating_sub(size); }
    }
    pool.cache_size.store(total.min(usize::MAX as u64) as usize, std::sync::atomic::Ordering::Relaxed);
}

fn job_timeout(priority: u8) -> std::time::Duration {
    if priority >= ACTIVE_VIEW_PREVIEW_PRIORITY {
        std::time::Duration::from_secs(5)
    } else {
        std::time::Duration::from_secs(15)
    }
}

fn enqueue_preview(pool: &PreviewPool, key: &TextureKey, size: (u32, u32), priority: u8, repaint: egui::Context) -> bool {
    let Ok(mut queue) = pool.queue.lock() else { return false };
    if queue.active.contains(key) { return false; }
    if let Some(index) = queue.jobs.iter().position(|job| job.key == *key) {
        queue.jobs[index].priority = queue.jobs[index].priority.max(priority);
        queue.jobs[index].repaint = repaint;
        queue.jobs[index].deadline = std::time::Instant::now() + job_timeout(priority);
        queue.jobs.sort_by(|left, right| right.priority.cmp(&left.priority));
        drop(queue);
        pool.ready.notify_one();
        return true;
    }
    if priority >= 90 {
        let mut index = 0;
        while index < queue.jobs.len() {
            if queue.jobs[index].priority >= 90 && queue.jobs[index].key.path != key.path {
                let removed = queue.jobs.remove(index);
                queue.active.remove(&removed.key);
            } else {
                index += 1;
            }
        }
    }
    if queue.jobs.len() >= MAX_PREVIEW_QUEUE {
        if let Some(lowest) = queue.jobs.iter().enumerate().min_by_key(|(_, job)| job.priority).map(|(index, job)| (index, job.priority)) {
            if lowest.1 < priority || priority >= 90 {
                let removed = queue.jobs.remove(lowest.0);
                queue.active.remove(&removed.key);
            } else {
                return false;
            }
        } else {
            return false;
        }
    }
    queue.active.insert(key.clone());
    queue.jobs.push(PreviewJob { key: key.clone(), size, priority, repaint, deadline: std::time::Instant::now() + job_timeout(priority) });
    queue.jobs.sort_by(|left, right| right.priority.cmp(&left.priority));
    drop(queue);
    pool.ready.notify_one();
    true
}

fn preview_worker(pool: Arc<PreviewPool>, sender: Sender<(TextureKey, Option<ColorImage>)>, cache: PathBuf) {
    loop {
        let job = {
            let Ok(mut queue) = pool.queue.lock() else { return };
            while queue.jobs.is_empty() {
                let Ok(next) = pool.ready.wait(queue) else { return };
                queue = next;
            }
            queue.jobs.remove(0)
        };
        let image = if job.deadline <= std::time::Instant::now() {
            None
        } else {
            let cacheable = job.size == VIEW_PREVIEW;
            let cached = if cacheable { read_cached_preview(&cache, &job.key) } else { None };
            cached.or_else(|| {
                let image = decode_preview(&job.key.path, job.size)?;
                if job.deadline <= std::time::Instant::now() { return None; }
                if cacheable { write_cached_preview(&cache, &job.key, &image, &pool); }
                Some(image)
            })
        };
        if sender.send((job.key.clone(), image)).is_err() { return; }
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
    if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" { return false; }
    let mut offset = 8;
    while offset + 12 <= data.len() {
        let length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        let end = match offset.checked_add(12).and_then(|value| value.checked_add(length)) { Some(end) if end <= data.len() => end, _ => return false };
        let chunk = &data[offset + 8..offset + 8 + length];
        if &data[offset + 4..offset + 8] == b"tEXt" {
            if let Some(separator) = chunk.iter().position(|byte| *byte == 0) {
                if &chunk[..separator] == wanted_key.as_bytes() && &chunk[separator + 1..] == wanted_value.as_bytes() { return true; }
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
        let Ok(data) = fs::read(thumbnail) else { continue };
        if data.len() > 16 * 1024 * 1024 || !data.windows(uri.len()).any(|value| value == uri.as_bytes()) {
            continue;
        }
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs().to_string();
        let source_size = metadata.len().to_string();
        if !png_text_matches(&data, "Thumb::MTime", &modified) || !png_text_matches(&data, "Thumb::Size", &source_size) {
            continue;
        }
        if let Some(image) = decode_cached_thumbnail_png(&data) {
            let thumbnail = image.thumbnail(320, 240).to_rgba8();
            return Some(ColorImage::from_rgba_unmultiplied([thumbnail.width() as usize, thumbnail.height() as usize], thumbnail.as_raw()));
        }
    }
    None
}

fn decode_cached_thumbnail_png(data: &[u8]) -> Option<image::DynamicImage> {
    if data.len() <= 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" { return None; }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_ignore_text_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let width = reader.info().width;
    let height = reader.info().height;
    let buffer_size = reader.output_buffer_size()?;
    if width == 0 || height == 0 || buffer_size > MAX_TEXTURE_BYTES { return None; }
    let mut pixels = vec![0; buffer_size];
    let output = reader.next_frame(&mut pixels).ok()?;
    let rgba = match output.color_type {
        png::ColorType::Rgba => pixels[..output.buffer_size()].to_vec(),
        png::ColorType::Rgb => pixels[..output.buffer_size()].chunks_exact(3).flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => pixels[..output.buffer_size()].chunks_exact(2).flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]]).collect(),
        png::ColorType::Grayscale => pixels[..output.buffer_size()].iter().flat_map(|value| [*value, *value, *value, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    let decoded = image::RgbaImage::from_raw(output.width, output.height, rgba)?;
    Some(image::DynamicImage::ImageRgba8(decoded))
}

fn decode_preview(path: &Path, size: (u32, u32)) -> Option<ColorImage> {
    if size == VIEW_PREVIEW {
        if let Some(thumbnail) = system_thumbnail(path) {
            return Some(thumbnail);
        }
        if let Some(thumbnail) = embedded_thumbnail(path) {
            return Some(thumbnail);
        }
    }
    let decoded = if matches!(extension(path).as_str(), "jpg" | "jpeg") {
        let requested = if size == VIEW_PREVIEW { size } else { viewer_decode_size(path).map_or(size, |source| (size.0.min(source.0), size.1.min(source.1))) };
        let image = decode_jpeg(BufReader::new(fs::File::open(path).ok()?), Some(requested))?;
        if image.width() > requested.0 || image.height() > requested.1 { image.thumbnail(requested.0, requested.1) } else { image }
    } else {
        let mut reader = ImageReader::open(path).ok()?;
        reader.no_limits();
        let decoder = reader.into_decoder().ok()?;
        let dimensions = decoder.dimensions();
        if (dimensions.0 as usize).saturating_mul(dimensions.1 as usize).saturating_mul(4) > MAX_TEXTURE_BYTES {
            return None;
        }
        let pixels = image::DynamicImage::from_decoder(decoder).ok()?;
        let target = if size == VIEW_PREVIEW { size } else { viewer_decode_size(path)? };
        if pixels.width() <= target.0 && pixels.height() <= target.1 && size != VIEW_PREVIEW { pixels } else { pixels.thumbnail(target.0, target.1) }
    };
    let width = decoded.width() as usize;
    let height = decoded.height() as usize;
    if width.checked_mul(height)?.checked_mul(4)? > MAX_TEXTURE_BYTES { return None; }
    let rgba = decoded.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([width, height], rgba.as_raw()))
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
    let Ok(relative) = photo.display.strip_prefix(root) else { return false };
    let kept_path = keep_folder.join(relative);
    let equivalent = |left: &Path, right: &Path| {
        fs::canonicalize(left).ok().zip(fs::canonicalize(right).ok()).is_some_and(|(left, right)| left == right)
    };
    keep_files.contains(&kept_path) || equivalent(&kept_path, &photo.display) || photo.files.iter().any(|file| {
        file.strip_prefix(root).ok().map(|relative| keep_folder.join(relative)).is_some_and(|kept| keep_files.contains(&kept) || equivalent(&kept, file))
    })
}

fn scan_folder(root: &Path) -> Vec<Photo> {
    let mut folders: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_map(Result::ok) {
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
                    groups.entry(stem_key(&file).to_lowercase()).or_default().push(file);
        }
        for mut group in groups.into_values() {
            group.sort();
            let jpeg = group.iter().position(|path| matches!(extension(path).as_str(), "jpg" | "jpeg"));
            let display_index = jpeg
                .or_else(|| group.iter().position(|path| IMAGE_EXTENSIONS.contains(&extension(path).as_str())))
                .or_else(|| group.iter().position(|path| RAW_EXTENSIONS.contains(&extension(path).as_str())));
            let Some(display_index) = display_index else { continue };
            let display = group[display_index].clone();
            let relative = folder.strip_prefix(root).unwrap_or(&folder).to_path_buf();
            let exif = read_exif(&display);
            photos.push(Photo {
                datetime: exif_datetime(&exif),
                display,
                files: group,
                subfolder: relative,
                exif,
                kept: false,
            });
        }
    }
    photos.sort_by(|a, b| a.subfolder.cmp(&b.subfolder).then_with(|| a.display.file_name().cmp(&b.display.file_name())));
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
            for photo in &mut self.photos { photo.kept = false; }
            return;
        };
        self.keep_scan_generation = self.keep_scan_generation.wrapping_add(1);
        let generation = self.keep_scan_generation;
        let sender = self.keep_scan_sender.clone();
        let repaint = ctx.clone();
        self.keep_scan_pending = true;
        self.next_keep_scan = std::time::Instant::now() + std::time::Duration::from_secs(3);
        thread::spawn(move || {
            let files = scan_keep_folder(&keep_folder);
            let _ = sender.send((generation, files));
            repaint.request_repaint();
        });
    }

    fn sync_keep_watcher(&mut self, ctx: &egui::Context) {
        use notify::Watcher;
        let wanted = (!self.settings.keep_folder.trim().is_empty()).then(|| PathBuf::from(self.settings.keep_folder.trim()));
        if wanted == self.keep_watch_path { return; }
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
            if watcher.watch(&path, notify::RecursiveMode::Recursive).is_ok() {
                self.keep_watcher = Some(watcher);
            }
        }
        self.keep_dirty.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn poll_kept_status(&mut self, ctx: &egui::Context) {
        self.sync_keep_watcher(ctx);
        if self.settings.keep_folder.trim().is_empty() {
            for photo in &mut self.photos { photo.kept = false; }
            return;
        }
        let mut latest = None;
        while let Ok(result) = self.keep_scan_receiver.try_recv() { latest = Some(result); }
        if let Some((generation, files)) = latest {
            self.keep_scan_pending = false;
            self.next_keep_scan = std::time::Instant::now() + std::time::Duration::from_secs(3);
            if generation == self.keep_scan_generation {
                if let (Some(root), Some(keep_folder)) = (self.root.as_deref(), (!self.settings.keep_folder.trim().is_empty()).then(|| PathBuf::from(self.settings.keep_folder.trim()))) {
                    for photo in &mut self.photos {
                        photo.kept = is_kept(photo, root, &keep_folder, &files);
                    }
                }
            }
        } else if self.keep_scan_pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        } else if self.keep_dirty.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // The keep folder changed on disk: rescan shortly (debounces bursts of events).
            let soon = std::time::Instant::now() + std::time::Duration::from_millis(300);
            if soon < self.next_keep_scan { self.next_keep_scan = soon; }
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        } else if std::time::Instant::now() >= self.next_keep_scan {
            self.next_keep_scan = std::time::Instant::now() + std::time::Duration::from_secs(3);
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
            self.selection.clear();
            self.textures.clear();

            self.texture_last_used.clear();
            self.queued_textures.clear();
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

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let mut latest = None;
        while let Ok(result) = self.scan_receiver.try_recv() {
            latest = Some(result);
        }
        if let Some((generation, folder, photos)) = latest {
                if generation == self.scan_generation && self.root.as_ref() == Some(&folder) {
                    self.notice = format!("{} images found in {}", photos.len(), folder.display());
                    self.photos = photos;
                    self.refresh_kept_status(ctx);
                self.textures.clear();

                self.texture_last_used.clear();
                self.queued_textures.clear();
                self.selection.clear();
                self.current = 0;
            }
        } else if self.root.is_some() && self.notice.starts_with("Scanning ") {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn poll_textures(&mut self, ctx: &egui::Context) {
        while let Ok((key, image)) = self.texture_receiver.try_recv() {
            if let Some(image) = image {
                if self.photos.iter().any(|photo| photo.display == key.path) {
                    let texture = ctx.load_texture(texture_name(&key), image, TextureOptions::LINEAR);
                    self.textures.insert(key.clone(), texture);
                    self.texture_clock = self.texture_clock.wrapping_add(1);
                    self.texture_last_used.insert(key.clone(), self.texture_clock);
                    while self.texture_memory_usage() > MAX_TEXTURE_MEMORY {
                        let Some(oldest) = self.texture_last_used.iter().min_by_key(|(_, last_used)| *last_used).map(|(key, _)| key.clone()) else { break };
                        self.textures.remove(&oldest);
                        self.texture_last_used.remove(&oldest);
                    }
                }
            }
            self.queued_textures.remove(&key);
            if let Ok(mut queue) = self.preview_pool.queue.lock() {
                queue.active.remove(&key);
            }
        }
    }

    fn texture_memory_usage(&self) -> usize {
        self.textures.values().map(|texture| {
            let [width, height] = texture.size();
            width.saturating_mul(height).saturating_mul(4)
        }).sum()
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

    fn texture(&mut self, ctx: &egui::Context, path: &Path, size: (u32, u32), priority: u8) {
        let key = texture_key(path, size);
        if let Some(existing) = self.textures.keys().find(|existing| existing.path == path && existing.fingerprint == key.fingerprint && existing.width >= key.width && existing.height >= key.height) {
            let existing = existing.clone();
            self.texture_clock = self.texture_clock.wrapping_add(1);
            self.texture_last_used.insert(existing, self.texture_clock);
            return;
        }
        if self.textures.contains_key(&key) {
            return;
        }
        if self.queued_textures.contains(&key) {
            let still_active = self.preview_pool.queue.lock().map(|queue| queue.active.contains(&key)).unwrap_or(false);
            if still_active { return; }
            self.queued_textures.remove(&key);
        }
        if size != VIEW_PREVIEW && priority >= ACTIVE_VIEW_IMAGE_PRIORITY {
            if enqueue_preview(&self.preview_pool, &key, size, priority, ctx.clone()) {
                self.queued_textures.insert(key);
            }
            return;
        }
        if enqueue_preview(&self.preview_pool, &key, size, priority, ctx.clone()) {
            self.queued_textures.insert(key);
        }
    }

    fn source_size(&mut self, path: &Path) -> (u32, u32) {
        if let Some(size) = self.source_sizes.get(path) { return *size; }
        let size = viewer_decode_size(path).unwrap_or(VIEW_PREVIEW);
        self.source_sizes.insert(path.to_path_buf(), size);
        size
    }

    fn viewer_texture(&mut self, path: &Path, target: (u32, u32)) -> Option<TextureHandle> {
        let fingerprint = texture_key(path, (0, 0)).fingerprint;
        let best = self.textures.iter()
            .filter(|(key, _)| key.path == path && key.fingerprint == fingerprint)
            .min_by_key(|(key, _)| key.width.abs_diff(target.0) as u64 + key.height.abs_diff(target.1) as u64)
            .map(|(key, texture)| (key.clone(), texture.clone()));
        if let Some((key, texture)) = best {
            self.texture_clock = self.texture_clock.wrapping_add(1);
            self.texture_last_used.insert(key, self.texture_clock);
            Some(texture)
        } else {
            None
        }
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
            self.settings_open = true;
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
        if let (Ok(root_absolute), Ok(destination_absolute)) = (fs::canonicalize(&root), fs::canonicalize(&destination)) {
            if destination_absolute.starts_with(&root_absolute) {
                self.notice = "Keep and delete destinations must be outside the scanned folder.".into();
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
            .filter_map(|index| self.photos.get(*index).map(|photo| (photo.display.clone(), photo.files.clone())))
            .collect();
        let files = photo_files.iter().flat_map(|(_, files)| files.iter().cloned()).collect::<Vec<_>>();
        let photos = indices.iter().filter_map(|index| self.photos.get(*index).cloned()).collect();
        let sender = self.operation_sender.clone();
        let progress_sender = sender.clone();
        self.operation_pending = true;
        self.action_origin = Some(self.mode);
        self.action_next = self.photos.iter().skip(self.current + 1).map(|photo| photo.display.clone()).next()
            .or_else(|| self.photos.iter().take(self.current).map(|photo| photo.display.clone()).next());
        self.notice = format!("{} {} file(s)…", if kind == ActionKind::Keep { "Copying" } else { "Moving" }, files.len());
        let repaint = ctx.clone();
        thread::spawn(move || {
            let result = process_files(kind, root, destination, files, photo_files, photos, progress_sender);
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
                            self.photos.sort_by(|left, right| left.subfolder.cmp(&right.subfolder).then_with(|| left.display.file_name().cmp(&right.display.file_name())));
                            self.selection.clear();
                            if let Some(index) = self.photos.iter().position(|photo| photo.display == restored[0].display) {
                                self.current = index;
                                self.mode = ViewMode::Viewer;
                                let display = self.photos[index].display.clone();
                                self.texture(ctx, &display, VIEW_PREVIEW, ACTIVE_VIEW_PREVIEW_PRIORITY);
                            }
                        } else if result.kind == ActionKind::Keep {
                            for photo in &mut self.photos {
                                if photo.files.iter().any(|file| result.history.photos.iter().any(|before| before.files.contains(file))) {
                                    photo.kept = result.history.kept_before.contains(&photo.display);
                                }
                            }
                        }
                        self.notice = if result.errors.is_empty() { "Last keep/delete operation undone.".into() } else { format!("Undo incomplete: {}", result.errors.join("; ")) };
                    } else {
                        if !result.history.files.is_empty() { self.history.push(result.history); }
                        self.notice = if result.errors.is_empty() {
                            format!("{} file(s) {}", result.completed.len(), if result.kind == ActionKind::Keep { "copied" } else { "moved" })
                        } else {
                            format!("Some files could not be processed: {}", result.errors.join("; "))
                        };
                        let done: HashSet<_> = result.completed;
                        let successful_photos: HashSet<_> = result.photo_files.into_iter()
                            .filter(|(_, files)| files.iter().all(|file| done.contains(file)))
                            .map(|(display, _)| display)
                            .collect();
                        let current_display = self.photos.get(self.current).map(|photo| photo.display.clone());
                        if result.kind == ActionKind::Keep {
                            for photo in &mut self.photos {
                                if successful_photos.contains(&photo.display) { photo.kept = true; }
                            }
                        } else {
                            self.photos.retain(|photo| !successful_photos.contains(&photo.display));
                        }
                        self.selection.clear();
                        let available: HashSet<_> = self.photos.iter().map(|photo| photo.display.clone()).collect();
                        self.textures.retain(|key, _| available.contains(&key.path));
                        self.texture_last_used.retain(|key, _| available.contains(&key.path));
                        self.queued_textures.retain(|key| available.contains(&key.path));
                        if let Some(next) = self.action_next.take().or(current_display) {
                            if let Some(index) = self.photos.iter().position(|photo| photo.display == next) { self.current = index; }
                            else { self.current = self.current.min(self.photos.len().saturating_sub(1)); }
                        } else {
                            self.current = self.current.min(self.photos.len().saturating_sub(1));
                        }
                        if self.photos.is_empty() { self.mode = ViewMode::Grid; self.current = 0; }
                        else if self.action_origin == Some(ViewMode::Viewer) { self.mode = ViewMode::Viewer; }
                    }
                    if result.kind == ActionKind::Keep || result.kind == ActionKind::Delete {
                        self.refresh_kept_status(ctx);
                    }
                    self.action_origin = None;
                }
            }
        }
        if self.operation_pending { ctx.request_repaint_after(std::time::Duration::from_millis(100)); }
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
                        if let Some(parent) = item.original.parent() { let _ = fs::create_dir_all(parent); }
                        move_file(&item.destination, &item.original)
                    }
                };
                if let Err(error) = result { errors.push(format!("{}: {error}", item.destination.display())); }
            }
            let restored_photos = if entry.kind == ActionKind::Delete {
                entry.photos.iter().cloned().filter(|photo| photo.files.iter().all(|file| file.exists())).collect()
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
            self.current = if self.current == 0 { self.photos.len() - 1 } else { self.current - 1 };
        }
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
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
        match value.trim().to_ascii_lowercase().as_str() {
            "backspace" => key == Key::Backspace,
            "delete" => key == Key::Delete,
            "space" => key == Key::Space,
            "enter" => key == Key::Enter,
            "left" => key == Key::ArrowLeft,
            "right" => key == Key::ArrowRight,
            value if value.len() == 1 => value.chars().next().is_some_and(|character| character.to_string().eq_ignore_ascii_case(&format!("{key:?}"))),
            _ => false,
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
        let open_settings = ctx.input(|input| input.key_pressed(Key::Comma) && input.modifiers.ctrl);
        if open_settings { self.settings_open = true; }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open folder").clicked() { self.choose_folder(ctx); }
                if ui.button("Grid").clicked() { self.show_grid(); }
                if ui.checkbox(&mut self.settings.show_filenames, "Filenames").changed() { self.save_settings(); }
                if ui.checkbox(&mut self.settings.show_datetime, "Date/time").changed() { self.save_settings(); }
                if ui.checkbox(&mut self.settings.show_exif, "EXIF").changed() {
                    self.save_settings();
                    if self.settings.show_exif {
                        if self.mode == ViewMode::Viewer {
                            if let Some(photo) = self.photos.get(self.current) {
                                let fields = read_exif(&photo.display);
                                if let Some(photo) = self.photos.get_mut(self.current) { photo.exif = fields; }
                            }
                        } else if self.selection.len() == 1 {
                            if let Some(index) = self.selection.iter().next().copied() {
                                if let Some(photo) = self.photos.get(index) {
                                    let fields = read_exif(&photo.display);
                                    if let Some(photo) = self.photos.get_mut(index) { photo.exif = fields; }
                                }
                            }
                        }
                    }
                }
                if ui.button(format!("Keep ({})", keep_shortcut)).clicked() { self.sort_photos(ActionKind::Keep, ctx); }
                if ui.button(format!("Leave ({})", leave_shortcut)).clicked() && self.mode == ViewMode::Viewer { self.next_photo(); }
                if ui.button(format!("Delete ({})", delete_shortcut)).clicked() { self.sort_photos(ActionKind::Delete, ctx); }
                if ui.button("Undo (Ctrl+Z)").clicked() { self.undo(ctx); }
                if ui.button("Settings").clicked() { self.settings_open = true; }
            });
        });
        if self.mode == ViewMode::Grid {
            egui::TopBottomPanel::bottom("grid_actions").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Keep selected").clicked() { self.sort_photos(ActionKind::Keep, ctx); }
                    if ui.button("Delete selected").clicked() { self.sort_photos(ActionKind::Delete, ctx); }
                    ui.label(format!("{} selected", self.selection.len()));
                });
            });
        } else {
            egui::TopBottomPanel::bottom("viewer_status").show(ctx, |ui| { ui.label(&self.notice); });
        }
        let undo_pressed = ctx.input(|input| input.key_pressed(Key::Z) && input.modifiers.ctrl);
        if undo_pressed { self.undo(ctx); }
        if self.mode == ViewMode::Viewer && !ctx.wants_keyboard_input() {
            let (right, left, pressed) = ctx.input(|input| {
                (
                    input.key_pressed(Key::ArrowRight),
                    input.key_pressed(Key::ArrowLeft),
                    [Key::K, Key::L, Key::Backspace, Key::Delete, Key::Space, Key::Enter]
                        .into_iter()
                        .filter(|key| input.key_pressed(*key))
                        .collect::<Vec<_>>(),
                )
            });
            let keep_pressed = pressed.iter().any(|key| Self::shortcut_matches(&self.settings.keep_shortcut, *key));
            let leave_pressed = pressed.iter().any(|key| Self::shortcut_matches(&self.settings.leave_shortcut, *key));
            let delete_pressed = pressed.iter().any(|key| Self::shortcut_matches(&self.settings.delete_shortcut, *key));
            if ctx.input(|input| input.key_pressed(Key::Escape)) && !self.settings_open { self.show_grid(); }
            if right { self.next_photo(); }
            if left { self.previous_photo(); }
            if keep_pressed { self.sort_photos(ActionKind::Keep, ctx); }
            if leave_pressed { self.next_photo(); }
            if delete_pressed { self.sort_photos(ActionKind::Delete, ctx); }
        }
        if self.settings_open {
            let response = egui::Modal::new(egui::Id::new("settings_modal")).show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.heading("Settings");
                ui.label("Keep destination folder");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.settings.keep_folder);
                    if ui.button("Browse…").clicked() {
                        if let Ok(Some(path)) = native_dialog::FileDialog::new().show_open_single_dir() { self.settings.keep_folder = path.display().to_string(); }
                    }
                });
                ui.label("Delete destination folder");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.settings.delete_folder);
                    if ui.button("Browse…").clicked() {
                        if let Ok(Some(path)) = native_dialog::FileDialog::new().show_open_single_dir() { self.settings.delete_folder = path.display().to_string(); }
                    }
                });
                ui.separator();
                ui.label("Viewer shortcuts");
                ui.horizontal(|ui| { ui.label("Keep"); ui.text_edit_singleline(&mut self.settings.keep_shortcut); });
                ui.horizontal(|ui| { ui.label("Leave"); ui.text_edit_singleline(&mut self.settings.leave_shortcut); });
                ui.horizontal(|ui| { ui.label("Delete"); ui.text_edit_singleline(&mut self.settings.delete_shortcut); });
                ui.horizontal(|ui| {
                    if self.pending_action.is_some_and(|kind| match kind { ActionKind::Keep => !self.settings.keep_folder.trim().is_empty(), ActionKind::Delete => !self.settings.delete_folder.trim().is_empty() }) && ui.button("Continue").clicked() {
                        let kind = self.pending_action.take().unwrap();
                        self.save_settings();
                        self.refresh_kept_status(ctx);
                        self.settings_open = false;
                        self.sort_photos(kind, ctx);
                    }
                    if ui.button("Save").clicked() {
                        self.save_settings();
                        self.settings_open = false;
                        self.refresh_kept_status(ctx);
                        if self.pending_action.is_some_and(|kind| match kind { ActionKind::Keep => !self.settings.keep_folder.trim().is_empty(), ActionKind::Delete => !self.settings.delete_folder.trim().is_empty() }) {
                            let kind = self.pending_action.take().unwrap();
                            self.sort_photos(kind, ctx);
                        }
                    }
                    if ui.button("Cancel").clicked() { self.settings_open = false; self.pending_action = None; }
                });
            });
            if response.should_close() { self.settings_open = false; self.pending_action = None; }
        }

        if self.mode == ViewMode::Viewer {
            egui::CentralPanel::default().show(ctx, |ui| {
                if self.photos.is_empty() { ui.centered_and_justified(|ui| ui.label("No images to display.")); return; }
                let photo = self.photos[self.current].clone();
                egui::TopBottomPanel::top("viewer_info").show_inside(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if self.settings.show_filenames { ui.label(photo.display.file_name().unwrap_or_default().to_string_lossy()); }
                        if self.settings.show_datetime { ui.label(photo.datetime.as_deref().unwrap_or("No EXIF datetime")); }
                        if photo.kept { ui.label("Kept").on_hover_text("This photo has been copied to the keep folder."); }
                    });
                });
                if self.last_viewer_image.as_ref() != Some(&photo.display) {
                    self.prioritize_viewer_image(&photo.display);
                    self.last_viewer_image = Some(photo.display.clone());
                }
                if self.settings.show_exif && photo.exif.is_empty() {
                    let fields = read_exif(&photo.display);
                    if let Some(current) = self.photos.get_mut(self.current) { current.exif = fields; }
                }
                egui::TopBottomPanel::bottom("viewer_actions").show_inside(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Previous").clicked() { self.previous_photo(); }
                        ui.label(format!("{} / {}", self.current + 1, self.photos.len()));
                        if ui.button("Next").clicked() { self.next_photo(); }
                        if ui.button("Reset zoom").clicked() { self.zoom = 1.0; self.pan = Vec2::ZERO; }
                        if ui.button(format!("Keep ({})", self.settings.keep_shortcut)).clicked() { self.sort_photos(ActionKind::Keep, ctx); }
                        if ui.button(format!("Leave ({})", self.settings.leave_shortcut)).clicked() { self.next_photo(); }
                        if ui.button(format!("Delete ({})", self.settings.delete_shortcut)).clicked() { self.sort_photos(ActionKind::Delete, ctx); }
                    });
                });
                egui::SidePanel::right("viewer_exif").resizable(true).default_width(260.0).show_animated_inside(ui, self.settings.show_exif, |ui| {
                    ui.heading("EXIF");
                    show_exif_fields(ui, &photo, &mut self.expanded_exif);
                });
                let available = ui.available_size();
                let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
                if response.double_clicked() { self.show_grid(); }
                let scroll = if response.hovered() { ctx.input(|input| input.raw_scroll_delta.y) } else { 0.0 };
                if scroll != 0.0 {
                    self.zoom = (self.zoom * (scroll / 300.0).exp()).clamp(0.1, 8.0);
                }
                let source_size = self.source_size(&photo.display);
                let viewport = (
                    (rect.width() * ctx.pixels_per_point()).round().max(1.0) as u32,
                    (rect.height() * ctx.pixels_per_point()).round().max(1.0) as u32,
                );
                let requested_size = viewer_request_size(source_size, viewport, self.zoom);
                if self.zoom <= 1.0 && self.photos.len() > 1 {
                    let count = self.photos.len();
                    for neighbour in [(self.current + 1) % count, (self.current + count - 1) % count] {
                        let path = self.photos[neighbour].display.clone();
                        let source = self.source_size(&path);
                        self.texture(ctx, &path, viewer_request_size(source, viewport, 1.0), PREFETCH_PRIORITY);
                    }
                }
                let fit = (rect.width() / requested_size.0 as f32).min(rect.height() / requested_size.1 as f32);
                let display_size = Vec2::new(requested_size.0 as f32, requested_size.1 as f32) * fit * self.zoom;
                if response.dragged_by(PointerButton::Primary) {
                    let delta = response.drag_delta();
                    self.pan += delta;
                    let margin = (display_size * 0.1).min(rect.size() * 0.25);
                    let max_pan = Vec2::new(
                        (rect.width() + display_size.x) * 0.5 - margin.x,
                        (rect.height() + display_size.y) * 0.5 - margin.y,
                    ).max(Vec2::ZERO);
                    self.pan.x = self.pan.x.clamp(-max_pan.x, max_pan.x);
                    self.pan.y = self.pan.y.clamp(-max_pan.y, max_pan.y);
                }
                self.texture(ctx, &photo.display, requested_size, ACTIVE_VIEW_IMAGE_PRIORITY);
                self.texture(ctx, &photo.display, VIEW_PREVIEW, ACTIVE_VIEW_PREVIEW_PRIORITY);
                if let Some(texture) = self.viewer_texture(&photo.display, requested_size) {
                    let image_size = texture.size_vec2();
                    let fit = (rect.width() / image_size.x).min(rect.height() / image_size.y);
                    let size = image_size * fit * self.zoom;
                    let margin = (size * 0.1).min(rect.size() * 0.25);
                    let max_pan = Vec2::new(
                        (rect.width() + size.x) * 0.5 - margin.x,
                        (rect.height() + size.y) * 0.5 - margin.y,
                    ).max(Vec2::ZERO);
                    self.pan.x = self.pan.x.clamp(-max_pan.x, max_pan.x);
                    self.pan.y = self.pan.y.clamp(-max_pan.y, max_pan.y);
                    let center = rect.center() + self.pan;
                    let image_rect = egui::Rect::from_center_size(center, size);
                    ui.painter().with_clip_rect(rect).image(texture.id(), image_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                } else {
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Could not load image preview", egui::FontId::default(), egui::Color32::WHITE);
                }
            });
        } else {
            egui::CentralPanel::default().show(ctx, |ui| {
                if self.photos.is_empty() { ui.centered_and_justified(|ui| ui.label("Choose a folder to scan images recursively.")); return; }
                let mut index = 0;
                let mut previous_folder: Option<PathBuf> = None;
                let mut pending_selection = None;
                let root_name = self.root.as_ref().and_then(|root| root.file_name()).map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| ".".into());
                let available = ui.available_size();
                let (grid_rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
                ui.scope_builder(egui::UiBuilder::new().max_rect(grid_rect), |ui| {
                    if self.settings.show_exif {
                        egui::SidePanel::right("grid_exif").resizable(true).default_width(280.0).show_inside(ui, |ui| {
                            ui.heading("Selected image EXIF");
                            let selected = if self.selection.len() == 1 { self.selection.iter().next().copied() } else { None };
                            if let Some(index) = selected {
                                if self.photos.get(index).is_some_and(|photo| photo.exif.is_empty()) {
                                    let fields = read_exif(&self.photos[index].display);
                                    self.photos[index].exif = fields;
                                }
                                if let Some(photo) = self.photos.get(index).cloned() {
                                    show_exif_fields(ui, &photo, &mut self.expanded_exif);
                                }
                            } else {
                                ui.label("Select one image to view its EXIF data.");
                            }
                        });
                    }
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    while index < self.photos.len() {
                        let folder = self.photos[index].subfolder.clone();
                        if previous_folder.as_ref() != Some(&folder) {
                            ui.add_space(8.0);
                            let title = if folder.as_os_str().is_empty() { root_name.clone() } else { folder.display().to_string() };
                            let group_len = self.photos[index..].iter().take_while(|photo| photo.subfolder == folder).count();
                            let date_range = date_range_label(&self.photos[index..index + group_len]);
                            let title_color = ui.visuals().strong_text_color();
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(title).strong().size(20.0).color(title_color));
                                if let Some(range) = date_range {
                                    ui.add_space(48.0);
                                    ui.label(egui::RichText::new(range).size(13.0).color(title_color));
                                }
                            });
                            ui.separator();
                            previous_folder = Some(folder.clone());
                        }
                        ui.horizontal(|ui| {
                            let spacing = ui.spacing().item_spacing.x;
                            let total_width = ui.available_width();
                            let columns = ((total_width + spacing) / (160.0 + spacing)).floor().max(1.0) as usize;
                            let cell_width = ((total_width - spacing * (columns - 1) as f32) / columns as f32).max(100.0);
                            let stop = (index + columns).min(self.photos.len());
                            while index < stop {
                                if self.photos[index].subfolder != folder { break; }
                                let photo = self.photos[index].clone();
                                let photo_index = index;
                                let selected = self.selection.contains(&index);
                                ui.vertical(|ui| {
                                    ui.set_width(cell_width);
                                    ui.horizontal_wrapped(|ui| {
                                        if self.settings.show_filenames { ui.label(photo.display.file_name().unwrap_or_default().to_string_lossy()); }
                                        if photo.kept { ui.label("Kept").on_hover_text("Copied to keep folder"); }
                                    });
                                    if self.settings.show_datetime {
                                        ui.label(egui::RichText::new(photo.datetime.as_deref().unwrap_or("No EXIF datetime")).size(10.0));
                                    }
                                    let size = Vec2::new(cell_width, cell_width * 0.75);
                                    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
                                    let visible = ui.is_rect_visible(rect);
                                    if visible {
                                        self.texture(ctx, &photo.display, (320, 240), 0);
                                    }
                                    if let Some(texture) = self.viewer_texture(&photo.display, (320, 240)) {
                                        let image_size = texture.size_vec2();
                                        let fit = (rect.width() / image_size.x).min(rect.height() / image_size.y);
                                        let image_rect = egui::Rect::from_center_size(rect.center(), image_size * fit);
                                        ui.painter().image(texture.id(), image_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                                    } else {
                                        ui.painter().rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
                                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Preview unavailable", egui::FontId::default(), egui::Color32::WHITE);
                                    }
                                    if self.scroll_to_selection && selected && self.selection.len() == 1 { ui.scroll_to_rect(rect, Some(egui::Align::Center)); }
                                    if selected { ui.painter().rect_stroke(rect, 2.0, egui::Stroke::new(3.0_f32, egui::Color32::LIGHT_BLUE), egui::StrokeKind::Inside); }
                                    if response.clicked() { pending_selection = Some((photo_index, response.ctx.input(|input| input.modifiers))); }
                                    if response.double_clicked() { self.current = photo_index; self.mode = ViewMode::Viewer; self.last_viewer_image = None; }
                                });
                                index += 1;
                            }
                        });
                    }
                    });
                });
                self.scroll_to_selection = false;
                if let Some((index, modifiers)) = pending_selection { self.select(index, modifiers); }
            });
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native("Image Sorter", options, Box::new(|_| Ok(Box::<SorterApp>::default())))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            kept: false,
        };
        let raw_copy = keep.join("trip/photo.ARW");
        assert!(is_kept(&photo, &root, &keep, &HashSet::from([raw_copy])));
        assert!(!is_kept(&photo, &root, &keep, &HashSet::new()));
    }

    #[test]
    fn known_exif_tags_have_human_readable_names() {
        assert_eq!(exif_tag_name(exif::Tag::DateTimeOriginal), "Date and time of original data generation");
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
            kept: false,
        };
        let same = [photo(Some("2024-05-01 10:00:00")), photo(Some("2024-05-01 18:30:00")), photo(None)];
        assert_eq!(date_range_label(&same).as_deref(), Some("2024-05-01"));
        let span = [photo(Some("2024-05-03 10:00:00")), photo(Some("2024-04-28 18:30:00"))];
        assert_eq!(date_range_label(&span).as_deref(), Some("2024-04-28 – 2024-05-03"));
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
        assert_eq!(exif_category(exif::Tag::DateTimeOriginal), "Date, time and position");
        assert_eq!(exif_category(exif::Tag::GPSLatitude), "Date, time and position");
        assert_eq!(exif_category(exif::Tag::PixelXDimension), "Image");
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
        let jpeg = photos.iter().find(|photo| extension(&photo.display) == "jpg").unwrap();
        assert_eq!(jpeg.files.len(), 2);
        assert_eq!(jpeg.display.file_name().unwrap(), "photo.JPG");
        assert!(jpeg.exif.is_empty());
        let raw = photos.iter().find(|photo| extension(&photo.display) == "nef").unwrap();
        assert_eq!(raw.files.len(), 1);
        assert_eq!(raw.subfolder, PathBuf::from("trip"));
        assert!(!jpeg.kept);

        let history = HistoryEntry {
            kind: ActionKind::Delete,
            files: vec![FileMove { original: raw.display.clone(), destination: PathBuf::from("deleted/other.NEF") }],
            photos: vec![raw.clone()],
            kept_before: HashSet::new(),
        };
        let restored = history.photos.clone();
        assert_eq!(restored[0].display, raw.display);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_decode_outputs_bounded_thumbnail_and_cache_roundtrips() {
        let root = std::env::temp_dir().join(format!("image_sorter_preview_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("preview.png");
        let source_image = image::RgbaImage::from_pixel(800, 600, image::Rgba([20, 80, 140, 255]));
        source_image.save(&source).unwrap();
        let preview = decode_preview(&source, VIEW_PREVIEW).unwrap();
        assert_eq!(preview.size, [VIEW_PREVIEW.0 as usize, VIEW_PREVIEW.1 as usize]);
        let key = texture_key(&source, VIEW_PREVIEW);
        let cache_pool = PreviewPool {
            queue: std::sync::Mutex::new(PreviewQueue::default()),
            ready: std::sync::Condvar::new(),
            cache_size: AtomicUsize::new(0),
        };
        write_cached_preview(&root.join("cache"), &key, &preview, &cache_pool);
        let cached = read_cached_preview(&root.join("cache"), &key).unwrap();
        assert_eq!(cached.size, [VIEW_PREVIEW.0 as usize, VIEW_PREVIEW.1 as usize]);
        assert_eq!(cached.pixels, preview.pixels);

        let jpeg_path = root.join("big.jpg");
        let mut jpeg_bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, 90)
            .encode_image(&image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(1200, 800, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]))))
            .unwrap();
        fs::write(&jpeg_path, &jpeg_bytes).unwrap();
        assert_eq!(viewer_decode_size(&jpeg_path), Some((1200, 800)));
        let full = decode_preview(&jpeg_path, (1200, 800)).unwrap();
        assert_eq!(full.size, [1200, 800]);
        let half = decode_preview(&jpeg_path, (600, 400)).unwrap();
        assert_eq!(half.size, [600, 400]);
        assert_ne!(texture_key(&jpeg_path, (1200, 800)), texture_key(&jpeg_path, VIEW_PREVIEW));
        assert_eq!(viewer_request_size((6000, 4000), (1200, 800), 1.0), (1200, 800));
        assert_eq!(viewer_request_size((6000, 4000), (1200, 800), 2.0), (6000, 4000));
        assert_eq!(viewer_request_size((1000, 800), (1600, 1000), 1.0), (1000, 800));

        let uri = Url::from_file_path(&source).unwrap().to_string();
        let thumbnails = root.join("thumbnails/normal");
        fs::create_dir_all(&thumbnails).unwrap();
        let thumbnail_name = format!("{:x}.png", md5::compute(uri.as_bytes()));
        let thumbnail_path = thumbnails.join(thumbnail_name);
        let thumbnail_file = fs::File::create(&thumbnail_path).unwrap();
        let thumbnail = image::RgbaImage::from_pixel(32, 24, image::Rgba([20, 80, 140, 255]));
        let mut encoder = png::Encoder::new(thumbnail_file, thumbnail.width(), thumbnail.height());
        encoder.add_text_chunk("Thumb::URI".into(), uri.clone()).unwrap();
        encoder.add_text_chunk("Thumb::MTime".into(), fs::metadata(&source).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs().to_string()).unwrap();
        encoder.add_text_chunk("Thumb::Size".into(), fs::metadata(&source).unwrap().len().to_string()).unwrap();
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(thumbnail.as_raw()).unwrap();
        drop(writer);
        assert_eq!(system_thumbnail_at(&source, &root.join("thumbnails")).unwrap().size, [320, 240]);
        fs::remove_dir_all(root).unwrap();
    }
}




