use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use fast_image_resize::{IntoImageView, Resizer, images::Image};
use flume::{Receiver, Sender};
use image::DynamicImage;
use lru::LruCache;
use ratatui::layout::Size;
use ratatui_image::{Resize, picker::Picker, protocol::Protocol};

use crate::{
    metadata::{apply_orientation, read_exif_metadata},
    state::{ImageEntry, ZoomMode},
};

#[derive(Debug, Clone, Eq)]
pub struct ThumbKey {
    pub path: PathBuf,
    pub file_len: u64,
    pub modified_nanos: Option<u128>,
    pub width_cells: u16,
    pub height_cells: u16,
    pub backend_id: String,
}

impl PartialEq for ThumbKey {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.file_len == other.file_len
            && self.modified_nanos == other.modified_nanos
            && self.width_cells == other.width_cells
            && self.height_cells == other.height_cells
            && self.backend_id == other.backend_id
    }
}

impl Hash for ThumbKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.file_len.hash(state);
        self.modified_nanos.hash(state);
        self.width_cells.hash(state);
        self.height_cells.hash(state);
        self.backend_id.hash(state);
    }
}

impl ThumbKey {
    pub fn for_entry(
        entry: &ImageEntry,
        width_cells: u16,
        height_cells: u16,
        backend_id: &str,
    ) -> Self {
        Self {
            path: entry.path.clone(),
            file_len: entry.file_len,
            modified_nanos: entry.modified.and_then(system_time_nanos),
            width_cells,
            height_cells,
            backend_id: backend_id.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Eq)]
pub struct PreviewKey {
    pub path: PathBuf,
    pub file_len: u64,
    pub modified_nanos: Option<u128>,
    pub width_cells: u16,
    pub height_cells: u16,
    pub zoom: ZoomMode,
    pub backend_id: String,
}

impl PartialEq for PreviewKey {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.file_len == other.file_len
            && self.modified_nanos == other.modified_nanos
            && self.width_cells == other.width_cells
            && self.height_cells == other.height_cells
            && self.zoom == other.zoom
            && self.backend_id == other.backend_id
    }
}

impl Hash for PreviewKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.file_len.hash(state);
        self.modified_nanos.hash(state);
        self.width_cells.hash(state);
        self.height_cells.hash(state);
        self.zoom.hash(state);
        self.backend_id.hash(state);
    }
}

impl PreviewKey {
    pub fn for_entry(
        entry: &ImageEntry,
        width_cells: u16,
        height_cells: u16,
        zoom: ZoomMode,
        backend_id: &str,
    ) -> Self {
        Self {
            path: entry.path.clone(),
            file_len: entry.file_len,
            modified_nanos: entry.modified.and_then(system_time_nanos),
            width_cells,
            height_cells,
            zoom,
            backend_id: backend_id.to_owned(),
        }
    }
}

#[derive(Clone)]
pub enum ThumbnailStatus {
    Ready {
        key: ThumbKey,
        protocol: Arc<Protocol>,
    },
    Loading,
    Failed(String),
}

#[derive(Clone)]
pub enum PreviewStatus {
    Ready {
        key: PreviewKey,
        protocol: Arc<Protocol>,
    },
    Loading,
    Failed(String),
}

#[derive(Clone)]
pub struct ThumbJob {
    pub key: ThumbKey,
    pub generation: u64,
    pub orientation: Option<u16>,
    pub picker: Picker,
}

#[derive(Clone)]
pub struct PreviewJob {
    pub key: PreviewKey,
    pub generation: u64,
    pub orientation: Option<u16>,
    pub picker: Picker,
}

pub struct ThumbResult {
    pub key: ThumbKey,
    pub generation: u64,
    pub result: Result<Arc<Protocol>, String>,
    pub bytes: usize,
}

pub struct PreviewResult {
    pub key: PreviewKey,
    pub generation: u64,
    pub result: Result<Arc<Protocol>, String>,
    pub bytes: usize,
}

enum RenderJob {
    Thumbnail(ThumbJob),
    Preview(PreviewJob),
}

enum RenderResult {
    Thumbnail(ThumbResult),
    Preview(PreviewResult),
}

pub struct ThumbnailService {
    thumbnail_cache: ProtocolCache<ThumbKey>,
    preview_cache: ProtocolCache<PreviewKey>,
    thumbnail_failures: HashMap<ThumbKey, String>,
    preview_failures: HashMap<PreviewKey, String>,
    thumbnail_inflight: HashSet<ThumbKey>,
    preview_inflight: HashSet<PreviewKey>,
    job_tx: Sender<RenderJob>,
    result_rx: Receiver<RenderResult>,
    picker: Option<Picker>,
    backend_id: String,
}

