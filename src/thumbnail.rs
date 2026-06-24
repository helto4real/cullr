use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use fast_image_resize::{IntoImageView, Resizer, images::Image};
use flume::{Receiver, Sender};
use image::DynamicImage;

use crate::{
    image_cache::MemoryImageCache,
    metadata::{apply_orientation, read_exif_metadata},
    state::ImageEntry,
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

#[derive(Debug, Clone)]
pub enum ThumbnailStatus {
    Ready {
        key: ThumbKey,
        image: Arc<DynamicImage>,
    },
    Loading,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct ThumbJob {
    pub key: ThumbKey,
    pub generation: u64,
    pub orientation: Option<u16>,
}

#[derive(Debug)]
pub struct ThumbResult {
    pub key: ThumbKey,
    pub generation: u64,
    pub result: Result<Arc<DynamicImage>, String>,
    pub bytes: usize,
}

#[derive(Debug)]
pub struct ThumbnailService {
    cache: MemoryImageCache,
    failures: HashMap<ThumbKey, String>,
    inflight: HashSet<ThumbKey>,
    job_tx: Sender<ThumbJob>,
    result_rx: Receiver<ThumbResult>,
}

impl ThumbnailService {
    pub fn new(cache_mb: usize) -> Self {
        let (job_tx, job_rx) = flume::bounded::<ThumbJob>(512);
        let (result_tx, result_rx) = flume::bounded::<ThumbResult>(512);
        spawn_workers(job_rx, result_tx);

        Self {
            cache: MemoryImageCache::new(cache_mb.saturating_mul(1024 * 1024)),
            failures: HashMap::new(),
            inflight: HashSet::new(),
            job_tx,
            result_rx,
        }
    }

    pub fn get_or_request(
        &mut self,
        entry: &ImageEntry,
        width_cells: u16,
        height_cells: u16,
        generation: u64,
        backend_id: &str,
    ) -> ThumbnailStatus {
        let key = ThumbKey::for_entry(entry, width_cells.max(1), height_cells.max(1), backend_id);
        if let Some(image) = self.cache.get(&key) {
            return ThumbnailStatus::Ready { key, image };
        }
        if let Some(error) = self.failures.get(&key) {
            return ThumbnailStatus::Failed(error.clone());
        }
        if self.inflight.insert(key.clone()) {
            let _ = self.job_tx.try_send(ThumbJob {
                key,
                generation,
                orientation: entry.exif_orientation,
            });
        }
        ThumbnailStatus::Loading
    }

    pub fn poll_finished(&mut self, generation: u64) {
        while let Ok(result) = self.result_rx.try_recv() {
            self.accept_result(result, generation);
        }
    }

    pub fn clear_for_new_generation(&mut self) {
        self.failures.clear();
        self.inflight.clear();
    }

    pub fn accept_result(&mut self, result: ThumbResult, active_generation: u64) -> bool {
        self.inflight.remove(&result.key);
        if result.generation != active_generation {
            return false;
        }
        match result.result {
            Ok(image) => {
                self.cache.insert(result.key, image, result.bytes);
                true
            }
            Err(error) => {
                self.failures.insert(result.key, error);
                true
            }
        }
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
    decode_resize(path, width_cells, height_cells, orientation)
}

fn spawn_workers(job_rx: Receiver<ThumbJob>, result_tx: Sender<ThumbResult>) {
    let workers = thread::available_parallelism()
        .map(|count| count.get().clamp(2, 6))
        .unwrap_or(2);

    for _ in 0..workers {
        let job_rx = job_rx.clone();
        let result_tx = result_tx.clone();
        thread::spawn(move || {
            for job in job_rx.iter() {
                let result = decode_resize(
                    job.key.path.clone(),
                    job.key.width_cells,
                    job.key.height_cells,
                    job.orientation,
                );
                let (result, bytes) = match result {
                    Ok(image) => {
                        let bytes = image_byte_len(&image);
                        (Ok(Arc::new(image)), bytes)
                    }
                    Err(error) => (Err(format!("{error:#}")), 0),
                };
                let _ = result_tx.send(ThumbResult {
                    key: job.key,
                    generation: job.generation,
                    result,
                    bytes,
                });
            }
        });
    }
}

fn decode_resize(
    path: PathBuf,
    width_cells: u16,
    height_cells: u16,
    orientation: Option<u16>,
) -> Result<DynamicImage> {
    let source = image::ImageReader::open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .decode()
        .with_context(|| format!("failed to decode {}", path.display()))?;

    let source = apply_orientation(source, orientation).to_rgba8();
    let source = DynamicImage::ImageRgba8(source);
    let (target_width, target_height) = thumbnail_pixel_size(&source, width_cells, height_cells);

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

fn thumbnail_pixel_size(image: &DynamicImage, width_cells: u16, height_cells: u16) -> (u32, u32) {
    let max_width = u32::from(width_cells.max(1)).saturating_mul(10).max(1);
    let max_height = u32::from(height_cells.max(1)).saturating_mul(20).max(1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ImageEntry, ImageKind};
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
        }
    }

    #[test]
    fn resize_key_changes_when_cell_size_changes() {
        let entry = entry();

        let small = ThumbKey::for_entry(&entry, 10, 10, "native");
        let large = ThumbKey::for_entry(&entry, 20, 10, "native");

        assert_ne!(small, large);
    }

    #[test]
    fn generation_mismatch_discards_stale_result() {
        let mut service = ThumbnailService::new(1);
        let key = ThumbKey::for_entry(&entry(), 10, 10, "native");
        let image = Arc::new(DynamicImage::new_rgba8(1, 1));
        let accepted = service.accept_result(
            ThumbResult {
                key: key.clone(),
                generation: 1,
                result: Ok(image),
                bytes: 4,
            },
            2,
        );

        assert!(!accepted);
        assert!(!service.cache.contains(&key));
    }
}