impl ThumbnailService {
    pub fn new(cache_mb: usize) -> Self {
        let (job_tx, job_rx) = flume::bounded::<RenderJob>(512);
        let (result_tx, result_rx) = flume::bounded::<RenderResult>(512);
        spawn_workers(job_rx, result_tx);
        let cache_bytes = cache_mb.saturating_mul(1024 * 1024).max(1);

        Self {
            thumbnail_cache: ProtocolCache::new(4096, cache_bytes),
            preview_cache: ProtocolCache::new(5, cache_bytes),
            thumbnail_failures: HashMap::new(),
            preview_failures: HashMap::new(),
            thumbnail_inflight: HashSet::new(),
            preview_inflight: HashSet::new(),
            job_tx,
            result_rx,
            picker: None,
            backend_id: "unconfigured".to_owned(),
        }
    }

    pub fn configure_renderer(&mut self, picker: Picker, backend_id: String) {
        if self.backend_id != backend_id {
            self.thumbnail_cache.clear();
            self.preview_cache.clear();
            self.thumbnail_failures.clear();
            self.preview_failures.clear();
            self.thumbnail_inflight.clear();
            self.preview_inflight.clear();
        }
        self.picker = Some(picker);
        self.backend_id = backend_id;
    }

    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }

    pub fn has_inflight(&self) -> bool {
        !self.thumbnail_inflight.is_empty() || !self.preview_inflight.is_empty()
    }

    pub fn preview_budget_bytes(&self) -> usize {
        self.preview_cache.budget_bytes()
    }

    pub fn preview_cache_capacity(&self) -> usize {
        self.preview_cache.capacity()
    }

    pub fn ensure_preview_capacity(&mut self, min_capacity: usize) {
        self.preview_cache.ensure_capacity_at_least(min_capacity);
    }

    pub fn preview_inflight_len(&self) -> usize {
        self.preview_inflight.len()
    }

    pub fn get_or_request_thumbnail(
        &mut self,
        entry: &ImageEntry,
        width_cells: u16,
        height_cells: u16,
        generation: u64,
    ) -> ThumbnailStatus {
        let key = ThumbKey::for_entry(
            entry,
            width_cells.max(1),
            height_cells.max(1),
            &self.backend_id,
        );
        if let Some(protocol) = self.thumbnail_cache.get(&key) {
            return ThumbnailStatus::Ready { key, protocol };
        }
        if let Some(error) = self.thumbnail_failures.get(&key) {
            return ThumbnailStatus::Failed(error.clone());
        }
        let _ = self.request_thumbnail_key(entry, key.clone(), generation);
        ThumbnailStatus::Loading
    }

    pub fn prefetch_thumbnail(
        &mut self,
        entry: &ImageEntry,
        width_cells: u16,
        height_cells: u16,
        generation: u64,
    ) -> bool {
        let key = ThumbKey::for_entry(
            entry,
            width_cells.max(1),
            height_cells.max(1),
            &self.backend_id,
        );
        self.request_thumbnail_key(entry, key, generation)
    }

    pub fn get_or_request_preview(
        &mut self,
        entry: &ImageEntry,
        width_cells: u16,
        height_cells: u16,
        zoom: ZoomMode,
        generation: u64,
    ) -> PreviewStatus {
        let key = PreviewKey::for_entry(
            entry,
            width_cells.max(1),
            height_cells.max(1),
            zoom,
            &self.backend_id,
        );
        if let Some(protocol) = self.preview_cache.get(&key) {
            return PreviewStatus::Ready { key, protocol };
        }
        if let Some(error) = self.preview_failures.get(&key) {
            return PreviewStatus::Failed(error.clone());
        }
        let _ = self.request_preview_key(entry, key.clone(), generation);
        PreviewStatus::Loading
    }

    pub fn prefetch_preview(
        &mut self,
        entry: &ImageEntry,
        width_cells: u16,
        height_cells: u16,
        zoom: ZoomMode,
        generation: u64,
    ) -> bool {
        let key = PreviewKey::for_entry(
            entry,
            width_cells.max(1),
            height_cells.max(1),
            zoom,
            &self.backend_id,
        );
        self.request_preview_key(entry, key, generation)
    }

    pub fn poll_finished(&mut self, generation: u64) -> bool {
        let mut changed = false;
        while let Ok(result) = self.result_rx.try_recv() {
            match result {
                RenderResult::Thumbnail(result) => {
                    changed |= self.accept_thumbnail_result(result, generation);
                }
                RenderResult::Preview(result) => {
                    changed |= self.accept_preview_result(result, generation);
                }
            }
        }
        changed
    }

    pub fn clear_for_new_generation(&mut self) {
        self.thumbnail_failures.clear();
        self.preview_failures.clear();
        self.thumbnail_inflight.clear();
        self.preview_inflight.clear();
    }

    pub fn accept_thumbnail_result(&mut self, result: ThumbResult, active_generation: u64) -> bool {
        self.thumbnail_inflight.remove(&result.key);
        if result.generation != active_generation {
            return false;
        }
        match result.result {
            Ok(protocol) => {
                self.thumbnail_cache
                    .insert(result.key, protocol, result.bytes);
                true
            }
            Err(error) => {
                self.thumbnail_failures.insert(result.key, error);
                true
            }
        }
    }

    pub fn accept_preview_result(&mut self, result: PreviewResult, active_generation: u64) -> bool {
        self.preview_inflight.remove(&result.key);
        if result.generation != active_generation {
            return false;
        }
        match result.result {
            Ok(protocol) => {
                self.preview_cache
                    .insert(result.key, protocol, result.bytes);
                true
            }
            Err(error) => {
                self.preview_failures.insert(result.key, error);
                true
            }
        }
    }

    fn request_thumbnail_key(
        &mut self,
        entry: &ImageEntry,
        key: ThumbKey,
        generation: u64,
    ) -> bool {
        if self.thumbnail_cache.contains(&key)
            || self.thumbnail_failures.contains_key(&key)
            || !self.thumbnail_inflight.insert(key.clone())
        {
            return false;
        }

        let Some(picker) = self.picker.clone() else {
            self.thumbnail_inflight.remove(&key);
            return false;
        };

        if self
            .job_tx
            .try_send(RenderJob::Thumbnail(ThumbJob {
                key: key.clone(),
                generation,
                orientation: entry.exif_orientation,
                picker,
            }))
            .is_err()
        {
            self.thumbnail_inflight.remove(&key);
            return false;
        }
        true
    }

    fn request_preview_key(
        &mut self,
        entry: &ImageEntry,
        key: PreviewKey,
        generation: u64,
    ) -> bool {
        if self.preview_cache.contains(&key)
            || self.preview_failures.contains_key(&key)
            || !self.preview_inflight.insert(key.clone())
        {
            return false;
        }

        let Some(picker) = self.picker.clone() else {
            self.preview_inflight.remove(&key);
            return false;
        };

        if self
            .job_tx
            .try_send(RenderJob::Preview(PreviewJob {
                key: key.clone(),
                generation,
                orientation: entry.exif_orientation,
                picker,
            }))
            .is_err()
        {
            self.preview_inflight.remove(&key);
            return false;
        }
        true
    }
}

pub fn generate_thumbnail_for_test(
    path: PathBuf,
    width_cells: u16,
    height_cells: u16,
) -> Result<DynamicImage> {
    let orientation = read_exif_metadata(&path)
        .ok()
        .and_then(|exif| exif.orientation);
    decode_resize(
        path,
        width_cells,
        height_cells,
        ratatui_image::FontSize::new(10, 20),
        orientation,
    )
}

pub fn generate_preview_protocol_for_test(
    path: PathBuf,
    width_cells: u16,
    height_cells: u16,
    zoom: ZoomMode,
) -> Result<Protocol> {
    let picker = Picker::halfblocks();
    let entry = ImageEntry {
        path,
        file_name: Default::default(),
        display_name: String::new(),
        extension: None,
        file_len: 0,
        created: None,
        modified: None,
        discovered_order: 0,
        dimensions: None,
        image_type: None,
        exif_date: None,
        exif_orientation: None,
        dimensions_attempted: false,
        exif_attempted: false,
    };
    build_preview_protocol(PreviewJob {
        key: PreviewKey::for_entry(&entry, width_cells, height_cells, zoom, "test"),
        generation: 0,
        orientation: None,
        picker,
    })
    .map(|(protocol, _bytes)| protocol)
}

fn spawn_workers(job_rx: Receiver<RenderJob>, result_tx: Sender<RenderResult>) {
    let workers = thread::available_parallelism()
        .map(|count| count.get().clamp(2, 6))
        .unwrap_or(2);

    for _ in 0..workers {
        let job_rx = job_rx.clone();
        let result_tx = result_tx.clone();
        thread::spawn(move || {
            for job in job_rx.iter() {
                match job {
                    RenderJob::Thumbnail(job) => {
                        let key = job.key.clone();
                        let generation = job.generation;
                        let (result, bytes) = match build_thumbnail_protocol(job) {
                            Ok((protocol, bytes)) => (Ok(Arc::new(protocol)), bytes),
                            Err(error) => (Err(format!("{error:#}")), 0),
                        };
                        let _ = result_tx.send(RenderResult::Thumbnail(ThumbResult {
                            key,
                            generation,
                            result,
                            bytes,
                        }));
                    }
                    RenderJob::Preview(job) => {
                        let key = job.key.clone();
                        let generation = job.generation;
                        let (result, bytes) = match build_preview_protocol(job) {
                            Ok((protocol, bytes)) => (Ok(Arc::new(protocol)), bytes),
                            Err(error) => (Err(format!("{error:#}")), 0),
                        };
                        let _ = result_tx.send(RenderResult::Preview(PreviewResult {
                            key,
                            generation,
                            result,
                            bytes,
                        }));
                    }
                }
            }
        });
    }
}

fn build_thumbnail_protocol(job: ThumbJob) -> Result<(Protocol, usize)> {
    let started = Instant::now();
    let orientation = orientation_for_path(&job.key.path, job.orientation);
    let image = decode_resize(
        job.key.path.clone(),
        job.key.width_cells,
        job.key.height_cells,
        job.picker.font_size(),
        orientation,
    )?;
    let bytes = image_byte_len(&image);
    let decode_elapsed = started.elapsed();
    let protocol_started = Instant::now();
    let protocol = job
        .picker
        .new_protocol(
            image,
            Size::new(job.key.width_cells, job.key.height_cells),
            Resize::Fit(None),
        )
        .map_err(|error| anyhow!("failed to build thumbnail protocol: {error}"))?;
    tracing::debug!(
        path = %job.key.path.display(),
        width_cells = job.key.width_cells,
        height_cells = job.key.height_cells,
        decode_ms = decode_elapsed.as_millis(),
        protocol_ms = protocol_started.elapsed().as_millis(),
        "built thumbnail protocol"
    );
    Ok((protocol, bytes))
}

fn build_preview_protocol(job: PreviewJob) -> Result<(Protocol, usize)> {
    let started = Instant::now();
    let orientation = orientation_for_path(&job.key.path, job.orientation);
    let image = image::ImageReader::open(&job.key.path)
        .with_context(|| format!("failed to open {}", job.key.path.display()))?
        .decode()
        .with_context(|| format!("failed to decode {}", job.key.path.display()))?;
    let image = apply_orientation(image, orientation);
    let bytes = image_byte_len(&image);
    let decode_elapsed = started.elapsed();
    let resize = match job.key.zoom {
        ZoomMode::Fit => Resize::Fit(None),
        ZoomMode::OriginalPixels => Resize::Crop(None),
    };
    let protocol_started = Instant::now();
    let protocol = job
        .picker
        .new_protocol(
            image,
            Size::new(job.key.width_cells, job.key.height_cells),
            resize,
        )
        .map_err(|error| anyhow!("failed to build preview protocol: {error}"))?;
    tracing::debug!(
        path = %job.key.path.display(),
        width_cells = job.key.width_cells,
        height_cells = job.key.height_cells,
        zoom = ?job.key.zoom,
        decode_ms = decode_elapsed.as_millis(),
        protocol_ms = protocol_started.elapsed().as_millis(),
        "built preview protocol"
    );
    Ok((protocol, bytes))
}

fn orientation_for_path(path: &Path, known: Option<u16>) -> Option<u16> {
    known.or_else(|| {
        let started = Instant::now();
        let orientation = read_exif_metadata(path)
            .ok()
            .and_then(|exif| exif.orientation);
        tracing::debug!(
            path = %path.display(),
            exif_ms = started.elapsed().as_millis(),
            has_orientation = orientation.is_some(),
            "read worker EXIF orientation"
        );
        orientation
    })
}

fn decode_resize(
    path: PathBuf,
    width_cells: u16,
    height_cells: u16,
    font_size: ratatui_image::FontSize,
    orientation: Option<u16>,
) -> Result<DynamicImage> {
    let source = image::ImageReader::open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .decode()
        .with_context(|| format!("failed to decode {}", path.display()))?;

    let source = apply_orientation(source, orientation).to_rgba8();
    let source = DynamicImage::ImageRgba8(source);
    let (target_width, target_height) =
        thumbnail_pixel_size(&source, width_cells, height_cells, font_size);

    let mut dst_image = Image::new(
        target_width,
        target_height,
        source
            .pixel_type()
            .context("unsupported thumbnail pixel type")?,
    );
    let mut resizer = Resizer::new();
    resizer.resize(&source, &mut dst_image, None)?;

    let rgba = image::RgbaImage::from_raw(target_width, target_height, dst_image.buffer().to_vec())
        .context("resized thumbnail buffer had unexpected dimensions")?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

fn thumbnail_pixel_size(
    image: &DynamicImage,
    width_cells: u16,
    height_cells: u16,
    font_size: ratatui_image::FontSize,
) -> (u32, u32) {
    let max_width = u32::from(width_cells.max(1))
        .saturating_mul(u32::from(font_size.width.max(1)))
        .max(1);
    let max_height = u32::from(height_cells.max(1))
        .saturating_mul(u32::from(font_size.height.max(1)))
        .max(1);
    let ratio = (max_width as f64 / image.width() as f64)
        .min(max_height as f64 / image.height() as f64)
        .min(1.0);
    let width = ((image.width() as f64 * ratio).round() as u32).max(1);
    let height = ((image.height() as f64 * ratio).round() as u32).max(1);
    (width, height)
}

fn image_byte_len(image: &DynamicImage) -> usize {
    image.width() as usize * image.height() as usize * 4
}

fn system_time_nanos(value: SystemTime) -> Option<u128> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

struct CachedProtocol {
    protocol: Arc<Protocol>,
    bytes: usize,
}

struct ProtocolCache<K>
where
    K: Hash + Eq,
{
    cache: LruCache<K, CachedProtocol>,
    current_bytes: usize,
    budget_bytes: usize,
}

impl<K> ProtocolCache<K>
where
    K: Hash + Eq,
{
    fn new(capacity: usize, budget_bytes: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("non-zero cache capacity");
        Self {
            cache: LruCache::new(capacity),
            current_bytes: 0,
            budget_bytes: budget_bytes.max(1),
        }
    }

    fn get(&mut self, key: &K) -> Option<Arc<Protocol>> {
        self.cache.get(key).map(|cached| cached.protocol.clone())
    }

    fn insert(&mut self, key: K, protocol: Arc<Protocol>, bytes: usize) {
        if let Some((_key, previous)) = self.cache.push(key, CachedProtocol { protocol, bytes }) {
            self.current_bytes = self.current_bytes.saturating_sub(previous.bytes);
        }
        self.current_bytes = self.current_bytes.saturating_add(bytes);
        self.evict_to_budget();
    }

    fn contains(&self, key: &K) -> bool {
        self.cache.contains(key)
    }

    fn clear(&mut self) {
        self.cache.clear();
        self.current_bytes = 0;
    }

    fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    fn capacity(&self) -> usize {
        self.cache.cap().get()
    }

    fn ensure_capacity_at_least(&mut self, min_capacity: usize) {
        let min_capacity = min_capacity.max(1);
        if min_capacity > self.capacity() {
            let capacity = NonZeroUsize::new(min_capacity).expect("non-zero cache capacity");
            self.cache.resize(capacity);
        }
    }

    fn evict_to_budget(&mut self) {
        while self.current_bytes > self.budget_bytes {
            match self.cache.pop_lru() {
                Some((_key, cached)) => {
                    self.current_bytes = self.current_bytes.saturating_sub(cached.bytes);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ImageKind;
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    fn entry() -> ImageEntry {
        ImageEntry {
            path: PathBuf::from("a.jpg"),
            file_name: OsString::from("a.jpg"),
            display_name: "a.jpg".to_owned(),
            extension: Some("jpg".to_owned()),
            file_len: 10,
            created: None,
            modified: Some(UNIX_EPOCH + Duration::from_secs(1)),
            discovered_order: 0,
            dimensions: None,
            image_type: Some(ImageKind::Jpeg),
            exif_date: None,
            exif_orientation: None,
            dimensions_attempted: false,
            exif_attempted: false,
        }
    }

    fn tiny_protocol() -> Arc<Protocol> {
        let picker = Picker::halfblocks();
        Arc::new(
            picker
                .new_protocol(
                    DynamicImage::new_rgba8(1, 1),
                    Size::new(1, 1),
                    Resize::Fit(None),
                )
                .unwrap(),
        )
    }

    #[test]
    fn resize_key_changes_when_cell_size_changes() {
        let entry = entry();

        let small = ThumbKey::for_entry(&entry, 10, 10, "native");
        let large = ThumbKey::for_entry(&entry, 20, 10, "native");

        assert_ne!(small, large);
    }

    #[test]
    fn preview_key_changes_with_zoom_and_backend() {
        let entry = entry();

        let fit = PreviewKey::for_entry(&entry, 80, 24, ZoomMode::Fit, "native:kitty");
        let original =
            PreviewKey::for_entry(&entry, 80, 24, ZoomMode::OriginalPixels, "native:kitty");
        let other_backend = PreviewKey::for_entry(&entry, 80, 24, ZoomMode::Fit, "native:sixel");

        assert_ne!(fit, original);
        assert_ne!(fit, other_backend);
    }

    #[test]
    fn thumbnail_generation_mismatch_discards_stale_result() {
        let mut service = ThumbnailService::new(1);
        let key = ThumbKey::for_entry(&entry(), 10, 10, "native");
        let accepted = service.accept_thumbnail_result(
            ThumbResult {
                key: key.clone(),
                generation: 1,
                result: Ok(tiny_protocol()),
                bytes: 4,
            },
            2,
        );

        assert!(!accepted);
        assert!(!service.thumbnail_cache.contains(&key));
    }

    #[test]
    fn poll_finished_reports_cache_changes() {
        let (job_tx, _job_rx) = flume::bounded(1);
        let (result_tx, result_rx) = flume::bounded(1);
        let mut service = ThumbnailService {
            thumbnail_cache: ProtocolCache::new(1, 1024),
            preview_cache: ProtocolCache::new(1, 1024),
            thumbnail_failures: HashMap::new(),
            preview_failures: HashMap::new(),
            thumbnail_inflight: HashSet::new(),
            preview_inflight: HashSet::new(),
            job_tx,
            result_rx,
            picker: None,
            backend_id: "native".to_owned(),
        };
        let key = ThumbKey::for_entry(&entry(), 10, 10, "native");
        result_tx
            .send(RenderResult::Thumbnail(ThumbResult {
                key: key.clone(),
                generation: 2,
                result: Ok(tiny_protocol()),
                bytes: 4,
            }))
            .unwrap();

        assert!(service.poll_finished(2));
        assert!(service.thumbnail_cache.contains(&key));
    }

    #[test]
    fn poll_finished_ignores_stale_results() {
        let (job_tx, _job_rx) = flume::bounded(1);
        let (result_tx, result_rx) = flume::bounded(1);
        let mut service = ThumbnailService {
            thumbnail_cache: ProtocolCache::new(1, 1024),
            preview_cache: ProtocolCache::new(1, 1024),
            thumbnail_failures: HashMap::new(),
            preview_failures: HashMap::new(),
            thumbnail_inflight: HashSet::new(),
            preview_inflight: HashSet::new(),
            job_tx,
            result_rx,
            picker: None,
            backend_id: "native".to_owned(),
        };
        let key = ThumbKey::for_entry(&entry(), 10, 10, "native");
        result_tx
            .send(RenderResult::Thumbnail(ThumbResult {
                key: key.clone(),
                generation: 1,
                result: Ok(tiny_protocol()),
                bytes: 4,
            }))
            .unwrap();

        assert!(!service.poll_finished(2));
        assert!(!service.thumbnail_cache.contains(&key));
    }

    #[test]
    fn preview_generation_mismatch_discards_stale_result() {
        let mut service = ThumbnailService::new(1);
        let key = PreviewKey::for_entry(&entry(), 80, 24, ZoomMode::Fit, "native");
        let accepted = service.accept_preview_result(
            PreviewResult {
                key: key.clone(),
                generation: 1,
                result: Ok(tiny_protocol()),
                bytes: 4,
            },
            2,
        );

        assert!(!accepted);
        assert!(!service.preview_cache.contains(&key));
    }

    #[test]
    fn preview_cache_capacity_can_expand_for_tiny_folders() {
        let mut service = ThumbnailService::new(1);

        assert!(service.preview_cache_capacity() >= 5);
        service.ensure_preview_capacity(16);

        assert!(service.preview_cache_capacity() >= 16);
        assert_eq!(service.preview_budget_bytes(), 1024 * 1024);
    }
}
