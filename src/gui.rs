//! GPU-windowed frontend (`cullr-gui`).
//!
//! Replaces the terminal UI with a native window. Reuses the whole non-terminal
//! core — directory scanning, sorting, the delete queue, navigation/zoom state,
//! and the JPEG-accelerated decode pipeline. Images decode once on worker
//! threads and upload as GPU textures, so the GPU rescales to any window size
//! for free (no terminal graphics protocol, no per-frame re-transmission).

use std::{
    collections::{HashMap, HashSet},
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use eframe::egui;
use flume::{Receiver, Sender};
use indexmap::IndexSet;
use lru::LruCache;

use crate::{
    browser::{
        BROWSER_DEFAULT_WIDTH, BROWSER_ROW_HEIGHT, browser_move_index, draw_browser_icon,
        draw_unsupported_file_message, left_elided_text, listed_directory_for_target,
        preferred_browser_index,
    },
    cli::{Cli, CliViewMode},
    decode::{
        decode_memory_limit, decode_rgba_capped_cancellable, estimated_color_image_decode_bytes,
    },
    delete,
    refresh::{
        BrowserRefreshScope, MediaRefreshOutput, MediaRefreshScope, ReadJob, ReadOutput,
        RefreshResult, RefreshService,
    },
    scanner::{ScanOptions, scan_directory, scan_files},
    sorter,
    state::{
        AppState, BrowserEntry, BrowserEntryKind, BrowserPaneFocus, BrowserState, MediaEntry,
        MediaKind, MediaMode, SortMode, ViewMode, ZoomMode,
    },
    video,
    watcher::{MediaWatcher, WatchMessage, WatchPath, path_is_within},
};

#[cfg(test)]
use crate::refresh::prepare_media_refresh;

/// Long-edge cap for fit-to-window decodes. A fit view never needs more pixels
/// than a high-DPI monitor; the GPU handles any further downscaling.
const FIT_CAP: u32 = 3840;
const APP_IDENTITY: &str = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"));
/// Long-edge cap for grid thumbnails.
const THUMB_CAP: u32 = 320;
/// How many media files on each side of the current one to decode ahead in preview.
const PREFETCH_RADIUS: usize = 4;
/// Resident texture budgets. Entry caps guard bookkeeping while byte caps bound
/// the actual CPU/GPU footprint for unusually large media.
const PREVIEW_CACHE_ENTRIES: usize = 64;
const THUMB_CACHE_ENTRIES: usize = 1024;
const PREVIEW_CACHE_BYTES: usize = 512 * 1024 * 1024;
const THUMB_CACHE_BYTES: usize = 192 * 1024 * 1024;
const DECODE_QUEUE_CAPACITY: usize = 48;
const DECODE_RESULT_CAPACITY: usize = 8;
/// Keep image conversion and GPU texture creation from monopolizing a frame.
/// Decoding remains parallel, but uploads are deliberately paced on the UI thread.
const DECODE_UPLOADS_PER_FRAME: usize = 1;
/// Grid cell edge in points.
const CELL: f32 = 168.0;
/// Extra grid rows to decode above/below the viewport for smooth scrolling.
const GRID_PREFETCH_ROWS: usize = 3;
const EMPTY_MEDIA_PROMPT: &str = "press `r` for recursive search or `q` to quit";
/// How long the browser selection must rest before the right pane loads it,
/// so holding j/k doesn't scan every directory the selection passes through.
const BROWSER_APPLY_DEBOUNCE_SECONDS: f64 = 0.12;
const VIDEO_PROGRESS_OVERLAY_SECONDS: f64 = 2.0;
const VIDEO_PROGRESS_OVERLAY_FADE_SECONDS: f32 = 0.18;
const PREVIEW_PAN_STEP: f32 = 80.0;

fn request_background_repaint(enabled: &AtomicBool, request_repaint: impl FnOnce()) {
    if enabled.load(Ordering::Relaxed) {
        request_repaint();
    }
}

fn preview_scale(zoom_mode: ZoomMode, container_size: egui::Vec2, texture_size: egui::Vec2) -> f32 {
    let fit_scale = (container_size.x / texture_size.x).min(container_size.y / texture_size.y);
    match zoom_mode {
        ZoomMode::Fit => fit_scale,
        ZoomMode::OriginalPixels => 1.0,
        ZoomMode::OriginalPixelsMaxWindow => fit_scale.min(1.0),
    }
}

#[derive(Default)]
struct PreviewPan {
    key: Option<TexKey>,
    offset: egui::Vec2,
    limit: egui::Vec2,
}

impl PreviewPan {
    fn move_by(&mut self, delta: egui::Vec2) {
        self.offset = (self.offset + delta).clamp(-self.limit, self.limit);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Variant {
    Fit,
    Original,
    Thumb,
}

impl Variant {
    fn cap(self) -> u32 {
        match self {
            Variant::Fit => FIT_CAP,
            Variant::Original => u32::MAX,
            Variant::Thumb => THUMB_CAP,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TexKey {
    path: PathBuf,
    variant: Variant,
    media_kind: MediaKind,
    file_len: u64,
    modified: Option<std::time::SystemTime>,
    generation: u64,
}

impl TexKey {
    fn for_entry(entry: &MediaEntry, variant: Variant, generation: u64) -> Self {
        Self {
            path: entry.path.clone(),
            variant,
            media_kind: entry.media_kind.clone(),
            file_len: entry.file_len,
            modified: entry.modified,
            generation,
        }
    }
}

struct DecodeRequest {
    key: TexKey,
    /// Longest edge to decode at: the variant cap, bounded by the GPU's
    /// largest texture so actual-size images can always be uploaded.
    cap: u32,
    work_epoch: u64,
}

struct DecodeResult {
    key: TexKey,
    work_epoch: u64,
    image: std::result::Result<egui::ColorImage, String>,
    _memory_permit: Option<DecodeMemoryPermit>,
}

struct DecodeMemoryBudget {
    capacity: usize,
    available: Mutex<usize>,
    ready: Condvar,
}

impl DecodeMemoryBudget {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            available: Mutex::new(capacity),
            ready: Condvar::new(),
        })
    }

    #[cfg(test)]
    fn acquire(
        self: &Arc<Self>,
        requested: usize,
    ) -> std::result::Result<DecodeMemoryPermit, DecodeMemoryBudgetExceeded> {
        self.acquire_cancellable(requested, &|| false)
            .map(|permit| permit.expect("cancellation is disabled"))
    }

    fn acquire_cancellable(
        self: &Arc<Self>,
        requested: usize,
        cancelled: &impl Fn() -> bool,
    ) -> std::result::Result<Option<DecodeMemoryPermit>, DecodeMemoryBudgetExceeded> {
        if requested == 0 || requested > self.capacity {
            return Err(DecodeMemoryBudgetExceeded {
                requested,
                capacity: self.capacity,
            });
        }
        let bytes = requested;
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *available < bytes {
            if cancelled() {
                return Ok(None);
            }
            let (next, _) = self
                .ready
                .wait_timeout(available, Duration::from_millis(20))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            available = next;
        }
        if cancelled() {
            return Ok(None);
        }
        *available -= bytes;
        Ok(Some(DecodeMemoryPermit {
            budget: Arc::clone(self),
            bytes,
        }))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("decode needs {requested} bytes but the shared budget is {capacity} bytes")]
struct DecodeMemoryBudgetExceeded {
    requested: usize,
    capacity: usize,
}

struct DecodeMemoryPermit {
    budget: Arc<DecodeMemoryBudget>,
    bytes: usize,
}

impl Drop for DecodeMemoryPermit {
    fn drop(&mut self) {
        let mut available = self
            .budget
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available = (*available)
            .saturating_add(self.bytes)
            .min(self.budget.capacity);
        self.budget.ready.notify_all();
    }
}

/// A small worker pool that turns paths into oriented RGBA buffers.
struct DecodeService {
    job_tx: Sender<DecodeRequest>,
    result_rx: Receiver<DecodeResult>,
    work_epoch: Arc<AtomicU64>,
    memory_budget: Arc<DecodeMemoryBudget>,
    repaint: Arc<Mutex<video::RepaintSignal>>,
}

impl DecodeService {
    fn new(suspended: Arc<AtomicBool>) -> Self {
        let (job_tx, job_rx) = flume::bounded::<DecodeRequest>(DECODE_QUEUE_CAPACITY);
        let (result_tx, result_rx) = flume::bounded::<DecodeResult>(DECODE_RESULT_CAPACITY);
        let work_epoch = Arc::new(AtomicU64::new(0));
        let memory_budget = DecodeMemoryBudget::new(decode_memory_limit());
        let repaint: Arc<Mutex<video::RepaintSignal>> = Arc::new(Mutex::new(Arc::new(|| {})));
        let workers = thread::available_parallelism()
            .map(|count| count.get().clamp(2, 6))
            .unwrap_or(2);
        for _ in 0..workers {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let work_epoch = work_epoch.clone();
            let memory_budget = Arc::clone(&memory_budget);
            let suspended = Arc::clone(&suspended);
            let repaint = Arc::clone(&repaint);
            thread::spawn(move || {
                let publish = |result| {
                    if result_tx.send(result).is_ok() && !suspended.load(Ordering::Acquire) {
                        let signal = repaint
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone();
                        signal();
                    }
                };
                for job in job_rx.iter() {
                    let cancelled = || {
                        suspended.load(Ordering::Acquire)
                            || job.work_epoch != work_epoch.load(Ordering::Acquire)
                    };
                    if cancelled() {
                        continue;
                    }
                    let reservation = match decode_memory_reservation(
                        &job.key,
                        job.cap,
                        memory_budget.capacity,
                    ) {
                        Ok(reservation) => reservation,
                        Err(error) => {
                            publish(DecodeResult {
                                key: job.key,
                                work_epoch: job.work_epoch,
                                image: Err(error),
                                _memory_permit: None,
                            });
                            continue;
                        }
                    };
                    if cancelled() {
                        continue;
                    }
                    let memory_permit =
                        match memory_budget.acquire_cancellable(reservation, &cancelled) {
                            Ok(Some(permit)) => permit,
                            Ok(None) => continue,
                            Err(error) => {
                                publish(DecodeResult {
                                    key: job.key,
                                    work_epoch: job.work_epoch,
                                    image: Err(error.to_string()),
                                    _memory_permit: None,
                                });
                                continue;
                            }
                        };
                    if cancelled() {
                        continue;
                    }
                    let decoded = match &job.key.media_kind {
                        MediaKind::Image(_) => {
                            decode_rgba_capped_cancellable(&job.key.path, job.cap, &cancelled)
                        }
                        MediaKind::Video(_) => video::decode_first_frame_rgba_cancellable(
                            &job.key.path,
                            job.cap,
                            &cancelled,
                        ),
                    };
                    if cancelled() {
                        continue;
                    }
                    let image = match decoded {
                        Ok(Some(image)) => Ok(color_image_from_rgba(image)),
                        Ok(None) => continue,
                        Err(error) => Err(format!("{error:#}")),
                    };
                    tracing::debug!(path = %job.key.path.display(), ok = image.is_ok(), "decoded");
                    let memory_permit = image.is_ok().then_some(memory_permit);
                    publish(DecodeResult {
                        key: job.key,
                        work_epoch: job.work_epoch,
                        image,
                        _memory_permit: memory_permit,
                    });
                }
            });
        }
        Self {
            job_tx,
            result_rx,
            work_epoch,
            memory_budget,
            repaint,
        }
    }

    fn request(
        &self,
        key: TexKey,
        max_texture_side: u32,
    ) -> Result<(), flume::TrySendError<DecodeRequest>> {
        let work_epoch = self.work_epoch.load(Ordering::Relaxed);
        let cap = key.variant.cap().min(max_texture_side.max(1));
        self.job_tx.try_send(DecodeRequest {
            key,
            cap,
            work_epoch,
        })
    }

    fn cancel_pending(&self) {
        self.work_epoch.fetch_add(1, Ordering::AcqRel);
        self.memory_budget.ready.notify_all();
    }

    fn accepts(&self, work_epoch: u64) -> bool {
        work_epoch == self.work_epoch.load(Ordering::Relaxed)
    }
}

fn decode_memory_reservation(
    key: &TexKey,
    cap: u32,
    budget_capacity: usize,
) -> std::result::Result<usize, String> {
    match &key.media_kind {
        MediaKind::Image(_) => {
            estimated_color_image_decode_bytes(&key.path, cap).map_err(|error| format!("{error:#}"))
        }
        // FFmpeg may retain a native-resolution source frame while producing
        // the scaled RGBA frame, so video decode is serialized at full budget.
        MediaKind::Video(_) => Ok(budget_capacity),
    }
}

fn color_image_from_rgba(image: image::RgbaImage) -> egui::ColorImage {
    let size = [image.width() as usize, image.height() as usize];
    egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw())
}

fn preview_decode_allowed(
    index: usize,
    current: usize,
    path: &Path,
    deferred_preview_prefetch: &HashSet<PathBuf>,
) -> bool {
    index == current || !deferred_preview_prefetch.contains(path)
}

struct TextureCache {
    entries: LruCache<TexKey, egui::TextureHandle>,
    resident_bytes: usize,
    max_bytes: usize,
}

impl TextureCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: LruCache::new(NonZeroUsize::new(max_entries).expect("cache size is nonzero")),
            resident_bytes: 0,
            max_bytes,
        }
    }

    fn contains(&self, key: &TexKey) -> bool {
        self.entries.contains(key)
    }

    fn get(&mut self, key: &TexKey) -> Option<&egui::TextureHandle> {
        self.entries.get(key)
    }

    fn put(&mut self, key: TexKey, handle: egui::TextureHandle) {
        self.resident_bytes = self
            .resident_bytes
            .saturating_add(texture_byte_len(&handle));
        if let Some((_key, replaced)) = self.entries.push(key, handle) {
            self.resident_bytes = self
                .resident_bytes
                .saturating_sub(texture_byte_len(&replaced));
        }
        while self.resident_bytes > self.max_bytes && self.entries.len() > 1 {
            let Some((_key, evicted)) = self.entries.pop_lru() else {
                break;
            };
            self.resident_bytes = self
                .resident_bytes
                .saturating_sub(texture_byte_len(&evicted));
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.resident_bytes = 0;
    }
}

fn texture_byte_len(handle: &egui::TextureHandle) -> usize {
    let [width, height] = handle.size();
    width.saturating_mul(height).saturating_mul(4)
}

struct GuiApp {
    state: AppState,
    decoder: DecodeService,
    video_events: video::PlaybackEvents,
    video_repaint: video::RepaintSignal,
    previews: TextureCache,
    thumbs: TextureCache,
    inflight: HashSet<TexKey>,
    failed: HashMap<TexKey, String>,
    decode_generation: u64,
    decode_retry_pending: bool,
    locale: Option<String>,
    dry_run: bool,
    status: String,
    right_pane_error: Option<String>,
    preview_pan: PreviewPan,
    /// Largest texture edge the renderer accepts, read from egui every frame.
    max_texture_side: u32,
    grid_cols: usize,
    grid_rows: usize,
    browser_rows: usize,
    last_browser_visible_rows: std::ops::Range<usize>,
    pending_browser_apply: bool,
    browser_apply_deadline: Option<f64>,
    last_visible_rows: std::ops::Range<usize>,
    scroll_to_current: bool,
    fullscreen: bool,
    window_focused: bool,
    active_video: Option<ActiveVideo>,
    pending_initial_video_autoplay: Option<PathBuf>,
    video_muted: bool,
    selection_autoplay_armed: bool,
    repeat_video: bool,
    auto_next: bool,
    media_type_badges_visible: bool,
    video_progress_overlay_visible_until: f64,
    empty_media_target: PathBuf,
    // Deferred from inside the input closure (which borrows ctx immutably).
    pending_sort: Option<crate::state::SortMode>,
    pending_enrich: bool,
    pending_rescan: bool,
    deferred_preview_prefetch: HashSet<PathBuf>,
    deferred_live_refresh_pending: bool,
    pending_live_navigation: Option<isize>,
    // Paused previews stop expensive decode/upload work, but must not stop the
    // live library refresh that can make a pending boundary move possible.
    preview_work_suspended: Arc<AtomicBool>,
    live_refresh_suspended: Arc<AtomicBool>,
    live_refresh_repaint_enabled: Arc<AtomicBool>,
    watcher: Option<MediaWatcher>,
    watcher_error: Option<String>,
    refresh: Option<RefreshService>,
    pending_read: Option<PendingRead>,
    read_generation: u64,
    read_inflight: bool,
    media_refresh_generation: u64,
    browser_refresh_generation: u64,
    media_refresh_inflight: bool,
    browser_refresh_inflight: bool,
}

struct PendingRead {
    job: ReadJob,
    effect: ReadEffect,
}

enum ReadEffect {
    Browse {
        browser: BrowserState,
        preferred: Option<PathBuf>,
        force_reload: bool,
    },
    Load(ViewLoad),
    SortMedia(SortMode),
    SortBrowser(SortMode),
    Metadata,
}

#[derive(Clone)]
struct ViewLoad {
    directory: PathBuf,
    selected_files: Option<IndexSet<PathBuf>>,
    preferred: Option<PathBuf>,
    required_file: Option<PathBuf>,
    mode: ViewMode,
    target: PathBuf,
    browser: Option<BrowserState>,
}

struct ActiveVideo {
    handle: video::PlaybackHandle,
    texture: Option<egui::TextureHandle>,
    position: Duration,
    duration: Option<Duration>,
    ended: bool,
}

impl GuiApp {
    fn new(
        state: AppState,
        locale: Option<String>,
        dry_run: bool,
        auto_next: bool,
        empty_media_target: PathBuf,
    ) -> Self {
        let preview_work_suspended = Arc::new(AtomicBool::new(false));
        let live_refresh_suspended = Arc::new(AtomicBool::new(false));
        let status = if state.entries.is_empty() {
            empty_media_status(&empty_media_target)
        } else {
            String::new()
        };
        Self {
            state,
            decoder: DecodeService::new(Arc::clone(&preview_work_suspended)),
            video_events: video::PlaybackEvents::default(),
            video_repaint: Arc::new(|| {}),
            previews: TextureCache::new(PREVIEW_CACHE_ENTRIES, PREVIEW_CACHE_BYTES),
            thumbs: TextureCache::new(THUMB_CACHE_ENTRIES, THUMB_CACHE_BYTES),
            inflight: HashSet::new(),
            failed: HashMap::new(),
            decode_generation: 0,
            decode_retry_pending: false,
            locale,
            dry_run,
            status,
            right_pane_error: None,
            preview_pan: PreviewPan::default(),
            max_texture_side: u32::MAX,
            grid_cols: 1,
            grid_rows: 1,
            browser_rows: 1,
            last_browser_visible_rows: 0..0,
            pending_browser_apply: false,
            browser_apply_deadline: None,
            last_visible_rows: 0..0,
            scroll_to_current: false,
            fullscreen: false,
            window_focused: true,
            active_video: None,
            pending_initial_video_autoplay: None,
            video_muted: true,
            selection_autoplay_armed: false,
            repeat_video: false,
            auto_next,
            media_type_badges_visible: true,
            video_progress_overlay_visible_until: 0.0,
            empty_media_target,
            pending_sort: None,
            pending_enrich: false,
            pending_rescan: false,
            deferred_preview_prefetch: HashSet::new(),
            deferred_live_refresh_pending: false,
            pending_live_navigation: None,
            preview_work_suspended,
            live_refresh_suspended,
            live_refresh_repaint_enabled: Arc::new(AtomicBool::new(true)),
            watcher: None,
            watcher_error: None,
            refresh: None,
            pending_read: None,
            read_generation: 0,
            read_inflight: false,
            media_refresh_generation: 0,
            browser_refresh_generation: 0,
            media_refresh_inflight: false,
            browser_refresh_inflight: false,
        }
    }

    fn media_actions_blocked(&self) -> bool {
        self.pending_browser_apply
            || self.browser_apply_deadline.is_some()
            || self.pending_read.as_ref().is_some_and(|read| {
                matches!(read.effect, ReadEffect::Browse { .. } | ReadEffect::Load(_))
            })
    }

    fn cancel_read(&mut self) {
        if self.media_refresh_inflight || self.browser_refresh_inflight {
            self.deferred_live_refresh_pending = true;
        }
        self.media_refresh_generation = self.media_refresh_generation.wrapping_add(1);
        self.browser_refresh_generation = self.browser_refresh_generation.wrapping_add(1);
        self.media_refresh_inflight = false;
        self.browser_refresh_inflight = false;
        self.read_generation = self.read_generation.wrapping_add(1);
        self.pending_read = None;
        self.read_inflight = false;
        if let Some(refresh) = &self.refresh {
            refresh.cancel_all();
        }
        self.sync_work_suspension_state();
    }

    fn begin_read(&mut self, job: ReadJob, effect: ReadEffect) {
        self.cancel_read();
        self.pending_read = Some(PendingRead { job, effect });
        if self.media_actions_blocked() {
            self.state.confirm_delete = false;
            self.stop_active_video();
        }
        self.status = "loading…".to_owned();
        self.sync_work_suspension_state();
        self.resume_read();
    }

    fn resume_read(&mut self) {
        if self.read_inflight || !self.window_focused {
            return;
        }
        let Some(read) = &self.pending_read else {
            return;
        };
        let signal = Arc::clone(&self.decoder.repaint);
        let refresh = self.refresh.get_or_insert_with(|| {
            RefreshService::new(Arc::clone(&self.live_refresh_suspended), move || {
                let callback = signal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                callback();
            })
        });
        refresh.request_read(self.read_generation, read.job.clone());
        self.read_inflight = true;
    }

    fn handle_read_result(&mut self, generation: u64, result: Result<ReadOutput, String>) {
        if generation != self.read_generation {
            return;
        }
        self.read_inflight = false;
        if !self.window_focused {
            return;
        }
        let Some(read) = self.pending_read.take() else {
            return;
        };
        self.sync_work_suspension_state();
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                if matches!(read.effect, ReadEffect::Load(_)) {
                    self.right_pane_error = Some(format!("Failed to scan media\n{error}"));
                }
                self.status = format!("load failed: {error}");
                return;
            }
        };
        match (read.effect, output) {
            (
                ReadEffect::Browse {
                    mut browser,
                    preferred,
                    force_reload,
                },
                ReadOutput::Browser(entries),
            ) => {
                let remembered = browser.remembered_selection.get(&browser.listed_directory);
                browser.selected_index =
                    preferred_browser_index(&entries, preferred.as_ref().or(remembered), 0);
                browser.entries = entries;
                browser.remember_current_selection();
                self.load_browser_preview(browser, force_reload);
            }
            (ReadEffect::Load(load), ReadOutput::Media(entries)) => {
                if load
                    .required_file
                    .as_ref()
                    .is_some_and(|path| !entries.iter().any(|e| &e.path == path))
                {
                    self.right_pane_error =
                        Some("Selected file is no longer available after rescanning".to_owned());
                    self.status = "selected file disappeared".to_owned();
                    return;
                }
                let queued = self.state.queue_count();
                self.invalidate_decodes();
                self.state.delete_queue.clear();
                self.state.directory = load.directory;
                self.state.selected_files = load.selected_files;
                self.state.mode = load.mode;
                self.empty_media_target = load.target;
                self.state
                    .set_entries_preserving_current(entries, load.preferred);
                if let Some(browser) = load.browser {
                    self.state.browser = Some(browser);
                }
                self.right_pane_error = None;
                self.status = if self.state.entries.is_empty() {
                    empty_media_status(&self.empty_media_target)
                } else {
                    format!("scanned {} media files", self.state.entries.len())
                };
                if queued > 0 {
                    self.status
                        .push_str(&format!("; cleared {queued} queued deletion(s)"));
                }
                self.sync_browser_to_current_path();
                self.scroll_to_current = true;
            }
            (ReadEffect::SortMedia(mode), ReadOutput::Media(entries)) => {
                let current = self.state.current_path();
                self.state.sort_mode = mode;
                self.state.set_entries_preserving_current(entries, current);
                self.sync_browser_to_current_path();
                self.scroll_to_current = true;
                self.status = format!("sort: {mode:?}");
            }
            (ReadEffect::SortBrowser(mode), ReadOutput::Browser(entries)) => {
                self.apply_browser_listing_preserving_selection(entries);
                if let Some(browser) = self.state.browser.as_mut() {
                    browser.sort_mode = mode;
                }
                self.status = format!("browser sort: {mode:?}");
            }
            (ReadEffect::Metadata, ReadOutput::Metadata(enriched)) => {
                if let Some(entry) = self.state.current_entry_mut()
                    && entry.path == enriched.path
                    && entry.file_len == enriched.file_len
                    && entry.modified == enriched.modified
                {
                    *entry = enriched;
                }
                self.status = "metadata loaded".to_owned();
            }
            _ => unreachable!("read result must match its request"),
        }
        self.sync_work_suspension_state();
    }

    fn queue_browser(
        &mut self,
        directory: PathBuf,
        preferred: Option<PathBuf>,
        force_reload: bool,
    ) {
        let (remembered_selection, sort_mode) = self
            .state
            .browser
            .as_ref()
            .map(|b| (b.remembered_selection.clone(), b.sort_mode))
            .unwrap_or((HashMap::new(), SortMode::NameAsc));
        let scope = BrowserRefreshScope {
            directory: directory.clone(),
            include_hidden: self.state.include_hidden,
            extensions: self.state.extensions.clone(),
            sort_mode,
        };
        let browser = BrowserState {
            listed_directory: directory,
            entries: Vec::new(),
            selected_index: 0,
            focus: BrowserPaneFocus::Browser,
            sort_mode,
            remembered_selection,
            scroll_to_selection: true,
        };
        self.begin_read(
            ReadJob::Browser(scope),
            ReadEffect::Browse {
                browser,
                preferred,
                force_reload,
            },
        );
    }

    fn load_browser_preview(&mut self, browser: BrowserState, force_reload: bool) {
        let selected = browser.entries.get(browser.selected_index).cloned();
        match selected {
            Some(entry) if entry.kind == BrowserEntryKind::UnsupportedFile => {
                self.state.browser = Some(browser);
                self.stop_active_video();
                self.state.mode = ViewMode::Preview;
                self.right_pane_error = None;
                self.empty_media_target = entry.path;
                self.status = "unsupported file".to_owned();
            }
            Some(entry) if matches!(entry.kind, BrowserEntryKind::Media(_)) => {
                self.queue_file(entry.path, force_reload, Some(browser));
            }
            entry => {
                let directory = entry
                    .map(|e| e.path)
                    .unwrap_or_else(|| browser.listed_directory.clone());
                self.queue_directory(directory, Some(browser));
            }
        }
    }

    fn queue_directory(&mut self, directory: PathBuf, browser: Option<BrowserState>) {
        let preferred = (self.state.directory == directory)
            .then(|| self.state.current_path())
            .flatten();
        self.queue_view(ViewLoad {
            directory: directory.clone(),
            selected_files: None,
            preferred,
            required_file: None,
            mode: ViewMode::Grid,
            target: directory,
            browser,
        });
    }

    fn queue_view(&mut self, load: ViewLoad) {
        let scope = if let Some(files) = &load.selected_files {
            MediaRefreshScope::SelectedFiles {
                files: files.iter().cloned().collect(),
                extensions: self.state.extensions.clone(),
            }
        } else {
            MediaRefreshScope::Directory(ScanOptions {
                root: load.directory.clone(),
                recursive: self.state.recursive,
                include_hidden: self.state.include_hidden,
                extensions: self.state.extensions.clone(),
            })
        };
        self.begin_read(
            ReadJob::Media {
                scope,
                sort_mode: self.state.sort_mode,
                locale: self.locale.clone(),
            },
            ReadEffect::Load(load),
        );
    }

    fn queue_file(&mut self, path: PathBuf, force_reload: bool, browser: Option<BrowserState>) {
        let Some(directory) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        if !force_reload
            && self.state.selected_files.is_none()
            && self.state.directory == directory
            && let Some(index) = self
                .state
                .entries
                .iter()
                .position(|entry| entry.path == path)
        {
            self.cancel_read();
            self.stop_active_video();
            if let Some(browser) = browser {
                self.state.browser = Some(browser);
            }
            self.state.mode = ViewMode::Preview;
            self.state.current_index = index;
            self.empty_media_target = path;
            self.right_pane_error = None;
            self.scroll_to_current = true;
            self.status = "preview".to_owned();
            return;
        }
        self.queue_view(ViewLoad {
            directory,
            selected_files: None,
            preferred: Some(path.clone()),
            required_file: Some(path.clone()),
            mode: ViewMode::Preview,
            target: path,
            browser,
        });
    }

    fn preview_variant(&self) -> Variant {
        match self.state.zoom_mode {
            ZoomMode::Fit | ZoomMode::OriginalPixelsMaxWindow => Variant::Fit,
            ZoomMode::OriginalPixels => Variant::Original,
        }
    }

    /// Ask the worker pool to decode `key` unless it is already available.
    fn request(&mut self, key: TexKey) {
        let cached = match key.variant {
            Variant::Thumb => self.thumbs.contains(&key),
            _ => self.previews.contains(&key),
        };
        if cached || self.inflight.contains(&key) || self.failed.contains_key(&key) {
            return;
        }
        match self.decoder.request(key.clone(), self.max_texture_side) {
            Ok(()) => {
                self.inflight.insert(key);
            }
            Err(flume::TrySendError::Full(_)) => self.decode_retry_pending = true,
            Err(flume::TrySendError::Disconnected(_)) => {
                self.failed
                    .insert(key, "decode workers unavailable".to_owned());
            }
        }
    }

    fn invalidate_decodes(&mut self) {
        self.decode_generation = self.decode_generation.wrapping_add(1);
        self.decoder.cancel_pending();
        self.previews.clear();
        self.thumbs.clear();
        self.inflight.clear();
        self.failed.clear();
        self.deferred_preview_prefetch.clear();
    }

    fn cancel_pending_decodes(&mut self) {
        self.decoder.cancel_pending();
        self.inflight.clear();
        self.decode_retry_pending = false;
    }

    /// Move the selection by `delta` entries, clamped to the list bounds.
    fn move_by(&mut self, delta: isize) {
        let len = self.state.entries.len();
        if len == 0 {
            return;
        }
        if self.state.mode == ViewMode::DeleteQueueGrid {
            let previous = self.state.current_index;
            self.state.move_in_queue_by(delta);
            self.finish_selection_change(previous);
            return;
        }
        let current = self.state.current_index as isize;
        let target = (current + delta).clamp(0, len as isize - 1);
        if self.state.mode == ViewMode::Preview
            && target == current
            && delta != 0
            && (self.media_refresh_inflight || self.deferred_live_refresh_pending)
        {
            self.pending_live_navigation = Some(delta);
            return;
        }
        self.set_current_index(target as usize);
    }

    fn toggle_queue_current_and_select_next_media(&mut self) {
        let previous = self.state.current_index;
        let in_delete_queue = self.state.mode == ViewMode::DeleteQueueGrid;
        self.state.toggle_queue_current();

        if in_delete_queue {
            // Removing an entry from the queue already selects the entry that
            // takes its place in the queue grid.
            self.finish_selection_change(previous);
        } else {
            self.move_by(1);
        }

        self.status = format!("queued: {}", self.state.queue_count());
    }

    fn browser_is_focused(&self) -> bool {
        self.state
            .browser
            .as_ref()
            .is_some_and(|browser| browser.focus == BrowserPaneFocus::Browser)
    }

    fn browser_selected_entry(&self) -> Option<&BrowserEntry> {
        self.state
            .browser
            .as_ref()
            .and_then(|browser| browser.entries.get(browser.selected_index))
    }

    fn browser_selected_entry_cloned(&self) -> Option<BrowserEntry> {
        self.browser_selected_entry().cloned()
    }

    fn browser_selected_is_unsupported(&self) -> bool {
        self.browser_selected_entry()
            .is_some_and(|entry| entry.kind == BrowserEntryKind::UnsupportedFile)
    }

    fn browser_selected_is_previewed_media(&self) -> bool {
        self.browser_selected_entry()
            .is_some_and(|entry| matches!(entry.kind, BrowserEntryKind::Media(_)))
    }

    fn set_browser_focus(&mut self, focus: BrowserPaneFocus) {
        if let Some(browser) = self.state.browser.as_mut() {
            browser.focus = focus;
            self.status = match focus {
                BrowserPaneFocus::Browser => "browser focus".to_owned(),
                BrowserPaneFocus::Preview => "preview focus".to_owned(),
            };
        }
    }

    fn toggle_browser_view(&mut self) {
        let opening = self
            .pending_read
            .as_ref()
            .is_some_and(|read| match &read.effect {
                ReadEffect::Browse { .. } => true,
                ReadEffect::Load(load) => load.browser.is_some(),
                _ => false,
            });
        self.cancel_read();
        self.pending_browser_apply = false;
        self.browser_apply_deadline = None;
        if self.state.browser.take().is_some() || opening {
            self.right_pane_error = None;
            self.status = "browser closed".to_owned();
            return;
        }
        if self.state.is_selected_file_scope() {
            self.status = "browser unavailable for explicit selected-file launches".to_owned();
            return;
        }
        let target = if self.state.mode == ViewMode::Preview {
            self.state
                .current_path()
                .unwrap_or_else(|| self.state.directory.clone())
        } else {
            self.state.directory.clone()
        };
        self.queue_browser(listed_directory_for_target(&target), Some(target), false);
    }

    fn refresh_browser_listing_preserving_selection(&mut self) {
        if let Some(browser) = &self.state.browser {
            self.queue_browser(
                browser.listed_directory.clone(),
                browser.selected_path(),
                true,
            );
        }
    }

    fn apply_browser_listing_preserving_selection(&mut self, entries: Vec<BrowserEntry>) {
        let Some(browser) = self.state.browser.as_mut() else {
            return;
        };
        let previous = browser.selected_path();
        let fallback = browser
            .remembered_selection
            .get(&browser.listed_directory)
            .cloned();
        browser.entries = entries;
        browser.selected_index = preferred_browser_index(
            &browser.entries,
            previous.as_ref().or(fallback.as_ref()),
            browser.selected_index,
        );
        browser.scroll_to_selection = true;
        browser.remember_current_selection();
    }

    fn toggle_hidden_files(&mut self) {
        self.state.include_hidden = !self.state.include_hidden;
        self.pending_rescan = true;
    }

    fn select_browser_index(&mut self, index: usize) {
        let Some(browser) = self.state.browser.as_mut() else {
            return;
        };
        if browser.entries.is_empty() {
            return;
        }
        let target = index.min(browser.entries.len() - 1);
        let changed = browser.selected_index != target;
        browser.selected_index = target;
        browser.scroll_to_selection = true;
        browser.remember_current_selection();
        if changed {
            self.cancel_read();
            self.right_pane_error = None;
            // Defer the right-pane load; update() applies it once the
            // selection has rested for BROWSER_APPLY_DEBOUNCE_SECONDS.
            self.pending_browser_apply = true;
        }
    }

    fn move_browser_selection(&mut self, delta: isize) {
        let Some(browser) = self.state.browser.as_ref() else {
            return;
        };
        let target = browser_move_index(browser.selected_index, delta, browser.entries.len());
        self.select_browser_index(target);
    }

    fn pending_browser_sort_mode(&self) -> Option<SortMode> {
        match self.pending_read.as_ref().map(|read| &read.effect) {
            Some(ReadEffect::SortBrowser(mode)) => Some(*mode),
            _ => None,
        }
    }

    fn pending_media_sort_mode(&self) -> SortMode {
        match self.pending_read.as_ref().map(|read| &read.effect) {
            Some(ReadEffect::SortMedia(mode)) => *mode,
            _ => self.state.sort_mode,
        }
    }

    fn resort_browser(&mut self, sort_mode: SortMode) {
        if let Some(browser) = &self.state.browser {
            self.begin_read(
                ReadJob::SortBrowser {
                    entries: browser.entries.clone(),
                    sort_mode,
                },
                ReadEffect::SortBrowser(sort_mode),
            );
        }
    }

    fn enter_selected_browser_directory(&mut self) {
        let Some(entry) = self.browser_selected_entry_cloned() else {
            return;
        };
        if entry.kind != BrowserEntryKind::Directory {
            return;
        }
        self.open_browser_directory(entry.path, None);
    }

    /// Enter on a browser entry: descend into folders, hand media over to the
    /// preview pane.
    fn activate_selected_browser_entry(&mut self) {
        let Some(entry) = self.browser_selected_entry_cloned() else {
            return;
        };
        match entry.kind {
            BrowserEntryKind::Directory => self.open_browser_directory(entry.path, None),
            BrowserEntryKind::Media(_) => self.set_browser_focus(BrowserPaneFocus::Preview),
            BrowserEntryKind::UnsupportedFile => {
                self.status = format!("unsupported file: {}", entry.display_name);
            }
        }
    }

    fn open_browser_parent(&mut self) {
        let Some(browser) = self.state.browser.as_ref() else {
            return;
        };
        let current_directory = browser.listed_directory.clone();
        let Some(parent) = current_directory.parent().map(Path::to_path_buf) else {
            self.status = "already at filesystem root".to_owned();
            return;
        };
        self.open_browser_directory(parent, Some(current_directory));
    }

    fn open_browser_directory(&mut self, directory: PathBuf, preferred_selection: Option<PathBuf>) {
        self.pending_browser_apply = false;
        self.browser_apply_deadline = None;
        self.queue_browser(directory, preferred_selection, false);
    }

    fn apply_browser_selection(&mut self) {
        self.apply_browser_selection_with_force(false);
    }

    fn apply_browser_selection_with_force(&mut self, force_reload: bool) {
        self.pending_browser_apply = false;
        self.browser_apply_deadline = None;
        self.cancel_read();
        let Some(entry) = self.browser_selected_entry_cloned() else {
            if let Some(directory) = self
                .state
                .browser
                .as_ref()
                .map(|browser| browser.listed_directory.clone())
            {
                self.load_right_pane_directory(directory);
            }
            return;
        };

        match entry.kind {
            BrowserEntryKind::Directory => {
                self.load_right_pane_directory(entry.path);
            }
            BrowserEntryKind::Media(_) => {
                self.load_right_pane_file(entry.path, force_reload);
            }
            BrowserEntryKind::UnsupportedFile => {
                self.stop_active_video();
                self.right_pane_error = None;
                self.state.mode = ViewMode::Preview;
                self.empty_media_target = entry.path.clone();
                self.status = format!("unsupported file: {}", entry.display_name);
            }
        }
    }

    fn load_right_pane_directory(&mut self, directory: PathBuf) {
        self.queue_directory(directory, None);
    }

    fn load_right_pane_file(&mut self, path: PathBuf, force_reload: bool) {
        self.queue_file(path, force_reload, None);
    }

    fn sync_browser_to_current_path(&mut self) {
        let Some(current_path) = self.state.current_path() else {
            return;
        };
        let Some(browser) = self.state.browser.as_mut() else {
            return;
        };
        if let Some(index) = browser
            .entries
            .iter()
            .position(|entry| entry.path == current_path)
        {
            browser.selected_index = index;
            browser.scroll_to_selection = true;
            browser.remember_current_selection();
        }
    }

    fn ensure_requested(&mut self) {
        self.decode_retry_pending = false;
        if self.preview_work_is_suspended() {
            return;
        }
        if self.browser_selected_is_unsupported() || self.right_pane_error.is_some() {
            return;
        }
        let len = self.state.entries.len();
        if len == 0 {
            return;
        }
        if self.paused_video_preview_active() {
            return;
        }
        match self.state.mode {
            ViewMode::Preview => {
                let variant = self.preview_variant();
                let current = self.state.current_index;
                let mut wanted = vec![current];
                if variant != Variant::Original {
                    for distance in 1..=PREFETCH_RADIUS {
                        wanted.push((current + distance) % len);
                        wanted.push((current + len - (distance % len)) % len);
                    }
                }
                for index in wanted {
                    let Some(entry) = self.state.entries.get(index) else {
                        continue;
                    };
                    if !preview_decode_allowed(
                        index,
                        current,
                        &entry.path,
                        &self.deferred_preview_prefetch,
                    ) {
                        continue;
                    }
                    let selected_path = (index == current).then(|| entry.path.clone());
                    let key = TexKey::for_entry(entry, variant, self.decode_generation);
                    if let Some(path) = selected_path {
                        self.deferred_preview_prefetch.remove(&path);
                    }
                    self.request(key);
                }
            }
            ViewMode::Grid | ViewMode::DeleteQueueGrid => {
                // Only decode thumbnails for the rows on (or near) screen. This
                // keeps the working set far below the texture cache so visible
                // thumbnails are never evicted — no thrash, no flicker.
                let indices = self.grid_indices();
                let cols = self.grid_cols.max(1);
                let total_rows = indices.len().div_ceil(cols);
                let start = self
                    .last_visible_rows
                    .start
                    .saturating_sub(GRID_PREFETCH_ROWS);
                let end = (self.last_visible_rows.end + GRID_PREFETCH_ROWS).min(total_rows);
                for slot in (start * cols)..(end * cols).min(indices.len()) {
                    if let Some(&index) = indices.get(slot)
                        && let Some(entry) = self.state.entries.get(index)
                    {
                        self.request(TexKey::for_entry(
                            entry,
                            Variant::Thumb,
                            self.decode_generation,
                        ));
                    }
                }
            }
        }
    }

    fn grid_indices(&self) -> Vec<usize> {
        match self.state.mode {
            ViewMode::DeleteQueueGrid => self.state.queued_indices(),
            _ => (0..self.state.entries.len()).collect(),
        }
    }

    fn drain_results(&mut self, ctx: &egui::Context) {
        let mut uploads = 0;
        for _ in 0..self.decoder.result_rx.len() {
            let Ok(result) = self.decoder.result_rx.try_recv() else {
                break;
            };
            if result.key.generation != self.decode_generation
                || !self.decoder.accepts(result.work_epoch)
            {
                continue;
            }
            self.inflight.remove(&result.key);
            if self.preview_work_is_suspended()
                || !self.state.entries.iter().any(|entry| {
                    entry.path == result.key.path
                        && entry.file_len == result.key.file_len
                        && entry.modified == result.key.modified
                        && entry.media_kind == result.key.media_kind
                })
            {
                continue;
            }
            match result.image {
                Ok(color) => {
                    uploads += 1;
                    let handle = ctx.load_texture(
                        result.key.path.to_string_lossy(),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    match result.key.variant {
                        Variant::Thumb => {
                            self.thumbs.put(result.key, handle);
                        }
                        _ => {
                            self.previews.put(result.key, handle);
                        }
                    }
                }
                Err(error) => {
                    self.failed.insert(result.key, error);
                }
            }
            if uploads == DECODE_UPLOADS_PER_FRAME {
                break;
            }
        }
        if !self.preview_work_is_suspended() && !self.decoder.result_rx.is_empty() {
            ctx.request_repaint();
        }
    }

    fn drain_video_events(&mut self, ctx: &egui::Context) {
        let texture_uploads_enabled = !self.preview_work_is_suspended();
        for event in self.video_events.drain() {
            let Some(active) = self.active_video.as_mut() else {
                continue;
            };
            if active.handle.id() != event.playback_id || active.handle.path() != event.path {
                continue;
            }
            if let Some(position) = event.position {
                active.position = position;
            }
            if let Some(duration) = event.duration {
                active.duration = Some(duration);
            }
            let terminal_status = playback_terminal_status(event.ended, event.error.as_deref());
            let failed = event.error.is_some();
            if event.error.is_some() {
                active.ended = true;
            }
            if let Some(color) = event.frame
                && texture_uploads_enabled
                && should_apply_video_frame(active.handle.is_paused(), active.texture.is_some())
            {
                if let Some(texture) = active.texture.as_mut() {
                    texture.set(color, egui::TextureOptions::LINEAR);
                } else {
                    active.texture = Some(ctx.load_texture(
                        format!("video-playback:{}", event.path.display()),
                        color,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
            if event.ended {
                active.ended = true;
                active.handle.set_paused(true);
            }
            if let Some(status) = terminal_status {
                self.status = status;
            }
            if !self.window_focused {
                continue;
            }
            match playback_end_action(event.ended, failed, self.repeat_video, self.auto_next) {
                PlaybackEndAction::None => {}
                PlaybackEndAction::RepeatCurrent => {
                    self.start_video_playback(event.path);
                    self.status = if self.video_muted {
                        "video repeating muted".to_owned()
                    } else {
                        "video repeating with audio".to_owned()
                    };
                }
                PlaybackEndAction::Advance => self.play_next_video_after_current(),
            }
        }
        self.sync_work_suspension_state();
    }

    fn current_is_video(&self) -> bool {
        if self.browser_selected_is_unsupported() || self.right_pane_error.is_some() {
            return false;
        }
        if self.browser_is_focused() && !self.browser_selected_is_previewed_media() {
            return false;
        }
        self.state
            .current_entry()
            .map(|entry| entry.media_kind.is_video())
            .unwrap_or(false)
    }

    fn active_video_for(&self, path: &Path) -> Option<&ActiveVideo> {
        self.active_video
            .as_ref()
            .filter(|active| active.handle.path() == path)
    }

    fn active_video_is_playing_for(&self, path: &Path) -> bool {
        self.active_video_for(path)
            .is_some_and(|active| !active.handle.is_paused() && !active.ended)
    }

    fn active_video_is_playing(&self) -> bool {
        self.active_video
            .as_ref()
            .is_some_and(|active| !active.handle.is_paused() && !active.ended)
    }

    fn paused_video_preview_active(&self) -> bool {
        if self.state.mode != ViewMode::Preview {
            return false;
        }
        let Some(current_path) = self.state.current_entry().map(|entry| &entry.path) else {
            return false;
        };
        self.active_video_for(current_path)
            .is_some_and(|active| active.handle.is_paused() && !active.ended)
    }

    fn preview_work_is_suspended(&self) -> bool {
        self.preview_work_suspended.load(Ordering::Acquire)
    }

    fn live_refresh_is_suspended(&self) -> bool {
        self.live_refresh_suspended.load(Ordering::Acquire)
    }

    fn sync_work_suspension_state(&mut self) {
        let preview_work_suspended = !self.window_focused
            || self.paused_video_preview_active()
            || self.media_actions_blocked();
        let was_preview_work_suspended = self
            .preview_work_suspended
            .swap(preview_work_suspended, Ordering::AcqRel);
        if preview_work_suspended && !was_preview_work_suspended {
            self.cancel_pending_decodes();
        }

        let live_refresh_suspended = !self.window_focused;
        let was_live_refresh_suspended = self
            .live_refresh_suspended
            .swap(live_refresh_suspended, Ordering::AcqRel);
        self.live_refresh_repaint_enabled
            .store(!live_refresh_suspended, Ordering::Release);
        if live_refresh_suspended && !was_live_refresh_suspended {
            self.read_inflight = false;
            self.read_generation = self.read_generation.wrapping_add(1);
            if self.media_refresh_inflight || self.browser_refresh_inflight {
                self.deferred_live_refresh_pending = true;
            }
            self.media_refresh_inflight = false;
            self.browser_refresh_inflight = false;
            if let Some(refresh) = &self.refresh {
                refresh.cancel_all();
            }
        }
    }

    fn active_video_progress_for(&self, path: &Path) -> Option<VideoProgress> {
        self.active_video_for(path)
            .and_then(|active| video_progress(active.position, active.duration))
    }

    fn active_current_video_can_seek(&self) -> bool {
        if self.browser_selected_is_unsupported() || self.right_pane_error.is_some() {
            return false;
        }
        if self.browser_is_focused() && !self.browser_selected_is_previewed_media() {
            return false;
        }
        self.state
            .current_entry()
            .filter(|entry| entry.media_kind.is_video())
            .and_then(|entry| self.active_video_for(&entry.path))
            .is_some_and(|active| !active.ended)
    }

    fn active_current_video_progress(&self) -> Option<VideoProgress> {
        self.state
            .current_entry()
            .filter(|entry| entry.media_kind.is_video())
            .and_then(|entry| self.active_video_progress_for(&entry.path))
    }

    fn show_video_progress_overlay(&mut self, now: f64) {
        if let Some(deadline) = video_progress_overlay_deadline_for_trigger(
            now,
            matches!(self.state.mode, ViewMode::Preview)
                && self.active_current_video_progress().is_some(),
        ) {
            self.video_progress_overlay_visible_until = deadline;
        }
    }

    fn hide_video_progress_overlay(&mut self) {
        self.video_progress_overlay_visible_until = 0.0;
    }

    fn handle_focus_change(&mut self, focused: bool) {
        let effect = focus_playback_effect(
            self.window_focused,
            focused,
            self.active_video_is_playing(),
            self.selection_autoplay_armed,
        );
        if effect.pause_video {
            if let Some(active) = &self.active_video {
                active.handle.set_paused(true);
            }
            self.status = "video paused: window lost focus".to_owned();
        }
        self.selection_autoplay_armed = effect.selection_autoplay_armed;
        self.window_focused = focused;
        self.sync_work_suspension_state();
    }

    fn start_pending_initial_video_autoplay(&mut self, focused: bool) {
        if self.media_actions_blocked() {
            return;
        }
        if let Some(path) = take_pending_initial_video_autoplay(
            focused,
            &mut self.pending_initial_video_autoplay,
            &self.state,
        ) {
            self.start_video_playback(path);
        }
    }

    fn stop_active_video(&mut self) {
        self.video_events.activate(0);
        if let Some(active) = self.active_video.take() {
            active.handle.stop();
        }
        self.sync_work_suspension_state();
        self.hide_video_progress_overlay();
    }

    fn start_video_playback(&mut self, path: PathBuf) {
        self.start_video_playback_from(
            path,
            Duration::ZERO,
            false,
            selection_autoplay_after_playback_start(),
        );
    }

    fn start_video_playback_from(
        &mut self,
        path: PathBuf,
        start_at: Duration,
        paused: bool,
        selection_autoplay_armed: bool,
    ) {
        if !self.window_focused || self.media_actions_blocked() {
            return;
        }
        self.stop_active_video();
        self.selection_autoplay_armed = selection_autoplay_armed;
        let handle = video::spawn_playback_with_repaint(
            path.clone(),
            FIT_CAP,
            self.video_muted,
            start_at,
            paused,
            self.video_events.clone(),
            Arc::clone(&self.video_repaint),
        );
        self.active_video = Some(ActiveVideo {
            handle,
            texture: None,
            position: start_at,
            duration: None,
            ended: false,
        });
        self.sync_work_suspension_state();
        self.status = if paused {
            "video paused".to_owned()
        } else if self.video_muted {
            "video playing muted".to_owned()
        } else {
            "video playing with audio".to_owned()
        };
    }

    fn toggle_current_video_playback(&mut self) {
        if !self.window_focused || self.media_actions_blocked() {
            return;
        }
        let Some(entry) = self.state.current_entry() else {
            return;
        };
        if !entry.media_kind.is_video() {
            return;
        }
        let path = entry.path.clone();

        if let Some(active) = self.active_video.as_mut()
            && active.handle.path() == path
            && !active.ended
        {
            let paused = !active.handle.is_paused();
            active.handle.set_paused(paused);
            self.selection_autoplay_armed = selection_autoplay_after_pause_state(paused);
            self.sync_work_suspension_state();
            self.status = if paused {
                "video paused".to_owned()
            } else {
                "video playing".to_owned()
            };
            return;
        }

        self.start_video_playback(path);
    }

    fn seek_current_video(&mut self, direction: VideoSeekDirection, now: f64) -> bool {
        if self.media_actions_blocked() {
            return false;
        }
        let Some(entry) = self.state.current_entry() else {
            return false;
        };
        if !entry.media_kind.is_video() {
            return false;
        }
        let path = entry.path.clone();
        let Some((position, duration, was_paused)) = self
            .active_video_for(&path)
            .filter(|active| !active.ended)
            .map(|active| (active.position, active.duration, active.handle.is_paused()))
        else {
            return false;
        };
        let Some(target) = video_seek_target(position, duration, direction) else {
            self.status = "video duration unavailable".to_owned();
            return true;
        };
        let duration_for_overlay = duration;

        let selection_autoplay_armed = selection_autoplay_after_seek(was_paused);
        self.start_video_playback_from(path, target, was_paused, selection_autoplay_armed);
        if let Some(active) = self.active_video.as_mut() {
            active.duration = duration_for_overlay;
        }
        self.show_video_progress_overlay(now);
        let verb = match direction {
            VideoSeekDirection::Backward => "rewound",
            VideoSeekDirection::Forward => "fast-forwarded",
        };
        let state = if was_paused { "paused" } else { "playing" };
        self.status = format!("video {verb} 10% ({state})");
        true
    }

    fn autoplay_current_video_if_armed(&mut self) {
        let Some(entry) = self.state.current_entry() else {
            return;
        };
        if !should_autoplay_selected_video(self.selection_autoplay_armed, Some(entry)) {
            return;
        }
        let path = entry.path.clone();
        if self
            .active_video_for(&path)
            .is_some_and(|active| !active.handle.is_paused() && !active.ended)
        {
            return;
        }
        self.start_video_playback(path);
    }

    fn play_next_video_after_current(&mut self) {
        if let Some(index) = next_video_index_after(&self.state.entries, self.state.current_index) {
            self.set_current_index(index);
        } else {
            self.status = "video ended; no next video".to_owned();
        }
    }

    fn toggle_video_mute(&mut self) {
        self.video_muted = !self.video_muted;
        if let Some(active) = &self.active_video {
            active.handle.set_muted(self.video_muted);
        }
        self.status = if self.video_muted {
            "video muted".to_owned()
        } else {
            "video unmuted".to_owned()
        };
    }

    fn toggle_media_type_badges(&mut self) {
        self.media_type_badges_visible = !self.media_type_badges_visible;
        self.status = if self.media_type_badges_visible {
            "media badges shown".to_owned()
        } else {
            "media badges hidden".to_owned()
        };
    }

    fn toggle_auto_next(&mut self) {
        self.auto_next = !self.auto_next;
        self.status = if self.auto_next {
            "auto-next enabled".to_owned()
        } else {
            "auto-next disabled".to_owned()
        };
    }

    fn toggle_repeat_video(&mut self) {
        self.repeat_video = !self.repeat_video;
        self.status = if self.repeat_video {
            "video repeat enabled".to_owned()
        } else {
            "video repeat disabled".to_owned()
        };
    }

    fn set_current_index(&mut self, index: usize) {
        self.pending_live_navigation = None;
        let previous = self.state.current_index;
        self.state.current_index = index.min(self.state.entries.len().saturating_sub(1));
        self.finish_selection_change(previous);
    }

    fn finish_selection_change(&mut self, previous_index: usize) {
        if self.state.current_index != previous_index {
            self.stop_active_video();
            self.autoplay_current_video_if_armed();
        }
        self.sync_browser_to_current_path();
        self.scroll_to_current = true;
    }

    fn resort(&mut self, new_mode: SortMode) {
        self.stop_active_video();
        self.begin_read(
            ReadJob::SortMedia {
                entries: self.state.entries.clone(),
                sort_mode: new_mode,
                locale: self.locale.clone(),
            },
            ReadEffect::SortMedia(new_mode),
        );
    }

    fn rescan(&mut self) {
        // Settings changes during navigation apply to the pending destination.
        if let Some(read) = &self.pending_read {
            match &read.effect {
                ReadEffect::Browse {
                    browser, preferred, ..
                } => {
                    self.queue_browser(browser.listed_directory.clone(), preferred.clone(), true);
                    return;
                }
                ReadEffect::Load(load) => {
                    let load = load.clone();
                    if let Some(browser) = load.browser {
                        self.queue_browser(
                            browser.listed_directory.clone(),
                            browser.selected_path(),
                            true,
                        );
                    } else {
                        self.queue_view(load);
                    }
                    return;
                }
                _ => {}
            }
        }
        if self.state.browser.is_some() {
            self.refresh_browser_listing_preserving_selection();
            return;
        }
        self.queue_view(ViewLoad {
            directory: self.state.directory.clone(),
            selected_files: self.state.selected_files.clone(),
            preferred: self.state.current_path(),
            required_file: None,
            mode: self.state.mode,
            target: self.empty_media_target.clone(),
            browser: None,
        });
    }

    fn attach_repaint(&mut self, ctx: &egui::Context) {
        let video_repaint_ctx = ctx.clone();
        let video_repaint_enabled = Arc::clone(&self.live_refresh_repaint_enabled);
        self.video_repaint = Arc::new(move || {
            request_background_repaint(&video_repaint_enabled, || {
                video_repaint_ctx.request_repaint()
            });
        });
        *self
            .decoder
            .repaint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::clone(&self.video_repaint);
        let signal = Arc::clone(&self.decoder.repaint);
        self.refresh.get_or_insert_with(|| {
            RefreshService::new(Arc::clone(&self.live_refresh_suspended), move || {
                let callback = signal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                callback();
            })
        });
    }

    fn attach_watcher(&mut self, ctx: &egui::Context) {
        self.attach_repaint(ctx);
        let repaint_ctx = ctx.clone();
        let watcher_repaint_enabled = Arc::clone(&self.live_refresh_repaint_enabled);
        match MediaWatcher::new(move || {
            request_background_repaint(&watcher_repaint_enabled, || {
                repaint_ctx.request_repaint();
            });
        }) {
            Ok(watcher) => {
                let polling = watcher.is_polling();
                self.watcher = Some(watcher);
                self.sync_watcher_paths();
                if polling && self.status.is_empty() {
                    self.status = "live watching via polling fallback".to_owned();
                }
            }
            Err(error) => {
                let message = format!("live watching unavailable: {error:#}");
                tracing::warn!(%message);
                self.watcher_error = Some(message.clone());
                self.status = message;
            }
        }
    }

    fn desired_watch_paths(&self) -> HashSet<WatchPath> {
        let mut paths = HashSet::new();
        if let Some(selected_files) = &self.state.selected_files {
            for file in selected_files {
                if let Some(parent) = file.parent() {
                    paths.insert(WatchPath::new(parent.to_path_buf(), false));
                }
            }
        } else {
            paths.insert(WatchPath::new(
                self.state.directory.clone(),
                self.state.recursive,
            ));
        }
        if let Some(browser) = &self.state.browser {
            paths.insert(WatchPath::new(browser.listed_directory.clone(), false));
        }
        paths
    }

    fn sync_watcher_paths(&mut self) {
        let paths = self.desired_watch_paths();
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let was_polling = watcher.is_polling();
        let result = watcher.replace_paths(paths);
        let switched_to_polling = !was_polling && watcher.is_polling();
        if let Err(error) = result {
            let message = format!("live watch update failed: {error:#}");
            if self.watcher_error.as_deref() != Some(&message) {
                tracing::warn!(%message);
                self.status = message.clone();
                self.watcher_error = Some(message);
            }
        } else {
            self.watcher_error = None;
            if switched_to_polling {
                self.status = "live watching via polling fallback".to_owned();
            }
        }
    }

    fn drain_watcher(&mut self) {
        let messages = self
            .watcher
            .as_ref()
            .map(MediaWatcher::drain)
            .unwrap_or_default();
        if messages.is_empty() {
            return;
        }

        let mut paths = HashSet::new();
        let mut watcher_failed = false;
        for message in messages {
            match message {
                WatchMessage::Changed(changed) => paths.extend(changed),
                WatchMessage::Rescan => watcher_failed = true,
                WatchMessage::Error(error) => {
                    watcher_failed = true;
                    tracing::warn!(%error, "filesystem watcher event failed");
                    self.status = format!("live watch error: {error}; reconciling active views");
                }
            }
        }

        // A pending view may target a different directory from the one still displayed.
        // Preserve the notification and reconcile the committed view after the read.
        if self.pending_read.is_some() || self.media_actions_blocked() {
            self.deferred_live_refresh_pending |= watcher_failed || !paths.is_empty();
            self.sync_watcher_paths();
            return;
        }

        let refresh_library = watcher_failed
            || paths
                .iter()
                .any(|path| path_affects_media_state(path, &self.state));
        let refresh_browser = watcher_failed
            || self.state.browser.as_ref().is_some_and(|browser| {
                paths
                    .iter()
                    .any(|path| path_affects_flat_directory(path, &browser.listed_directory))
            });

        if (refresh_library || refresh_browser) && self.live_refresh_is_suspended() {
            self.deferred_live_refresh_pending = true;
            self.status = "live update deferred while window is unfocused".to_owned();
            self.sync_watcher_paths();
            return;
        }

        if refresh_library {
            self.request_live_media_refresh();
        }
        if refresh_browser {
            self.request_live_browser_refresh();
        }
        self.sync_watcher_paths();
    }

    fn media_refresh_scope(&self) -> MediaRefreshScope {
        if let Some(selected_files) = &self.state.selected_files {
            MediaRefreshScope::SelectedFiles {
                files: selected_files.iter().cloned().collect(),
                extensions: self.state.extensions.clone(),
            }
        } else {
            MediaRefreshScope::Directory(ScanOptions {
                root: self.state.directory.clone(),
                recursive: self.state.recursive,
                include_hidden: self.state.include_hidden,
                extensions: self.state.extensions.clone(),
            })
        }
    }

    fn browser_refresh_scope(&self) -> Option<BrowserRefreshScope> {
        let browser = self.state.browser.as_ref()?;
        Some(BrowserRefreshScope {
            directory: browser.listed_directory.clone(),
            include_hidden: self.state.include_hidden,
            extensions: self.state.extensions.clone(),
            sort_mode: browser.sort_mode,
        })
    }

    fn request_live_media_refresh(&mut self) {
        if self.live_refresh_is_suspended()
            || self.pending_read.is_some()
            || self.media_actions_blocked()
        {
            self.deferred_live_refresh_pending = true;
            return;
        }
        let scope = self.media_refresh_scope();
        self.media_refresh_generation = self.media_refresh_generation.wrapping_add(1);
        if let Some(refresh) = &self.refresh {
            self.media_refresh_inflight = true;
            refresh.request_media(
                self.media_refresh_generation,
                scope,
                self.state.entries.clone(),
                self.state.sort_mode,
                self.locale.clone(),
            );
        }
    }

    fn request_live_browser_refresh(&mut self) {
        if self.live_refresh_is_suspended()
            || self.pending_read.is_some()
            || self.media_actions_blocked()
        {
            self.deferred_live_refresh_pending = true;
            return;
        }
        let Some(scope) = self.browser_refresh_scope() else {
            return;
        };
        self.browser_refresh_generation = self.browser_refresh_generation.wrapping_add(1);
        if let Some(refresh) = &self.refresh {
            self.browser_refresh_inflight = true;
            refresh.request_browser(self.browser_refresh_generation, scope);
        }
    }

    fn flush_deferred_live_refresh(&mut self) {
        if !self.deferred_live_refresh_pending
            || self.live_refresh_is_suspended()
            || self.pending_read.is_some()
            || self.media_actions_blocked()
        {
            return;
        }
        self.deferred_live_refresh_pending = false;
        self.request_live_media_refresh();
        self.request_live_browser_refresh();
        self.status = "refreshing changes deferred while window was unfocused".to_owned();
    }

    fn drain_refresh_results(&mut self) {
        let results = self
            .refresh
            .as_ref()
            .map(RefreshService::drain)
            .unwrap_or_default();
        for result in results {
            self.handle_refresh_result(result);
        }
    }

    fn handle_refresh_result(&mut self, refresh: RefreshResult) {
        match refresh {
            RefreshResult::Read { generation, result } => {
                self.handle_read_result(generation, result)
            }
            RefreshResult::Media {
                generation,
                scope,
                sort_mode,
                result,
            } => {
                if generation != self.media_refresh_generation
                    || scope != self.media_refresh_scope()
                {
                    return;
                }
                if sort_mode != self.state.sort_mode {
                    self.media_refresh_inflight = false;
                    self.request_live_media_refresh();
                    return;
                }
                self.media_refresh_inflight = false;
                if self.live_refresh_is_suspended() {
                    self.deferred_live_refresh_pending = true;
                    return;
                }
                match result {
                    Ok(entries) => self.apply_live_media_entries(entries),
                    Err(error) => {
                        self.pending_live_navigation = None;
                        self.status = format!("live refresh failed: {error}");
                    }
                }
            }
            RefreshResult::Browser {
                generation,
                scope,
                result,
            } => {
                if generation != self.browser_refresh_generation
                    || self.browser_refresh_scope().as_ref() != Some(&scope)
                {
                    return;
                }
                self.browser_refresh_inflight = false;
                if self.live_refresh_is_suspended() {
                    self.deferred_live_refresh_pending = true;
                    return;
                }
                match result {
                    Ok(entries) => {
                        self.apply_browser_listing_preserving_selection(entries);
                        self.sync_browser_to_current_path();
                    }
                    Err(error) => self.status = format!("browser live refresh failed: {error}"),
                }
            }
        }
    }

    fn apply_live_media_entries(&mut self, reconciliation: MediaRefreshOutput) {
        let previous_path = self.state.current_path();
        let old_queue = self.state.delete_queue.clone();
        if !reconciliation.changed() {
            self.pending_live_navigation = None;
            return;
        }

        let present_paths = reconciliation
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<HashSet<_>>();
        self.deferred_preview_prefetch
            .retain(|path| present_paths.contains(path));
        self.deferred_preview_prefetch.extend(
            reconciliation
                .entries
                .iter()
                .filter(|entry| !reconciliation.unchanged_paths.contains(&entry.path))
                .map(|entry| entry.path.clone()),
        );

        if previous_path
            .as_ref()
            .is_some_and(|path| reconciliation.invalidated_paths.contains(path))
        {
            self.stop_active_video();
        }
        self.state.delete_queue.retain(|path| {
            reconciliation.unchanged_paths.contains(path) && old_queue.contains(path)
        });
        let unqueued = old_queue
            .len()
            .saturating_sub(self.state.delete_queue.len());
        let added = reconciliation.added;
        let removed = reconciliation.removed;
        let updated = reconciliation.updated;
        self.state
            .set_entries_preserving_current(reconciliation.entries, previous_path);
        if let Some(current_path) = self.state.current_path() {
            self.deferred_preview_prefetch.remove(&current_path);
        }
        self.sync_browser_to_current_path();
        self.scroll_to_current = true;
        self.status = format!("live update: +{} -{} ~{}", added, removed, updated);
        if unqueued > 0 {
            self.status
                .push_str(&format!("; unqueued {unqueued} changed deletion(s)"));
        }
        if let Some(delta) = self.pending_live_navigation.take() {
            self.move_by(delta);
        }
    }

    #[cfg(test)]
    fn reconcile_live_media(&mut self) {
        match scan_state_entries(&self.state) {
            Ok(entries) => {
                let reconciliation = prepare_media_refresh(
                    self.state.entries.clone(),
                    entries,
                    self.state.sort_mode,
                    self.locale.as_deref(),
                );
                self.apply_live_media_entries(reconciliation);
            }
            Err(error) => self.status = format!("live refresh failed: {error}"),
        }
    }

    fn confirm_delete(&mut self) {
        if self.media_actions_blocked() {
            return;
        }
        self.cancel_read();
        self.stop_active_video();
        self.state.confirm_delete = false;
        let report = delete::delete_queued(&mut self.state, self.dry_run);
        let verb = if report.dry_run {
            "would delete"
        } else {
            "deleted"
        };
        if report.failed.is_empty() {
            self.status = format!("{verb} {} files", report.deleted.len());
        } else {
            self.status = format!(
                "{verb} {}; failed {}",
                report.deleted.len(),
                report.failed.len()
            );
        }
        if self.state.entries.is_empty() {
            self.state.mode = ViewMode::Preview;
        }
        if self.state.browser.is_some() {
            self.refresh_browser_listing_preserving_selection();
        }
        self.scroll_to_current = true;
    }

    fn sync_preview_pan(&mut self) {
        let key = (self.state.mode == ViewMode::Preview
            && self.state.zoom_mode == ZoomMode::OriginalPixels
            && self.right_pane_error.is_none()
            && !self.browser_selected_is_unsupported())
        .then(|| {
            self.state
                .current_entry()
                .map(|entry| TexKey::for_entry(entry, Variant::Original, self.decode_generation))
        })
        .flatten();
        if self.preview_pan.key != key {
            self.preview_pan = PreviewPan {
                key,
                ..Default::default()
            };
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        use egui::Key;
        self.sync_preview_pan();
        // Confirmation modal swallows everything except y/n/esc.
        if self.state.confirm_delete && !self.media_actions_blocked() {
            ctx.input(|i| {
                if i.key_pressed(Key::Y) {
                    if y_key_action(true) == YKeyAction::ConfirmDelete {
                        self.confirm_delete();
                    }
                } else if i.key_pressed(Key::N) || i.key_pressed(Key::Escape) {
                    self.state.confirm_delete = false;
                    self.status = "delete cancelled".to_owned();
                }
            });
            return;
        }

        let cols = self.grid_cols.max(1);
        let rows = self.grid_rows.max(1);
        let in_grid = matches!(self.state.mode, ViewMode::Grid | ViewMode::DeleteQueueGrid);
        let browser_half_page = (self.browser_rows / 2).max(1) as isize;

        // Viewport commands must be issued OUTSIDE the input closure — calling
        // them while ctx.input() holds the context lock silently drops them.
        let mut close = false;
        let mut toggle_fullscreen = false;

        ctx.input(|i| {
            if i.modifiers.ctrl && i.modifiers.alt {
                let left = i.key_pressed(Key::H) || i.key_pressed(Key::ArrowLeft);
                let right = i.key_pressed(Key::L) || i.key_pressed(Key::ArrowRight);
                let up = i.key_pressed(Key::K) || i.key_pressed(Key::ArrowUp);
                let down = i.key_pressed(Key::J) || i.key_pressed(Key::ArrowDown);
                if left || right || up || down {
                    if !self.browser_is_focused() && !self.media_actions_blocked() {
                        // Moving the viewport left/up moves the image right/down.
                        self.preview_pan.move_by(
                            egui::vec2(
                                i32::from(left) as f32 - i32::from(right) as f32,
                                i32::from(up) as f32 - i32::from(down) as f32,
                            ) * PREVIEW_PAN_STEP,
                        );
                    }
                    return;
                }
            }
            if i.key_pressed(Key::Q) {
                close = true;
            }
            if i.key_pressed(Key::Escape) {
                // Escape peels back one layer: overlay → grid → quit.
                if self.state.show_help_overlay || self.state.show_info_overlay {
                    self.state.show_help_overlay = false;
                    self.state.show_info_overlay = false;
                } else if in_grid && !self.browser_is_focused() {
                    self.state.mode = ViewMode::Preview;
                } else {
                    close = true;
                }
            }

            if i.key_pressed(Key::E) && !i.modifiers.ctrl {
                self.toggle_browser_view();
            }
            if i.modifiers.ctrl && i.key_pressed(Key::L) {
                self.set_browser_focus(BrowserPaneFocus::Preview);
            }
            if i.modifiers.ctrl && i.key_pressed(Key::H) {
                self.set_browser_focus(BrowserPaneFocus::Browser);
            }
            if i.key_pressed(Key::Period) && !i.modifiers.ctrl {
                self.toggle_hidden_files();
            }

            // Input routes to exactly one pane. An unsupported browser
            // selection blanks the right pane, so its shortcuts are parked
            // until the selection changes.
            let browser_focused = self.browser_is_focused();
            let unsupported_selected = self.browser_selected_is_unsupported();
            let right_pane_active = !browser_focused
                && !unsupported_selected
                && self.right_pane_error.is_none()
                && !self.media_actions_blocked();

            // Keys that follow the active video rather than a pane.
            if i.key_pressed(Key::Space) && self.current_is_video() {
                self.toggle_current_video_playback();
            }
            if !unsupported_selected && i.key_pressed(Key::M) {
                self.toggle_video_mute();
            }
            if i.key_pressed(Key::U) && !i.modifiers.ctrl {
                match contextual_u_key_action(self.active_current_video_can_seek()) {
                    UKeyAction::SeekBackward => {
                        self.seek_current_video(VideoSeekDirection::Backward, i.time);
                    }
                    UKeyAction::Unqueue => {
                        if right_pane_active {
                            self.state.unqueue_current();
                            self.status = format!("queued: {}", self.state.queue_count());
                        }
                    }
                }
            }
            if i.key_pressed(Key::O) && !i.modifiers.ctrl {
                self.seek_current_video(VideoSeekDirection::Forward, i.time);
            }
            if i.key_pressed(Key::Y) && y_key_action(false) == YKeyAction::ShowProgress {
                self.show_video_progress_overlay(i.time);
            }

            // Global toggles.
            if i.key_pressed(Key::F) {
                toggle_fullscreen = true;
            }
            if i.key_pressed(Key::A) {
                self.toggle_auto_next();
            }
            if i.key_pressed(Key::P) {
                self.toggle_repeat_video();
            }
            if i.key_pressed(Key::Questionmark) {
                self.state.show_help_overlay = !self.state.show_help_overlay;
            }
            if i.key_pressed(Key::R) {
                if i.modifiers.ctrl {
                    // Ctrl+R: delete the queued files (with confirmation).
                    if right_pane_active {
                        if self.state.queue_count() == 0 {
                            self.status = "delete queue is empty".to_owned();
                        } else {
                            self.state.confirm_delete = true;
                        }
                    }
                } else if i.modifiers.shift {
                    // Shift+R: rescan the directory.
                    self.pending_rescan = true;
                } else if self.state.is_selected_file_scope() {
                    // In selected-file mode, keep rescans scoped to the explicit file set.
                    self.pending_rescan = true;
                } else {
                    // r: toggle recursive scanning, then rescan.
                    self.state.recursive = !self.state.recursive;
                    self.pending_rescan = true;
                }
            }

            if browser_focused {
                // Browser pane: j/k move, ctrl+d/u half-page, l/h (or ←/→)
                // descend into or leave a folder, enter activates the entry.
                if i.key_pressed(Key::J) || i.key_pressed(Key::ArrowDown) {
                    self.move_browser_selection(1);
                }
                if i.key_pressed(Key::K) || i.key_pressed(Key::ArrowUp) {
                    self.move_browser_selection(-1);
                }
                if i.modifiers.ctrl && i.key_pressed(Key::D) {
                    self.move_browser_selection(browser_half_page);
                }
                if i.modifiers.ctrl && i.key_pressed(Key::U) {
                    self.move_browser_selection(-browser_half_page);
                }
                if !i.modifiers.ctrl && (i.key_pressed(Key::L) || i.key_pressed(Key::ArrowRight)) {
                    self.enter_selected_browser_directory();
                }
                if !i.modifiers.ctrl && (i.key_pressed(Key::H) || i.key_pressed(Key::ArrowLeft)) {
                    self.open_browser_parent();
                }
                if i.key_pressed(Key::Enter) {
                    self.activate_selected_browser_entry();
                }
                if i.key_pressed(Key::Home) {
                    self.select_browser_index(0);
                }
                if i.key_pressed(Key::End)
                    && let Some(browser) = self.state.browser.as_ref()
                {
                    self.select_browser_index(browser.entries.len().saturating_sub(1));
                }
                if i.key_pressed(Key::T)
                    && let Some(browser) = self.state.browser.as_ref()
                {
                    self.resort_browser(sorter::next_time_sort(
                        self.pending_browser_sort_mode()
                            .unwrap_or(browser.sort_mode),
                    ));
                }
                if i.key_pressed(Key::N)
                    && let Some(browser) = self.state.browser.as_ref()
                {
                    self.resort_browser(sorter::next_name_sort(
                        self.pending_browser_sort_mode()
                            .unwrap_or(browser.sort_mode),
                    ));
                }
            } else if right_pane_active {
                if in_grid {
                    // Vim-style spatial navigation in the library grid:
                    // h/l move within a row, j/k move between rows, ctrl+d/u
                    // half-page. Plain moves ignore ctrl so the ctrl+h/ctrl+l
                    // pane-focus chords don't also step the selection.
                    if !i.modifiers.ctrl {
                        if i.key_pressed(Key::H) || i.key_pressed(Key::ArrowLeft) {
                            self.move_by(-1);
                        }
                        if i.key_pressed(Key::L) || i.key_pressed(Key::ArrowRight) {
                            self.move_by(1);
                        }
                        if i.key_pressed(Key::K) || i.key_pressed(Key::ArrowUp) {
                            self.move_by(-(cols as isize));
                        }
                        if i.key_pressed(Key::J) || i.key_pressed(Key::ArrowDown) {
                            self.move_by(cols as isize);
                        }
                    }
                    let half_page = (rows / 2).max(1) as isize * cols as isize;
                    if i.modifiers.ctrl && i.key_pressed(Key::D) {
                        self.move_by(half_page);
                    }
                    if i.modifiers.ctrl && i.key_pressed(Key::U) {
                        self.move_by(-half_page);
                    }
                } else if !i.modifiers.ctrl {
                    // Preview is a one-wide list: h/k/←/↑ previous, l/j/→/↓
                    // next. Ctrl-modified presses belong to the pane-focus
                    // chords, not navigation.
                    if i.key_pressed(Key::L)
                        || i.key_pressed(Key::J)
                        || i.key_pressed(Key::ArrowRight)
                        || i.key_pressed(Key::ArrowDown)
                    {
                        self.move_by(1);
                    }
                    if i.key_pressed(Key::H)
                        || i.key_pressed(Key::K)
                        || i.key_pressed(Key::ArrowLeft)
                        || i.key_pressed(Key::ArrowUp)
                    {
                        self.move_by(-1);
                    }
                }
                if i.key_pressed(Key::Home) {
                    let previous = self.state.current_index;
                    self.state.first();
                    self.finish_selection_change(previous);
                }
                if i.key_pressed(Key::End) {
                    let previous = self.state.current_index;
                    self.state.last();
                    self.finish_selection_change(previous);
                }

                // Delete queue.
                if i.modifiers.shift && i.key_pressed(Key::D) {
                    let before = self.state.current_index;
                    self.state.enter_delete_queue_grid();
                    self.finish_selection_change(before);
                } else if i.key_pressed(Key::D) && !i.modifiers.ctrl {
                    self.toggle_queue_current_and_select_next_media();
                }

                // Views / overlays / zoom / sort.
                if i.key_pressed(Key::G) {
                    self.state.mode = match self.state.mode {
                        ViewMode::Preview => ViewMode::Grid,
                        ViewMode::Grid | ViewMode::DeleteQueueGrid => ViewMode::Preview,
                    };
                    self.scroll_to_current = true;
                }
                if in_grid && i.key_pressed(Key::Enter) {
                    self.state.mode = ViewMode::Preview;
                }
                if i.key_pressed(Key::Z) {
                    self.state.zoom_mode = self.state.zoom_mode.next();
                }
                if i.key_pressed(Key::B) {
                    self.toggle_media_type_badges();
                }
                if i.key_pressed(Key::T) {
                    self.pending_sort =
                        Some(sorter::next_time_sort(self.pending_media_sort_mode()));
                }
                if i.key_pressed(Key::N) {
                    self.pending_sort =
                        Some(sorter::next_name_sort(self.pending_media_sort_mode()));
                }
                if i.key_pressed(Key::I) {
                    self.state.show_info_overlay = !self.state.show_info_overlay;
                    self.pending_enrich = self.state.show_info_overlay;
                }
            }
        });

        if let Some(mode) = self.pending_sort.take() {
            self.resort(mode);
        }
        if std::mem::take(&mut self.pending_enrich)
            && let Some(entry) = self.state.current_entry_mut()
        {
            let entry = entry.clone();
            self.begin_read(ReadJob::Metadata(entry), ReadEffect::Metadata);
        }
        if std::mem::take(&mut self.pending_rescan) {
            self.rescan();
        }
        if toggle_fullscreen {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if close {
            self.stop_active_video();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn draw_browser_pane(&mut self, ui: &mut egui::Ui) {
        let Some(browser) = self.state.browser.as_ref() else {
            return;
        };
        let listed_directory = browser.listed_directory.clone();
        let entry_count = browser.entries.len();
        let selected_index = browser.selected_index;
        let scroll_requested = browser.scroll_to_selection;
        let focused = browser.focus == BrowserPaneFocus::Browser;
        let header = if self.state.include_hidden {
            format!("{}  [hidden]", listed_directory.display())
        } else {
            listed_directory.display().to_string()
        };

        let accent = ui.visuals().selection.bg_fill;
        let title = egui::RichText::new("Files").strong();
        ui.label(if focused { title.color(accent) } else { title });
        ui.add_space(2.0);
        let header_font = egui::FontId::monospace(10.0);
        let header_text = left_elided_text(ui, &header, &header_font, ui.available_width());
        ui.label(egui::RichText::new(header_text).font(header_font));
        // Accent underline marks the pane that owns keyboard input.
        draw_pane_focus_line(ui, focused);

        if entry_count == 0 {
            ui.centered_and_justified(|ui| {
                ui.label("No visible files.");
            });
            return;
        }

        let available_height = ui.available_height();
        self.browser_rows = ((available_height / BROWSER_ROW_HEIGHT).floor() as usize).max(1);
        let mut scroll = egui::ScrollArea::vertical();
        // On navigation, scroll only when the selected row is off-screen, so
        // in-view moves don't jump the viewport around.
        if scroll_requested && !self.last_browser_visible_rows.contains(&selected_index) {
            let target = (selected_index as f32 * BROWSER_ROW_HEIGHT
                - (available_height - BROWSER_ROW_HEIGHT) * 0.5)
                .max(0.0);
            scroll = scroll.vertical_scroll_offset(target);
        }
        let mut shown = 0..0;
        scroll.show_rows(ui, BROWSER_ROW_HEIGHT, entry_count, |ui, row_range| {
            shown = row_range.clone();
            for index in row_range {
                self.draw_browser_row(ui, index);
            }
        });
        self.last_browser_visible_rows = shown;
        if let Some(browser) = self.state.browser.as_mut() {
            browser.scroll_to_selection = false;
        }
    }

    fn draw_browser_row(&mut self, ui: &mut egui::Ui, index: usize) {
        let Some(browser) = self.state.browser.as_ref() else {
            return;
        };
        let Some(entry) = browser.entries.get(index).cloned() else {
            return;
        };
        let selected = index == browser.selected_index;
        let focused = browser.focus == BrowserPaneFocus::Browser;
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), BROWSER_ROW_HEIGHT),
            egui::Sense::click(),
        );

        // The focused pane carries the full selection accent; unfocused keeps
        // a dimmed version so the eye tracks which pane owns input.
        let mut text_color = ui.visuals().text_color();
        if selected {
            let fill = if focused {
                ui.visuals().selection.bg_fill
            } else {
                ui.visuals().selection.bg_fill.gamma_multiply(0.35)
            };
            ui.painter()
                .rect_filled(rect.shrink2(egui::vec2(0.0, 1.0)), 3.0, fill);
            text_color = if focused {
                ui.visuals().selection.stroke.color
            } else {
                ui.visuals().strong_text_color()
            };
        } else if response.hovered() {
            ui.painter().rect_filled(
                rect.shrink2(egui::vec2(0.0, 1.0)),
                3.0,
                ui.visuals().faint_bg_color,
            );
        }

        let icon_rect = egui::Rect::from_min_size(
            rect.left_center() + egui::vec2(8.0, -8.0),
            egui::vec2(16.0, 16.0),
        );
        draw_browser_icon(ui, icon_rect, &entry.kind);
        let text_pos = egui::pos2(icon_rect.right() + 8.0, rect.center().y);
        ui.painter().text(
            text_pos,
            egui::Align2::LEFT_CENTER,
            entry.display_name.clone(),
            egui::FontId::proportional(13.0),
            text_color,
        );

        if response.clicked() {
            self.select_browser_index(index);
        }
        if response.double_clicked() && entry.kind == BrowserEntryKind::Directory {
            self.select_browser_index(index);
            self.enter_selected_browser_directory();
        }
    }

    fn draw_browser_right_pane(&mut self, ui: &mut egui::Ui) {
        let focused = self
            .state
            .browser
            .as_ref()
            .is_some_and(|browser| browser.focus == BrowserPaneFocus::Preview);
        draw_pane_focus_line(ui, focused);
        if let Some(error) = &self.right_pane_error {
            ui.centered_and_justified(|ui| {
                ui.label(error);
            });
            return;
        }
        if let Some(entry) = self.browser_selected_entry()
            && entry.kind == BrowserEntryKind::UnsupportedFile
        {
            draw_unsupported_file_message(ui, &entry.path);
            return;
        }
        match self.state.mode {
            ViewMode::Preview => self.draw_preview(ui),
            ViewMode::Grid | ViewMode::DeleteQueueGrid => self.draw_grid(ui),
        }
    }

    fn draw_preview(&mut self, ui: &mut egui::Ui) {
        self.sync_preview_pan();
        let variant = self.preview_variant();
        let Some(entry) = self.state.current_entry() else {
            draw_empty_media_message(ui, &self.empty_media_target);
            return;
        };
        let key = TexKey::for_entry(entry, variant, self.decode_generation);
        let name = entry.display_name.clone();
        let progress = self.active_video_progress_for(&entry.path);

        if let Some(handle) = self
            .active_video_for(&entry.path)
            .and_then(|active| active.texture.as_ref())
            .cloned()
            .or_else(|| self.previews.get(&key).cloned())
        {
            let (container_rect, _response) =
                ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
            let tex = handle.size_vec2();
            let scale = preview_scale(self.state.zoom_mode, container_rect.size(), tex);
            let draw = tex * scale;
            self.preview_pan.limit = if self.state.zoom_mode == ZoomMode::OriginalPixels {
                ((draw - container_rect.size()) * 0.5).max(egui::Vec2::ZERO)
            } else {
                egui::Vec2::ZERO
            };
            self.preview_pan.move_by(egui::Vec2::ZERO);
            let image_rect = egui::Rect::from_center_size(
                container_rect.center() + self.preview_pan.offset,
                draw,
            );
            if self.preview_pan.limit != egui::Vec2::ZERO && !self.state.confirm_delete {
                let response = ui.interact(
                    image_rect.intersect(container_rect),
                    ui.id().with("preview-pan"),
                    egui::Sense::drag(),
                );
                if response.dragged_by(egui::PointerButton::Primary) {
                    self.preview_pan.move_by(response.drag_delta());
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                } else if response.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
            }
            let image_rect = egui::Rect::from_center_size(
                container_rect.center() + self.preview_pan.offset,
                draw,
            );
            ui.painter()
                .with_clip_rect(container_rect.intersect(ui.clip_rect()))
                .image(
                    handle.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            self.draw_video_progress_overlay(ui, image_rect.intersect(container_rect), progress);
        } else if let Some(error) = self.failed.get(&key) {
            ui.centered_and_justified(|ui| {
                ui.label(format!("Failed to decode {name}\n{error}"));
            });
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(format!("Loading {name}…"));
            });
        }
    }

    fn draw_video_progress_overlay(
        &self,
        ui: &mut egui::Ui,
        video_rect: egui::Rect,
        progress: Option<VideoProgress>,
    ) {
        if !self.window_focused {
            return;
        }
        let now = ui.ctx().input(|input| input.time);
        let target_visible = progress.is_some()
            && video_progress_overlay_target_visible(
                now,
                self.video_progress_overlay_visible_until,
            );
        let alpha = ui.ctx().animate_bool_with_time(
            egui::Id::new("video-progress-overlay"),
            target_visible,
            VIDEO_PROGRESS_OVERLAY_FADE_SECONDS,
        );
        let Some(progress) = progress else {
            return;
        };
        if alpha <= 0.01 || video_rect.width() < 96.0 || video_rect.height() < 54.0 {
            return;
        }

        let margin = (video_rect.width() * 0.035).clamp(8.0, 18.0);
        let overlay_height = 38.0_f32.min((video_rect.height() - margin * 2.0).max(0.0));
        if overlay_height < 24.0 {
            return;
        }
        let overlay_rect = egui::Rect::from_min_max(
            egui::pos2(
                video_rect.left() + margin,
                video_rect.bottom() - margin - overlay_height,
            ),
            egui::pos2(video_rect.right() - margin, video_rect.bottom() - margin),
        );
        if overlay_rect.width() < 80.0 {
            return;
        }

        let painter = ui.painter();
        painter.rect_filled(
            overlay_rect,
            6.0,
            egui::Color32::from_black_alpha(scaled_alpha(alpha, 150)),
        );

        let inner = overlay_rect.shrink2(egui::vec2(10.0, 8.0));
        let label_y = inner.top() + 6.0;
        let text_color = egui::Color32::from_white_alpha(scaled_alpha(alpha, 230));
        painter.text(
            egui::pos2(inner.left(), label_y),
            egui::Align2::LEFT_CENTER,
            format_video_timestamp(progress.position),
            egui::FontId::proportional(12.0),
            text_color,
        );
        painter.text(
            egui::pos2(inner.right(), label_y),
            egui::Align2::RIGHT_CENTER,
            format_video_timestamp(progress.duration),
            egui::FontId::proportional(12.0),
            text_color,
        );

        let bar_rect = egui::Rect::from_min_max(
            egui::pos2(inner.left(), inner.bottom() - 7.0),
            egui::pos2(inner.right(), inner.bottom() - 2.0),
        );
        painter.rect_filled(
            bar_rect,
            3.0,
            egui::Color32::from_white_alpha(scaled_alpha(alpha, 70)),
        );
        let fill_width = bar_rect.width() * progress.fraction;
        if fill_width > 0.5 {
            let fill_rect = egui::Rect::from_min_max(
                bar_rect.left_top(),
                egui::pos2(bar_rect.left() + fill_width, bar_rect.bottom()),
            );
            painter.rect_filled(
                fill_rect,
                3.0,
                egui::Color32::from_rgba_unmultiplied(94, 204, 255, scaled_alpha(alpha, 240)),
            );
        }
    }

    fn draw_grid(&mut self, ui: &mut egui::Ui) {
        let indices = self.grid_indices();
        if indices.is_empty() {
            if self.state.mode == ViewMode::DeleteQueueGrid {
                ui.centered_and_justified(|ui| {
                    ui.label("Delete queue is empty.");
                });
            } else {
                draw_empty_media_message(ui, &self.empty_media_target);
            }
            return;
        }

        let spacing = 8.0;
        ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
        let pitch = CELL + spacing;
        let avail_height = ui.available_height();
        let cols = (((ui.available_width() + spacing) / pitch).floor() as usize).max(1);
        let visible_rows = (((avail_height + spacing) / pitch).floor() as usize).max(1);
        let total_rows = indices.len().div_ceil(cols);
        self.grid_cols = cols;
        self.grid_rows = visible_rows;

        let mut scroll = egui::ScrollArea::vertical();
        // On navigation, scroll only when the current cell is off-screen, so
        // in-view moves don't jump the viewport around.
        if self.scroll_to_current
            && let Some(slot) = indices.iter().position(|&i| i == self.state.current_index)
        {
            let current_row = slot / cols;
            if !self.last_visible_rows.contains(&current_row) {
                let target = (current_row as f32 * pitch - (avail_height - pitch) * 0.5).max(0.0);
                scroll = scroll.vertical_scroll_offset(target);
            }
        }

        // Virtualized: egui only invokes the closure for visible rows, so we
        // lay out and access only a screenful of cells regardless of folder size.
        let mut shown = 0..0;
        scroll.show_rows(ui, CELL, total_rows, |ui, row_range| {
            shown = row_range.clone();
            for row in row_range {
                ui.horizontal(|ui| {
                    for col in 0..cols {
                        let slot = row * cols + col;
                        if let Some(&index) = indices.get(slot) {
                            self.draw_cell(ui, index);
                        }
                    }
                });
            }
        });
        self.last_visible_rows = shown;
    }

    fn draw_cell(&mut self, ui: &mut egui::Ui, index: usize) {
        let Some(entry) = self.state.entries.get(index) else {
            return;
        };
        let path = entry.path.clone();
        let media_kind = entry.media_kind.clone();
        let is_current = index == self.state.current_index;
        let is_queued = self.state.delete_queue.contains(&path);
        let key = TexKey::for_entry(entry, Variant::Thumb, self.decode_generation);

        let (rect, response) = ui.allocate_exact_size(egui::vec2(CELL, CELL), egui::Sense::click());

        if let Some(handle) = self
            .active_video_for(&path)
            .and_then(|active| active.texture.as_ref())
            .cloned()
            .or_else(|| self.thumbs.get(&key).cloned())
        {
            let tex = handle.size_vec2();
            let scale = ((CELL - 8.0) / tex.x).min((CELL - 8.0) / tex.y);
            let draw = tex * scale;
            let image_rect = egui::Rect::from_center_size(rect.center(), draw);
            egui::Image::new(&handle).paint_at(ui, image_rect);
        } else {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        }

        let stroke = if is_current {
            egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(240, 200, 0))
        } else if is_queued {
            egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(220, 60, 60))
        } else {
            egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color)
        };
        ui.painter()
            .rect_stroke(rect, 4.0, stroke, egui::StrokeKind::Inside);

        if is_queued {
            ui.painter().text(
                rect.left_top() + egui::vec2(6.0, 6.0),
                egui::Align2::LEFT_TOP,
                "DEL",
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(220, 60, 60),
            );
        }

        if should_show_media_type_badge(
            self.media_type_badges_visible,
            self.state.media_mode,
            &media_kind,
            self.active_video_is_playing_for(&path),
        ) {
            draw_media_type_badge(ui, rect, &media_kind);
        }

        if response.clicked() {
            self.set_current_index(index);
        }
        if response.double_clicked() {
            self.set_current_index(index);
            self.state.mode = ViewMode::Preview;
        }
    }

    fn draw_overlays(&mut self, ctx: &egui::Context) {
        if self.state.show_help_overlay {
            egui::Window::new("Help")
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(HELP_TEXT);
                });
        }
        if self.state.show_info_overlay
            && !self.browser_selected_is_unsupported()
            && self.right_pane_error.is_none()
            && let Some(entry) = self.state.current_entry()
        {
            let dims = entry
                .dimensions
                .map(|(w, h)| format!("{w}×{h}"))
                .unwrap_or_else(|| "—".to_owned());
            let kind = entry.media_kind.as_str().to_owned();
            let info = format!(
                "{}\npath: {}\nsize: {} bytes\ndimensions: {}\ntype: {}",
                entry.display_name,
                entry.path.display(),
                entry.file_len,
                dims,
                kind,
            );
            egui::Window::new("Info")
                .collapsible(false)
                .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
                .show(ctx, |ui| {
                    ui.label(info);
                });
        }
        if self.state.confirm_delete {
            egui::Window::new("Confirm delete")
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Delete {} queued file(s)?  [y] yes   [n] no",
                        self.state.queue_count()
                    ));
                });
        }
    }
}

const HELP_TEXT: &str = "\
grid (library) mode:
  h / l           left / right one file
  j / k           down / up one row
  ctrl+d / ctrl+u half page down / up
  enter           open highlighted
preview mode:
  h / k / ← / ↑   previous
  l / j / → / ↓   next
  ctrl+alt+h/j/k/l or arrows pan actual-size image
  mouse drag      pan actual-size image
browser mode:
  e               toggle browser
  ctrl+h / ctrl+l focus browser / preview pane
  j / k           browser down / up
  ctrl+d / ctrl+u browser half page down / up
  l / h           enter folder / parent folder
  enter           open folder / focus preview
  .               show / hide hidden files
  t / n           browser time / name sort
home / end      first / last
g               toggle grid
space           play / pause videos
u / o           rewind / fast-forward active video 10%
y               show video progress
d               toggle delete queue, then select next media file
u               unqueue current when no video is active
shift+D         show delete queue
ctrl+R          delete queued (confirm)
z               cycle actual / actual max window / fit
f               toggle fullscreen window
m               mute / unmute video audio
a               toggle auto-next video advance
p               toggle sticky video repeat
b               show / hide media badges
t / n           cycle right-pane time / name sort
r               toggle recursive scan
shift+R         rescan directory
i               info overlay
?               this help
q / esc         quit / close";

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_impl(ctx);
    }
}

impl GuiApp {
    /// The whole per-frame flow, separated from `eframe::App` so tests can
    /// drive full frames through `egui::Context::run`.
    fn update_impl(&mut self, ctx: &egui::Context) {
        let (focused, max_texture_side) =
            ctx.input(|input| (input.focused, input.max_texture_side));
        self.max_texture_side = u32::try_from(max_texture_side).unwrap_or(u32::MAX);
        self.handle_focus_change(focused);
        self.start_pending_initial_video_autoplay(focused);
        self.resume_read();
        self.flush_deferred_live_refresh();
        self.sync_watcher_paths();
        self.drain_watcher();
        self.drain_refresh_results();
        self.handle_input(ctx);
        self.sync_work_suspension_state();
        self.flush_deferred_live_refresh();
        self.sync_watcher_paths();

        // Apply a browser selection only once it has rested briefly, so
        // holding j/k doesn't scan every directory the selection crosses.
        let now = ctx.input(|input| input.time);
        if std::mem::take(&mut self.pending_browser_apply) {
            self.browser_apply_deadline = Some(now + BROWSER_APPLY_DEBOUNCE_SECONDS);
        }
        if let Some(deadline) = self.browser_apply_deadline
            && focused
        {
            if now >= deadline {
                self.browser_apply_deadline = None;
                self.apply_browser_selection();
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(deadline - now));
            }
        }

        self.drain_results(ctx);
        self.drain_video_events(ctx);
        self.ensure_requested();

        let right_mode = match self.state.mode {
            ViewMode::Preview => "preview",
            ViewMode::Grid => "grid",
            ViewMode::DeleteQueueGrid => "delete-queue",
        };
        let mode = if let Some(browser) = &self.state.browser {
            let focus = match browser.focus {
                BrowserPaneFocus::Browser => "browser",
                BrowserPaneFocus::Preview => "preview",
            };
            format!("browser/{right_mode}/{focus}")
        } else {
            right_mode.to_owned()
        };
        let zoom = match self.state.zoom_mode {
            ZoomMode::Fit => "fit",
            ZoomMode::OriginalPixels => "original",
            ZoomMode::OriginalPixelsMaxWindow => "original/max-window",
        };
        let position = if self.right_pane_error.is_some() {
            "error".to_owned()
        } else if self.browser_selected_is_unsupported() {
            "unsupported".to_owned()
        } else if self.state.entries.is_empty() {
            "0 / 0".to_owned()
        } else {
            format!(
                "{} / {}",
                self.state.current_index + 1,
                self.state.entries.len()
            )
        };
        let status = format!(
            "{mode}  |  {position}  |  {zoom}  |  queued: {}  |  sort: {:?}  |  audio: {}  |  repeat: {}  |  auto-next: {}  |  {}",
            self.state.queue_count(),
            self.state.sort_mode,
            if self.video_muted { "muted" } else { "on" },
            if self.repeat_video { "on" } else { "off" },
            if self.auto_next { "on" } else { "off" },
            if self.pending_read.is_some() {
                "loading…"
            } else {
                &self.status
            },
        );

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(APP_IDENTITY);
                ui.separator();
                ui.label(status);
            });
        });
        let mut browser_panel_rect = None;
        if self.state.browser.is_some() {
            let response = egui::SidePanel::left("file-browser")
                .resizable(true)
                .default_width(BROWSER_DEFAULT_WIDTH)
                .width_range(220.0..=520.0)
                .show(ctx, |ui| self.draw_browser_pane(ui));
            browser_panel_rect = Some(response.response.rect);
        }
        let central_rect = egui::CentralPanel::default()
            .show(ctx, |ui| {
                ui.add_enabled_ui(!self.media_actions_blocked(), |ui| {
                    if self.state.browser.is_some() {
                        self.draw_browser_right_pane(ui);
                    } else {
                        match self.state.mode {
                            ViewMode::Preview => self.draw_preview(ui),
                            ViewMode::Grid | ViewMode::DeleteQueueGrid => self.draw_grid(ui),
                        }
                    }
                });
            })
            .response
            .rect;
        // Clicking a pane also claims keyboard focus for it.
        if self.state.browser.is_some() {
            let press_origin = ctx.input(|input| {
                input
                    .pointer
                    .any_pressed()
                    .then(|| input.pointer.press_origin())
                    .flatten()
            });
            if let Some(pos) = press_origin
                && let Some(browser) = self.state.browser.as_mut()
            {
                if browser_panel_rect.is_some_and(|rect| rect.contains(pos)) {
                    browser.focus = BrowserPaneFocus::Browser;
                } else if central_rect.contains(pos) {
                    browser.focus = BrowserPaneFocus::Preview;
                }
            }
        }
        self.draw_overlays(ctx);

        self.scroll_to_current = false;
        let now = ctx.input(|input| input.time);
        let progress_overlay_visible =
            video_progress_overlay_target_visible(now, self.video_progress_overlay_visible_until);
        if progress_overlay_visible && focused {
            let remaining = (self.video_progress_overlay_visible_until - now).max(0.0);
            ctx.request_repaint_after(Duration::from_secs_f64(remaining));
        }
        if self.decode_retry_pending && !self.preview_work_is_suspended() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn should_show_media_type_badge(
    badges_visible: bool,
    media_mode: MediaMode,
    media_kind: &MediaKind,
    active_video_playing: bool,
) -> bool {
    badges_visible
        && media_mode == MediaMode::Both
        && !(media_kind.is_video() && active_video_playing)
}

fn should_apply_video_frame(paused: bool, has_texture: bool) -> bool {
    !paused || !has_texture
}

fn draw_media_type_badge(ui: &egui::Ui, cell_rect: egui::Rect, media_kind: &MediaKind) {
    let badge_size = 18.0;
    let margin = 6.0;
    let badge_rect = egui::Rect::from_min_size(
        egui::pos2(
            cell_rect.right() - margin - badge_size,
            cell_rect.top() + margin,
        ),
        egui::vec2(badge_size, badge_size),
    );
    let painter = ui.painter();
    painter.rect_filled(badge_rect, 4.0, egui::Color32::from_black_alpha(150));
    painter.rect_stroke(
        badge_rect,
        4.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(170)),
        egui::StrokeKind::Inside,
    );

    match media_kind {
        MediaKind::Image(_) => {
            let icon = badge_rect.shrink(4.5);
            let stroke = egui::Stroke::new(1.25_f32, egui::Color32::WHITE);
            painter.rect_stroke(icon, 1.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(icon.left() + 2.0, icon.bottom() - 2.5),
                    egui::pos2(icon.center().x - 1.0, icon.center().y + 1.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(icon.center().x - 1.0, icon.center().y + 1.0),
                    egui::pos2(icon.right() - 2.0, icon.bottom() - 2.5),
                ],
                stroke,
            );
        }
        MediaKind::Video(_) => {
            let center = badge_rect.center();
            let points = vec![
                egui::pos2(center.x - 3.5, center.y - 5.0),
                egui::pos2(center.x - 3.5, center.y + 5.0),
                egui::pos2(center.x + 5.0, center.y),
            ];
            painter.add(egui::Shape::convex_polygon(
                points,
                egui::Color32::WHITE,
                egui::Stroke::NONE,
            ));
        }
    }
}

/// A 2px strip that reads as an accent underline when `focused`, and blends
/// with the widget outlines otherwise, so the layout never shifts.
fn draw_pane_focus_line(ui: &mut egui::Ui, focused: bool) {
    let color = if focused {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 2.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 1.0, color);
    ui.add_space(2.0);
}

fn draw_empty_media_message(ui: &mut egui::Ui, target: &Path) {
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, egui::Sense::hover());

    let painter = ui.painter();
    let center_x = rect.center().x;
    let max_width = (rect.width() * 0.82).clamp(260.0, 900.0);
    let strong = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    let normal = ui.visuals().text_color();

    let title = centered_text_galley(
        painter,
        "No media found".to_owned(),
        egui::FontId::proportional(24.0),
        strong,
        max_width,
    );
    let path = centered_text_galley(
        painter,
        target.display().to_string(),
        egui::FontId::monospace(13.0),
        weak,
        max_width,
    );
    let prompt = EmptyPromptRow::new(painter, normal, strong);

    let title_gap = 8.0;
    let prompt_gap = 12.0;
    let total_height = title.size().y + title_gap + path.size().y + prompt_gap + prompt.height;
    let mut y = rect.center().y - total_height * 0.5;

    paint_centered_galley(painter, center_x, &mut y, title, title_gap, strong);
    paint_centered_galley(painter, center_x, &mut y, path, prompt_gap, weak);
    prompt.paint(ui, center_x, y);
}

fn empty_media_status(target: &Path) -> String {
    format!(
        "No media found at {}; {}",
        target.display(),
        EMPTY_MEDIA_PROMPT
    )
}

fn initial_view_mode(initial_file: Option<&Path>) -> ViewMode {
    if initial_file.is_some() {
        ViewMode::Preview
    } else {
        ViewMode::Grid
    }
}

fn browser_state_for_launch(
    launch: &LaunchTarget,
    include_hidden: bool,
    extensions: &[String],
) -> Result<Option<BrowserState>> {
    let LaunchTarget::Directory {
        directory,
        initial_file,
    } = launch
    else {
        return Ok(None);
    };

    let target = initial_file.as_ref().unwrap_or(directory);
    let listed_directory = listed_directory_for_target(target);
    BrowserState::for_directory(
        listed_directory,
        Some(target.clone()),
        HashMap::new(),
        include_hidden,
        extensions,
    )
    .map(Some)
}

fn centered_text_galley(
    painter: &egui::Painter,
    text: String,
    font_id: egui::FontId,
    color: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text, font_id, color, max_width);
    job.halign = egui::Align::Center;
    painter.layout_job(job)
}

fn paint_centered_galley(
    painter: &egui::Painter,
    center_x: f32,
    y: &mut f32,
    galley: std::sync::Arc<egui::Galley>,
    gap_after: f32,
    fallback_color: egui::Color32,
) {
    let height = galley.size().y;
    painter.galley(egui::pos2(center_x, *y), galley, fallback_color);
    *y += height + gap_after;
}

struct EmptyPromptRow {
    press: std::sync::Arc<egui::Galley>,
    recursive: std::sync::Arc<egui::Galley>,
    quit: std::sync::Arc<egui::Galley>,
    r: std::sync::Arc<egui::Galley>,
    q: std::sync::Arc<egui::Galley>,
    key_size: egui::Vec2,
    height: f32,
    width: f32,
}

impl EmptyPromptRow {
    fn new(painter: &egui::Painter, text_color: egui::Color32, key_color: egui::Color32) -> Self {
        let text_font = egui::FontId::proportional(15.0);
        let key_font = egui::FontId::monospace(15.0);
        let press = painter.layout_no_wrap("Press".to_owned(), text_font.clone(), text_color);
        let recursive =
            painter.layout_no_wrap("for recursive search or".to_owned(), text_font, text_color);
        let quit = painter.layout_no_wrap(
            "to quit.".to_owned(),
            egui::FontId::proportional(15.0),
            text_color,
        );
        let r = painter.layout_no_wrap("r".to_owned(), key_font.clone(), key_color);
        let q = painter.layout_no_wrap("q".to_owned(), key_font, key_color);
        let key_padding = egui::vec2(7.0, 3.0);
        let key_size = egui::vec2(
            r.size().x.max(q.size().x) + key_padding.x * 2.0,
            r.size().y.max(q.size().y) + key_padding.y * 2.0,
        );
        let gap = 6.0;
        let width = press.size().x
            + gap
            + key_size.x
            + gap
            + recursive.size().x
            + gap
            + key_size.x
            + gap
            + quit.size().x;
        let height = press
            .size()
            .y
            .max(recursive.size().y)
            .max(quit.size().y)
            .max(key_size.y);

        Self {
            press,
            recursive,
            quit,
            r,
            q,
            key_size,
            height,
            width,
        }
    }

    fn paint(&self, ui: &egui::Ui, center_x: f32, y: f32) {
        let painter = ui.painter();
        let mut x = center_x - self.width * 0.5;
        let gap = 6.0;
        let text_color = ui.visuals().text_color();

        paint_row_galley(
            painter,
            &mut x,
            y,
            self.height,
            self.press.clone(),
            gap,
            text_color,
        );
        self.paint_key(ui, &mut x, y, self.r.clone(), gap);
        paint_row_galley(
            painter,
            &mut x,
            y,
            self.height,
            self.recursive.clone(),
            gap,
            text_color,
        );
        self.paint_key(ui, &mut x, y, self.q.clone(), gap);
        paint_row_galley(
            painter,
            &mut x,
            y,
            self.height,
            self.quit.clone(),
            0.0,
            text_color,
        );
    }

    fn paint_key(
        &self,
        ui: &egui::Ui,
        x: &mut f32,
        y: f32,
        key: std::sync::Arc<egui::Galley>,
        gap_after: f32,
    ) {
        let painter = ui.painter();
        let key_top = y + (self.height - self.key_size.y) * 0.5;
        let rect = egui::Rect::from_min_size(egui::pos2(*x, key_top), self.key_size);
        painter.rect_filled(rect, 4.0, ui.visuals().widgets.inactive.bg_fill);
        painter.rect_stroke(
            rect,
            4.0,
            ui.visuals().widgets.inactive.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let key_pos = rect.center() - key.size() * 0.5;
        painter.galley(key_pos, key, ui.visuals().strong_text_color());
        *x += self.key_size.x + gap_after;
    }
}

fn paint_row_galley(
    painter: &egui::Painter,
    x: &mut f32,
    y: f32,
    row_height: f32,
    galley: std::sync::Arc<egui::Galley>,
    gap_after: f32,
    fallback_color: egui::Color32,
) {
    let pos = egui::pos2(*x, y + (row_height - galley.size().y) * 0.5);
    let width = galley.size().x;
    painter.galley(pos, galley, fallback_color);
    *x += width + gap_after;
}

fn should_autoplay_initial_video(state: &AppState, initial_file: Option<&Path>) -> bool {
    let Some(initial_file) = initial_file else {
        return false;
    };

    state
        .current_entry()
        .is_some_and(|entry| entry.path.as_path() == initial_file && entry.media_kind.is_video())
}

fn take_pending_initial_video_autoplay(
    focused: bool,
    pending: &mut Option<PathBuf>,
    state: &AppState,
) -> Option<PathBuf> {
    if !focused {
        return None;
    }
    let path = pending.take()?;
    should_autoplay_initial_video(state, Some(&path)).then_some(path)
}

fn selection_autoplay_after_playback_start() -> bool {
    true
}

fn selection_autoplay_after_pause_state(paused: bool) -> bool {
    !paused
}

fn should_autoplay_selected_video(armed: bool, entry: Option<&MediaEntry>) -> bool {
    armed && entry.is_some_and(|entry| entry.media_kind.is_video())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoSeekDirection {
    Backward,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UKeyAction {
    SeekBackward,
    Unqueue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YKeyAction {
    ConfirmDelete,
    ShowProgress,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct VideoProgress {
    position: Duration,
    duration: Duration,
    fraction: f32,
}

fn contextual_u_key_action(active_current_video_can_seek: bool) -> UKeyAction {
    if active_current_video_can_seek {
        UKeyAction::SeekBackward
    } else {
        UKeyAction::Unqueue
    }
}

fn y_key_action(confirm_delete: bool) -> YKeyAction {
    if confirm_delete {
        YKeyAction::ConfirmDelete
    } else {
        YKeyAction::ShowProgress
    }
}

fn video_seek_step(duration: Duration) -> Option<Duration> {
    (!duration.is_zero()).then(|| Duration::from_secs_f64(duration.as_secs_f64() * 0.10))
}

fn video_seek_target(
    position: Duration,
    duration: Option<Duration>,
    direction: VideoSeekDirection,
) -> Option<Duration> {
    let duration = duration?;
    let step = video_seek_step(duration)?;
    let target = match direction {
        VideoSeekDirection::Backward => position.saturating_sub(step),
        VideoSeekDirection::Forward => position.checked_add(step).unwrap_or(duration),
    };
    Some(target.min(duration))
}

fn selection_autoplay_after_seek(was_paused: bool) -> bool {
    !was_paused
}

fn video_progress(position: Duration, duration: Option<Duration>) -> Option<VideoProgress> {
    let duration = duration.filter(|duration| !duration.is_zero())?;
    let fraction = (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0) as f32;
    Some(VideoProgress {
        position: position.min(duration),
        duration,
        fraction,
    })
}

fn video_progress_overlay_deadline(now: f64) -> f64 {
    now + VIDEO_PROGRESS_OVERLAY_SECONDS
}

fn video_progress_overlay_deadline_for_trigger(now: f64, can_show: bool) -> Option<f64> {
    can_show.then(|| video_progress_overlay_deadline(now))
}

fn video_progress_overlay_target_visible(now: f64, visible_until: f64) -> bool {
    now < visible_until
}

fn scaled_alpha(alpha: f32, max_alpha: u8) -> u8 {
    (alpha.clamp(0.0, 1.0) * f32::from(max_alpha)).round() as u8
}

fn format_video_timestamp(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn next_video_index_after(entries: &[MediaEntry], current_index: usize) -> Option<usize> {
    let start = current_index.checked_add(1)?;
    entries
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, entry)| entry.media_kind.is_video().then_some(index))
}

fn playback_terminal_status(ended: bool, error: Option<&str>) -> Option<String> {
    if let Some(error) = error {
        Some(format!("video failed: {error}"))
    } else if ended {
        Some("video ended".to_owned())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackEndAction {
    None,
    RepeatCurrent,
    Advance,
}

fn playback_end_action(
    ended: bool,
    failed: bool,
    repeat_video: bool,
    auto_next: bool,
) -> PlaybackEndAction {
    if !ended || failed {
        PlaybackEndAction::None
    } else if repeat_video {
        PlaybackEndAction::RepeatCurrent
    } else if auto_next {
        PlaybackEndAction::Advance
    } else {
        PlaybackEndAction::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FocusPlaybackEffect {
    pause_video: bool,
    selection_autoplay_armed: bool,
}

fn focus_playback_effect(
    _was_focused: bool,
    is_focused: bool,
    active_video_playing: bool,
    selection_autoplay_armed: bool,
) -> FocusPlaybackEffect {
    let pause_video = !is_focused && active_video_playing;
    FocusPlaybackEffect {
        pause_video,
        selection_autoplay_armed: if pause_video {
            false
        } else {
            selection_autoplay_armed
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ImageKind, MediaEntry, SortMode, VideoKind};
    use std::{ffi::OsString, fs};
    use tempfile::tempdir;

    #[test]
    fn original_pixels_max_window_never_upscales_or_overflows() {
        let container = egui::vec2(1_000.0, 800.0);

        assert_eq!(
            preview_scale(
                ZoomMode::OriginalPixelsMaxWindow,
                container,
                egui::vec2(500.0, 400.0),
            ),
            1.0
        );
        assert_eq!(
            preview_scale(
                ZoomMode::OriginalPixelsMaxWindow,
                container,
                egui::vec2(2_000.0, 1_600.0),
            ),
            0.5
        );
    }

    #[test]
    fn fit_can_upscale_while_original_pixels_stays_at_one_to_one() {
        let container = egui::vec2(1_000.0, 800.0);
        let texture = egui::vec2(500.0, 400.0);

        assert_eq!(preview_scale(ZoomMode::Fit, container, texture), 2.0);
        assert_eq!(
            preview_scale(ZoomMode::OriginalPixels, container, texture),
            1.0
        );
    }

    fn pan_test_app(ctx: &egui::Context, size: [usize; 2]) -> GuiApp {
        let entries = ["first.jpg", "second.jpg"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                media_entry(
                    PathBuf::from(name),
                    MediaKind::Image(ImageKind::Jpeg),
                    index,
                )
            })
            .collect();
        let mut state = app_state(entries, 0);
        state.mode = ViewMode::Preview;
        state.zoom_mode = ZoomMode::OriginalPixels;
        let mut app = GuiApp::new(state, None, true, false, PathBuf::new());
        let texture = ctx.load_texture(
            "pan-test",
            egui::ColorImage::new(size, egui::Color32::WHITE),
            egui::TextureOptions::default(),
        );
        for entry in &app.state.entries {
            for variant in [Variant::Original, Variant::Fit] {
                app.previews
                    .put(TexKey::for_entry(entry, variant, 0), texture.clone());
            }
        }
        app
    }

    fn pan_frame(
        app: &mut GuiApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        modifiers: egui::Modifiers,
        size: egui::Vec2,
    ) {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                modifiers,
                events,
                ..Default::default()
            },
            |ctx| {
                app.handle_input(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    if app.state.mode == ViewMode::Preview {
                        app.draw_preview(ui);
                    }
                });
            },
        );
    }

    #[test]
    fn preview_pan_chords_preserve_selection_and_panel_focus() {
        let ctx = egui::Context::default();
        let mut app = pan_test_app(&ctx, [1000, 800]);
        app.state.browser = Some(BrowserState {
            listed_directory: PathBuf::new(),
            entries: Vec::new(),
            selected_index: 0,
            focus: BrowserPaneFocus::Preview,
            sort_mode: SortMode::Discovered,
            remembered_selection: HashMap::new(),
            scroll_to_selection: false,
        });
        let size = egui::vec2(600.0, 400.0);
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::ALT;
        pan_frame(&mut app, &ctx, vec![], modifiers, size);
        for (key, expected) in [
            (egui::Key::K, egui::vec2(0.0, 80.0)),
            (egui::Key::H, egui::vec2(80.0, 80.0)),
            (egui::Key::J, egui::vec2(80.0, 0.0)),
            (egui::Key::L, egui::Vec2::ZERO),
            (egui::Key::ArrowUp, egui::vec2(0.0, 80.0)),
            (egui::Key::ArrowLeft, egui::vec2(80.0, 80.0)),
            (egui::Key::ArrowDown, egui::vec2(80.0, 0.0)),
            (egui::Key::ArrowRight, egui::Vec2::ZERO),
        ] {
            pan_frame(&mut app, &ctx, vec![key_press(key)], modifiers, size);
            assert_eq!(app.preview_pan.offset, expected);
            assert_eq!(app.state.current_index, 0);
            assert!(!app.browser_is_focused());
        }
        pan_frame(
            &mut app,
            &ctx,
            vec![key_press(egui::Key::H)],
            egui::Modifiers::CTRL,
            size,
        );
        assert!(app.browser_is_focused());
        pan_frame(
            &mut app,
            &ctx,
            vec![key_press(egui::Key::K)],
            modifiers,
            size,
        );
        assert_eq!(app.preview_pan.offset, egui::Vec2::ZERO);
        pan_frame(
            &mut app,
            &ctx,
            vec![key_press(egui::Key::L)],
            egui::Modifiers::CTRL,
            size,
        );
        assert!(!app.browser_is_focused());
    }

    #[test]
    fn preview_pan_drag_tracks_motion_stops_on_release_and_clamps_to_edges() {
        let ctx = egui::Context::default();
        let mut app = pan_test_app(&ctx, [1000, 800]);
        let size = egui::vec2(600.0, 400.0);
        let modifiers = egui::Modifiers::NONE;
        let origin = egui::pos2(300.0, 200.0);
        pan_frame(&mut app, &ctx, vec![], modifiers, size);
        pan_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::PointerMoved(origin),
                egui::Event::PointerButton {
                    pos: origin,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers,
                },
            ],
            modifiers,
            size,
        );
        for delta in [egui::vec2(30.0, 20.0), egui::vec2(50.0, 40.0)] {
            pan_frame(
                &mut app,
                &ctx,
                vec![egui::Event::PointerMoved(origin + delta)],
                modifiers,
                size,
            );
            assert_eq!(app.preview_pan.offset, delta);
        }
        let edge = egui::pos2(900.0, 900.0);
        pan_frame(
            &mut app,
            &ctx,
            vec![egui::Event::PointerMoved(edge)],
            modifiers,
            size,
        );
        assert_eq!(app.preview_pan.offset, app.preview_pan.limit);
        pan_frame(
            &mut app,
            &ctx,
            vec![egui::Event::PointerButton {
                pos: edge,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers,
            }],
            modifiers,
            size,
        );
        pan_frame(
            &mut app,
            &ctx,
            vec![egui::Event::PointerMoved(origin)],
            modifiers,
            size,
        );
        assert_eq!(app.preview_pan.offset, app.preview_pan.limit);
        // A taller viewport locks the vertical axis, leaving horizontal pan available.
        pan_frame(&mut app, &ctx, vec![], modifiers, egui::vec2(600.0, 1000.0));
        assert_eq!(app.preview_pan.offset.y, 0.0);
        assert!(app.preview_pan.offset.x > 0.0);
        pan_frame(
            &mut app,
            &ctx,
            vec![],
            modifiers,
            egui::vec2(1200.0, 1000.0),
        );
        assert_eq!(app.preview_pan.offset, egui::Vec2::ZERO);
        assert_eq!(app.preview_pan.limit, egui::Vec2::ZERO);
    }

    #[test]
    fn preview_pan_resets_for_zoom_selection_and_grid_changes() {
        let ctx = egui::Context::default();
        let mut app = pan_test_app(&ctx, [1000, 800]);
        let size = egui::vec2(600.0, 400.0);
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::ALT;
        pan_frame(&mut app, &ctx, vec![], modifiers, size);
        for key in [egui::Key::Z, egui::Key::L, egui::Key::G] {
            pan_frame(
                &mut app,
                &ctx,
                vec![key_press(egui::Key::K)],
                modifiers,
                size,
            );
            assert!(app.preview_pan.offset.y > 0.0);
            pan_frame(
                &mut app,
                &ctx,
                vec![key_press(key)],
                egui::Modifiers::NONE,
                size,
            );
            pan_frame(&mut app, &ctx, vec![], egui::Modifiers::NONE, size);
            assert_eq!(app.preview_pan.offset, egui::Vec2::ZERO);
            app.state.mode = ViewMode::Preview;
            app.state.zoom_mode = ZoomMode::OriginalPixels;
            pan_frame(&mut app, &ctx, vec![], modifiers, size);
        }
        for zoom in [ZoomMode::Fit, ZoomMode::OriginalPixelsMaxWindow] {
            app.state.zoom_mode = zoom;
            pan_frame(
                &mut app,
                &ctx,
                vec![key_press(egui::Key::K)],
                modifiers,
                size,
            );
            assert_eq!(app.preview_pan.offset, egui::Vec2::ZERO);
        }
    }

    #[test]
    fn background_repaint_requests_respect_the_pause_gate() {
        let enabled = AtomicBool::new(false);
        let calls = std::cell::Cell::new(0);

        request_background_repaint(&enabled, || calls.set(calls.get() + 1));
        assert_eq!(calls.get(), 0);

        enabled.store(true, Ordering::Relaxed);
        request_background_repaint(&enabled, || calls.set(calls.get() + 1));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn media_type_badge_visibility_matches_mode_and_playback() {
        let image = MediaKind::Image(ImageKind::Jpeg);
        let video = MediaKind::Video(VideoKind::Mp4);

        assert!(should_show_media_type_badge(
            true,
            MediaMode::Both,
            &image,
            false
        ));
        assert!(should_show_media_type_badge(
            true,
            MediaMode::Both,
            &video,
            false
        ));
        assert!(!should_show_media_type_badge(
            true,
            MediaMode::Both,
            &video,
            true
        ));
        assert!(!should_show_media_type_badge(
            false,
            MediaMode::Both,
            &image,
            false
        ));
        assert!(!should_show_media_type_badge(
            true,
            MediaMode::Image,
            &image,
            false
        ));
        assert!(!should_show_media_type_badge(
            true,
            MediaMode::Video,
            &video,
            false
        ));
    }

    #[test]
    fn d_queues_current_file_and_selects_the_next_media_file() {
        let first_image_path = PathBuf::from("/tmp/media/first.jpg");
        let video_path = PathBuf::from("/tmp/media/between.mp4");
        let next_image_path = PathBuf::from("/tmp/media/next.jpg");
        let entries = vec![
            media_entry(
                first_image_path.clone(),
                MediaKind::Image(ImageKind::Jpeg),
                0,
            ),
            media_entry(video_path.clone(), MediaKind::Video(VideoKind::Mp4), 1),
            media_entry(
                next_image_path.clone(),
                MediaKind::Image(ImageKind::Jpeg),
                2,
            ),
        ];
        let state = app_state(entries, 0);
        let mut app = GuiApp::new(state, None, true, false, PathBuf::from("/tmp/media"));

        app.toggle_queue_current_and_select_next_media();

        assert!(app.state.delete_queue.contains(&first_image_path));
        assert_eq!(app.state.current_path().as_ref(), Some(&video_path));

        app.toggle_queue_current_and_select_next_media();

        assert!(app.state.delete_queue.contains(&video_path));
        assert_eq!(app.state.current_path().as_ref(), Some(&next_image_path));

        app.toggle_queue_current_and_select_next_media();

        assert!(app.state.delete_queue.contains(&next_image_path));
        assert_eq!(app.state.current_path().as_ref(), Some(&next_image_path));
    }

    #[test]
    fn paused_video_drops_queued_frames_after_its_preview_exists() {
        assert!(!should_apply_video_frame(true, true));
        assert!(should_apply_video_frame(true, false));
        assert!(should_apply_video_frame(false, true));
    }

    #[test]
    fn live_arrivals_are_only_decoded_when_they_become_current() {
        let path = PathBuf::from("newly-arrived.png");
        let deferred = HashSet::from([path.clone()]);

        assert!(!preview_decode_allowed(1, 0, &path, &deferred));
        assert!(preview_decode_allowed(1, 1, &path, &deferred));
        assert!(preview_decode_allowed(
            1,
            0,
            Path::new("already-known.png"),
            &deferred,
        ));
    }

    #[test]
    fn texture_cache_evicts_by_resident_bytes() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::new(8, 16);
        let key = |name: &str| TexKey {
            path: PathBuf::from(name),
            variant: Variant::Thumb,
            media_kind: MediaKind::Image(ImageKind::Png),
            file_len: 0,
            modified: None,
            generation: 0,
        };
        let texture = |name: &str| {
            ctx.load_texture(
                name,
                egui::ColorImage::new([2, 2], egui::Color32::WHITE),
                egui::TextureOptions::LINEAR,
            )
        };

        cache.put(key("first.png"), texture("first"));
        cache.put(key("second.png"), texture("second"));

        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.resident_bytes, 16);
        assert!(!cache.contains(&key("first.png")));
        assert!(cache.contains(&key("second.png")));
    }

    #[test]
    fn texture_cache_accounts_for_entry_capacity_evictions() {
        let ctx = egui::Context::default();
        let mut cache = TextureCache::new(1, 1024);
        let key = |name: &str| TexKey {
            path: PathBuf::from(name),
            variant: Variant::Thumb,
            media_kind: MediaKind::Image(ImageKind::Png),
            file_len: 0,
            modified: None,
            generation: 0,
        };
        let texture = |name: &str| {
            ctx.load_texture(
                name,
                egui::ColorImage::new([2, 2], egui::Color32::WHITE),
                egui::TextureOptions::LINEAR,
            )
        };

        cache.put(key("first.png"), texture("capacity-first"));
        cache.put(key("second.png"), texture("capacity-second"));

        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.resident_bytes, 16);
        assert!(!cache.contains(&key("first.png")));
        assert!(cache.contains(&key("second.png")));
    }

    #[test]
    fn decode_memory_permits_restore_the_reserved_capacity() {
        let budget = DecodeMemoryBudget::new(100);
        let permit = budget.acquire(60).unwrap();
        assert_eq!(*budget.available.lock().unwrap(), 40);

        drop(permit);

        assert_eq!(*budget.available.lock().unwrap(), 100);
    }

    #[test]
    fn decode_memory_budget_rejects_oversized_reservations() {
        let budget = DecodeMemoryBudget::new(100);

        assert!(budget.acquire(101).is_err());
        assert_eq!(*budget.available.lock().unwrap(), 100);
    }

    #[test]
    fn actual_size_decode_fits_the_largest_gpu_texture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.png");
        image::RgbImage::new(300, 200).save(&path).unwrap();
        let decoder = DecodeService::new(Arc::new(AtomicBool::new(false)));
        let key = TexKey {
            path,
            variant: Variant::Original,
            media_kind: MediaKind::Image(ImageKind::Png),
            file_len: 0,
            modified: None,
            generation: 0,
        };

        decoder.request(key, 100).unwrap();
        let result = decoder
            .result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();

        assert_eq!(result.image.unwrap().size, [100, 67]);
    }

    #[test]
    fn decode_memory_wait_stops_when_work_is_cancelled() {
        let budget = DecodeMemoryBudget::new(100);
        let _all_memory = budget.acquire(100).unwrap();

        let permit = budget.acquire_cancellable(1, &|| true).unwrap();

        assert!(permit.is_none());
    }

    #[test]
    fn cancelling_decode_work_rejects_results_from_the_previous_epoch() {
        let decoder = DecodeService::new(Arc::new(AtomicBool::new(false)));

        assert!(decoder.accepts(0));
        decoder.cancel_pending();
        assert!(!decoder.accepts(0));
        assert!(decoder.accepts(1));
    }

    #[test]
    fn empty_media_status_mentions_target_recursive_search_and_quit() {
        let status = empty_media_status(Path::new("/tmp/empty-media"));

        assert!(status.contains("No media found"));
        assert!(status.contains("/tmp/empty-media"));
        assert!(status.contains("`r`"));
        assert!(status.contains("recursive search"));
        assert!(status.contains("`q`"));
    }

    #[test]
    fn initial_view_mode_uses_grid_for_folder_targets() {
        assert_eq!(initial_view_mode(None), ViewMode::Grid);
        assert_eq!(
            initial_view_mode(Some(Path::new("/tmp/media/image.jpg"))),
            ViewMode::Preview
        );
    }

    #[test]
    fn resolve_launch_without_paths_uses_current_directory() {
        let launch = resolve_launch(&[], None).unwrap();
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();

        assert_eq!(
            launch,
            LaunchTarget::Directory {
                directory: cwd,
                initial_file: None,
            }
        );
    }

    #[test]
    fn resolve_launch_keeps_single_directory_as_directory_mode() {
        let temp = tempdir().unwrap();
        let launch = resolve_launch(&[temp.path().to_path_buf()], None).unwrap();

        assert_eq!(
            launch,
            LaunchTarget::Directory {
                directory: temp.path().canonicalize().unwrap(),
                initial_file: None,
            }
        );
    }

    #[test]
    fn resolve_launch_keeps_single_file_as_parent_directory_mode() {
        let temp = tempdir().unwrap();
        let image = temp.path().join("image.jpg");
        touch(&image);
        let launch = resolve_launch(std::slice::from_ref(&image), None).unwrap();
        let canonical_image = image.canonicalize().unwrap();

        assert_eq!(
            launch,
            LaunchTarget::Directory {
                directory: temp.path().canonicalize().unwrap(),
                initial_file: Some(canonical_image),
            }
        );
        assert_eq!(initial_view_mode_for_launch(&launch), ViewMode::Preview);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_launch_rejects_a_direct_file_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let target = temp.path().join("target.jpg");
        let link = temp.path().join("link.jpg");
        touch(&target);
        symlink(&target, &link).unwrap();

        let error = resolve_launch(&[link], None).unwrap_err();

        assert!(error.to_string().contains("refusing to open symlink"));
        assert!(target.exists());
    }

    #[test]
    fn resolve_launch_multiple_files_become_selected_file_scope_and_dedupe() {
        let temp = tempdir().unwrap();
        let image = temp.path().join("image.jpg");
        let video = temp.path().join("clip.mp4");
        touch(&image);
        touch(&video);

        let launch = resolve_launch(&[video.clone(), image.clone(), video.clone()], None).unwrap();
        let canonical_video = video.canonicalize().unwrap();
        let canonical_image = image.canonicalize().unwrap();

        assert_eq!(
            launch,
            LaunchTarget::SelectedFiles {
                directory: temp.path().canonicalize().unwrap(),
                files: vec![canonical_video, canonical_image],
            }
        );
        assert_eq!(initial_view_mode_for_launch(&launch), ViewMode::Grid);
    }

    #[test]
    fn browser_launch_for_folder_lists_parent_and_selects_folder() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let launch = resolve_launch(std::slice::from_ref(&folder), None).unwrap();

        let browser = browser_state_for_launch(&launch, false, &["jpg".to_owned()])
            .unwrap()
            .unwrap();

        assert_eq!(
            browser.listed_directory,
            temp.path().canonicalize().unwrap()
        );
        assert_eq!(
            browser.selected_path(),
            Some(folder.canonicalize().unwrap())
        );
    }

    #[test]
    fn browser_launch_for_file_lists_parent_and_selects_file() {
        let temp = tempdir().unwrap();
        let image = temp.path().join("image.jpg");
        touch(&image);
        let launch = resolve_launch(std::slice::from_ref(&image), None).unwrap();

        let browser = browser_state_for_launch(&launch, false, &["jpg".to_owned()])
            .unwrap()
            .unwrap();

        assert_eq!(
            browser.listed_directory,
            temp.path().canonicalize().unwrap()
        );
        assert_eq!(browser.selected_path(), Some(image.canonicalize().unwrap()));
    }

    #[test]
    fn browser_launch_ignores_selected_file_scope() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first.jpg");
        let second = temp.path().join("second.jpg");
        touch(&first);
        touch(&second);
        let launch = resolve_launch(&[first, second], None).unwrap();

        assert!(
            browser_state_for_launch(&launch, false, &["jpg".to_owned()])
                .unwrap()
                .is_none()
        );
        assert_eq!(initial_view_mode_for_launch(&launch), ViewMode::Grid);
    }

    fn controlled_decode_queue(
        app: &mut GuiApp,
    ) -> (Receiver<DecodeRequest>, Sender<DecodeResult>) {
        let (job_tx, job_rx) = flume::bounded(1);
        let (result_tx, result_rx) = flume::bounded(8);
        app.decoder.job_tx = job_tx;
        app.decoder.result_rx = result_rx;
        (job_rx, result_tx)
    }

    fn repaint_delay(output: &egui::FullOutput) -> Duration {
        output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
    }

    #[test]
    fn gui_repaints_only_for_full_decode_queue_or_available_results() {
        let temp = tempdir().unwrap();
        let entries = ["a.jpg", "b.jpg"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                media_entry(
                    temp.path().join(name),
                    MediaKind::Image(ImageKind::Jpeg),
                    index,
                )
            })
            .collect();
        let mut app = GuiApp::new(
            app_state(entries, 0),
            None,
            true,
            false,
            temp.path().to_path_buf(),
        );
        app.state.zoom_mode = ZoomMode::Fit;
        let (jobs, results) = controlled_decode_queue(&mut app);
        let ctx = egui::Context::default();
        for time in 0..4 {
            frame_output(&mut app, &ctx, true, f64::from(time));
        }
        let output = frame_output(&mut app, &ctx, true, 4.0);
        assert!(app.decode_retry_pending);
        assert_eq!(repaint_delay(&output), Duration::from_millis(50));
        let first = jobs.try_recv().unwrap();
        frame_output(&mut app, &ctx, true, 5.0);
        let second = jobs.try_recv().unwrap();
        assert!(!app.decode_retry_pending);
        let output = frame_output(&mut app, &ctx, true, 6.0);
        assert!(
            repaint_delay(&output) > Duration::from_secs(60),
            "inflight jobs must not poll"
        );
        let mut stale_key = first.key.clone();
        stale_key.generation = stale_key.generation.wrapping_add(1);
        for key in [stale_key, first.key, second.key] {
            results
                .send(DecodeResult {
                    key,
                    work_epoch: app.decoder.work_epoch.load(Ordering::Acquire),
                    image: Ok(egui::ColorImage::new([1, 1], egui::Color32::WHITE)),
                    _memory_permit: None,
                })
                .unwrap();
        }
        let output = frame_output(&mut app, &ctx, true, 7.0);
        assert_eq!(app.previews.entries.len(), 1);
        assert_eq!(output.textures_delta.set.len(), 1);
        assert_eq!(repaint_delay(&output), Duration::ZERO);
        let output = frame_output(&mut app, &ctx, true, 8.0);
        assert_eq!(app.previews.entries.len(), 2);
        assert_eq!(output.textures_delta.set.len(), 1);
        assert!(app.inflight.is_empty());
    }

    #[test]
    fn full_decode_queue_stops_retrying_while_unfocused_and_restarts_on_return() {
        let temp = tempdir().unwrap();
        let entries = ["a.jpg", "b.jpg"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                media_entry(
                    temp.path().join(name),
                    MediaKind::Image(ImageKind::Jpeg),
                    index,
                )
            })
            .collect();
        let mut app = GuiApp::new(
            app_state(entries, 0),
            None,
            true,
            false,
            temp.path().to_path_buf(),
        );
        app.state.zoom_mode = ZoomMode::Fit;
        let (_jobs, _results) = controlled_decode_queue(&mut app);
        let ctx = egui::Context::default();
        frame_output(&mut app, &ctx, true, 0.0);
        assert!(app.decode_retry_pending);
        for time in 1..4 {
            frame_output(&mut app, &ctx, false, f64::from(time));
        }
        assert!(!app.decode_retry_pending);
        let output = frame_output(&mut app, &ctx, false, 4.0);
        assert!(repaint_delay(&output) > Duration::from_secs(60));
        frame_output(&mut app, &ctx, true, 5.0);
        assert!(app.decode_retry_pending);
    }

    #[test]
    fn decode_workers_wake_idle_gui_for_success_and_error_without_mouse_input() {
        for valid in [true, false] {
            let temp = tempdir().unwrap();
            let path = temp.path().join("synthetic.jpg");
            if valid {
                image::RgbImage::new(8, 8).save(&path).unwrap();
            } else {
                touch(&path);
            }
            let mut app = app_for_browser_tests(temp.path());
            app.rescan();
            finish_reads(&mut app);
            app.state.mode = ViewMode::Preview;
            let deferred_entries = if valid {
                Vec::new()
            } else {
                std::mem::take(&mut app.state.entries)
            };
            let ctx = egui::Context::default();
            app.attach_repaint(&ctx);
            // Hold the real worker's memory permit, so a valid decode cannot complete yet.
            let permit = valid.then(|| {
                app.decoder
                    .memory_budget
                    .acquire(app.decoder.memory_budget.capacity)
                    .unwrap()
            });
            for time in 0..4 {
                frame_output(&mut app, &ctx, true, f64::from(time));
            }
            let (wake_tx, wake_rx) = flume::unbounded();
            ctx.set_request_repaint_callback(move |_| {
                let _ = wake_tx.send(());
            });
            if valid {
                let output = frame_output(&mut app, &ctx, true, 4.0);
                assert!(repaint_delay(&output) > Duration::from_secs(60));
                assert!(!app.inflight.is_empty());
                drop(permit);
            } else {
                // No error job is submitted before the callback is installed.
                app.state.entries = deferred_entries;
                app.ensure_requested();
            }
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            frame_output(&mut app, &ctx, true, 5.0);
            if valid {
                assert_eq!(app.previews.entries.len(), 1);
            } else {
                assert_eq!(app.failed.len(), 1);
            }
            frame_output(&mut app, &ctx, false, 6.0);
            while wake_rx.try_recv().is_ok() {}
            (app.video_repaint)();
            assert!(
                wake_rx.try_recv().is_err(),
                "background notification must respect focus gate"
            );
        }
    }

    #[test]
    fn gui_uploads_latest_video_frame_once_and_then_handles_terminal_status() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("synthetic.mp4");
        let entry = media_entry(path.clone(), MediaKind::Video(VideoKind::Mp4), 0);
        let mut app = GuiApp::new(
            app_state(vec![entry], 0),
            None,
            true,
            false,
            temp.path().to_path_buf(),
        );
        let (_jobs, _results) = controlled_decode_queue(&mut app);
        let handle = video::PlaybackHandle::test_handle(path.clone(), false);
        let id = handle.id();
        app.video_events.activate(id);
        app.active_video = Some(ActiveVideo {
            handle,
            texture: None,
            position: Duration::ZERO,
            duration: None,
            ended: false,
        });
        let ctx = egui::Context::default();
        for time in 0..4 {
            frame_output(&mut app, &ctx, true, f64::from(time));
        }
        assert!(repaint_delay(&frame_output(&mut app, &ctx, true, 4.0)) > Duration::from_secs(60));
        for width in 1..=8 {
            app.video_events.publish(video::PlaybackEvent {
                playback_id: id,
                path: path.clone(),
                frame: Some(egui::ColorImage::new([width, 1], egui::Color32::WHITE)),
                position: Some(Duration::from_millis(width as u64)),
                duration: None,
                ended: false,
                error: None,
            });
        }
        app.video_events.publish(video::PlaybackEvent {
            playback_id: id,
            path,
            frame: None,
            position: None,
            duration: None,
            ended: true,
            error: Some("synthetic error".to_owned()),
        });
        let output = frame_output(&mut app, &ctx, true, 5.0);
        assert_eq!(output.textures_delta.set.len(), 1);
        let active = app.active_video.as_ref().unwrap();
        assert_eq!(active.texture.as_ref().unwrap().size(), [8, 1]);
        assert_eq!(active.position, Duration::from_millis(8));
        assert!(active.ended);
        assert!(app.status.contains("synthetic error"));
    }

    fn frame_output(
        app: &mut GuiApp,
        ctx: &egui::Context,
        focused: bool,
        time: f64,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                focused,
                predicted_dt: 0.0,
                time: Some(time),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                ..Default::default()
            },
            |ctx| app.update_impl(ctx),
        )
    }

    fn blocked_directory_read(app: &mut GuiApp, directory: &Path) -> (Receiver<()>, Sender<()>) {
        let (started_tx, started_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded(1);
        let job = ReadJob::Media {
            scope: MediaRefreshScope::Directory(ScanOptions {
                root: directory.to_path_buf(),
                recursive: false,
                include_hidden: false,
                extensions: vec!["jpg".to_owned()],
            }),
            sort_mode: SortMode::NameAsc,
            locale: None,
        };
        app.begin_read(
            ReadJob::Blocked {
                started: started_tx,
                release: release_rx,
                job: Box::new(job),
            },
            ReadEffect::Load(ViewLoad {
                directory: directory.to_path_buf(),
                selected_files: None,
                preferred: None,
                required_file: None,
                mode: ViewMode::Grid,
                target: directory.to_path_buf(),
                browser: None,
            }),
        );
        (started_rx, release_tx)
    }

    #[test]
    fn blocked_read_keeps_ui_responsive_and_old_media_actions_disabled() {
        let temp = tempdir().unwrap();
        let old = temp.path().join("old.jpg");
        touch(&old);
        let folder = temp.path().join("new");
        fs::create_dir(&folder).unwrap();
        touch(&folder.join("new.jpg"));
        let mut app = app_for_browser_tests(temp.path());
        app.rescan();
        finish_reads(&mut app);
        app.state.delete_queue.insert(old.clone());
        let ctx = egui::Context::default();
        app.attach_repaint(&ctx);
        let (started, release) = blocked_directory_read(&mut app, &folder);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        run_frame(
            &mut app,
            &ctx,
            vec![
                key_press(egui::Key::D),
                key_press(egui::Key::Space),
                key_press(egui::Key::O),
            ],
        );
        assert_eq!(app.state.directory, temp.path());
        assert!(app.media_actions_blocked());
        assert_eq!(app.state.current_path(), Some(old.clone()));
        app.confirm_delete();
        assert_eq!(app.state.queue_count(), 1);
        run_frame(&mut app, &ctx, vec![key_press(egui::Key::Questionmark)]);
        assert!(
            app.state.show_help_overlay,
            "UI handles input while worker is blocked"
        );
        app.request_live_media_refresh();
        assert!(app.deferred_live_refresh_pending);
        assert!(!app.media_refresh_inflight);
        release.send(()).unwrap();
        finish_reads(&mut app);
        assert_eq!(app.state.directory, folder);
        assert_eq!(app.state.queue_count(), 0);
        assert!(!app.media_actions_blocked());
        app.flush_deferred_live_refresh();
        assert!(app.media_refresh_inflight);
    }

    #[test]
    fn latest_directory_intent_wins_and_focus_suspends_then_reissues_it() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        touch(&second.join("new.jpg"));
        let mut app = app_for_browser_tests(temp.path());
        let (started, release) = blocked_directory_read(&mut app, &first);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let stale = app.read_generation;
        app.queue_directory(second.clone(), None);
        let before_focus_loss = app.read_generation;
        app.handle_focus_change(false);
        app.handle_read_result(stale, Ok(ReadOutput::Media(Vec::new())));
        app.handle_read_result(before_focus_loss, Ok(ReadOutput::Media(Vec::new())));
        assert_eq!(app.state.directory, temp.path());
        assert!(!app.read_inflight);
        assert!(app.pending_read.is_some());
        release.send(()).unwrap();
        app.handle_focus_change(true);
        finish_reads(&mut app);
        assert_eq!(app.state.directory, second);
        assert_eq!(app.state.entries.len(), 1);
        app.handle_read_result(stale, Err("old failure".to_owned()));
        assert!(app.right_pane_error.is_none());
    }

    #[test]
    fn filter_change_during_navigation_keeps_pending_destination() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        touch(&target.join(".hidden.jpg"));
        let mut app = app_for_browser_tests(temp.path());
        let (started, release) = blocked_directory_read(&mut app, &target);
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        app.toggle_hidden_files();
        app.rescan();
        app.pending_rescan = false;
        release.send(()).unwrap();
        finish_reads(&mut app);
        assert_eq!(app.state.directory, target);
        assert_eq!(app.state.entries.len(), 1);
        assert_eq!(app.state.entries[0].display_name, ".hidden.jpg");
    }

    #[test]
    fn delayed_browser_selection_waits_for_focus_and_cached_file_selection_stays_fast() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join("a.jpg"));
        touch(&temp.path().join("b.jpg"));
        let mut app = app_for_browser_tests(temp.path());
        app.rescan();
        finish_reads(&mut app);
        app.state.browser = Some(
            BrowserState::for_directory(
                temp.path().to_path_buf(),
                Some(temp.path().join("a.jpg")),
                HashMap::new(),
                false,
                &["jpg".to_owned()],
            )
            .unwrap(),
        );
        app.state.mode = ViewMode::Preview;
        app.state.current_index = app
            .state
            .entries
            .iter()
            .position(|e| e.display_name == "a.jpg")
            .unwrap();
        app.select_browser_index(1);
        let ctx = egui::Context::default();
        frame_output(&mut app, &ctx, false, 1.0);
        frame_output(&mut app, &ctx, false, 2.0);
        assert_eq!(app.state.current_path(), Some(temp.path().join("a.jpg")));
        assert!(app.pending_read.is_none());
        assert!(app.browser_apply_deadline.is_some());
        frame_output(&mut app, &ctx, true, 3.0);
        assert_eq!(app.state.current_path(), Some(temp.path().join("b.jpg")));
        assert!(
            app.pending_read.is_none(),
            "already loaded files need no read job"
        );
        assert!(!app.media_actions_blocked());
    }

    #[test]
    fn decode_from_before_focus_loss_cannot_remove_a_new_inflight_request() {
        let temp = tempdir().unwrap();
        let entry = media_entry(
            temp.path().join("a.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        let mut app = GuiApp::new(
            app_state(vec![entry], 0),
            None,
            true,
            false,
            temp.path().to_path_buf(),
        );
        let (jobs, results) = controlled_decode_queue(&mut app);
        let ctx = egui::Context::default();
        frame_output(&mut app, &ctx, true, 0.0);
        let old = jobs.try_recv().unwrap();
        frame_output(&mut app, &ctx, false, 1.0);
        frame_output(&mut app, &ctx, true, 2.0);
        let current = jobs.try_recv().unwrap();
        results
            .send(DecodeResult {
                key: old.key,
                work_epoch: old.work_epoch,
                image: Ok(egui::ColorImage::new([1, 1], egui::Color32::WHITE)),
                _memory_permit: None,
            })
            .unwrap();
        frame_output(&mut app, &ctx, true, 3.0);
        assert!(app.inflight.contains(&current.key));
        assert!(app.previews.entries.is_empty());
        // A result whose file identity is no longer in the view is stale as well.
        app.state.entries[0].file_len += 1;
        results
            .send(DecodeResult {
                key: current.key,
                work_epoch: current.work_epoch,
                image: Ok(egui::ColorImage::new([1, 1], egui::Color32::WHITE)),
                _memory_permit: None,
            })
            .unwrap();
        frame_output(&mut app, &ctx, true, 4.0);
        assert!(app.previews.entries.is_empty());
    }

    #[test]
    fn failed_manual_read_preserves_queue_cache_and_old_view() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("image.jpg");
        touch(&path);
        let mut app = app_for_browser_tests(temp.path());
        app.rescan();
        finish_reads(&mut app);
        app.state.delete_queue.insert(path.clone());
        let generation = app.decode_generation;
        app.queue_directory(temp.path().join("missing"), None);
        finish_reads(&mut app);
        assert_eq!(app.state.directory, temp.path());
        assert_eq!(app.state.current_path(), Some(path));
        assert_eq!(app.state.queue_count(), 1);
        assert_eq!(app.decode_generation, generation);
        assert!(app.right_pane_error.is_some());
        assert!(!app.media_actions_blocked());
    }

    #[test]
    fn async_sort_preserves_latest_selection_and_queue_even_when_membership_is_unchanged() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join("a.jpg"));
        touch(&temp.path().join("b.jpg"));
        let mut app = app_for_browser_tests(temp.path());
        app.rescan();
        finish_reads(&mut app);
        app.state.delete_queue.insert(temp.path().join("a.jpg"));
        app.resort(SortMode::NameDesc);
        let current = app.state.entries[1].path.clone();
        app.set_current_index(1);
        finish_reads(&mut app);
        assert_eq!(app.state.entries[0].display_name, "b.jpg");
        assert_eq!(app.state.current_path(), Some(current));
        assert_eq!(app.state.queue_count(), 1);
    }

    #[test]
    fn metadata_result_cannot_restore_an_old_file_identity_or_selection() {
        let temp = tempdir().unwrap();
        let mut app = app_for_browser_tests(temp.path());
        let original = media_entry(
            temp.path().join("a.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        app.state.entries = vec![original.clone()];
        app.pending_read = Some(PendingRead {
            job: ReadJob::Metadata(original.clone()),
            effect: ReadEffect::Metadata,
        });
        app.state.entries[0].file_len += 1;
        let mut enriched = original.clone();
        enriched.dimensions = Some((100, 100));
        app.handle_read_result(
            app.read_generation,
            Ok(ReadOutput::Metadata(enriched.clone())),
        );
        assert_eq!(app.state.entries[0].dimensions, None);
        app.pending_read = Some(PendingRead {
            job: ReadJob::Metadata(original),
            effect: ReadEffect::Metadata,
        });
        app.state.entries[0].path = temp.path().join("b.jpg");
        app.handle_read_result(app.read_generation, Ok(ReadOutput::Metadata(enriched)));
        assert_eq!(app.state.entries[0].path, temp.path().join("b.jpg"));
        assert_eq!(app.state.entries[0].dimensions, None);
    }

    #[test]
    fn terminal_events_around_focus_loss_do_not_repeat_advance_or_resume() {
        for repeat in [true, false] {
            for publish_before_loss in [true, false] {
                let temp = tempdir().unwrap();
                let path = temp.path().join("synthetic.mp4");
                let mut state = app_state(
                    vec![
                        media_entry(path.clone(), MediaKind::Video(VideoKind::Mp4), 0),
                        media_entry(
                            temp.path().join("next.mp4"),
                            MediaKind::Video(VideoKind::Mp4),
                            1,
                        ),
                    ],
                    0,
                );
                state.mode = ViewMode::Preview;
                let mut app = GuiApp::new(state, None, true, !repeat, temp.path().to_path_buf());
                app.repeat_video = repeat;
                let handle = video::PlaybackHandle::test_handle(path.clone(), false);
                let id = handle.id();
                app.video_events.activate(id);
                app.active_video = Some(ActiveVideo {
                    handle,
                    texture: None,
                    position: Duration::ZERO,
                    duration: None,
                    ended: false,
                });
                let event = video::PlaybackEvent {
                    playback_id: id,
                    path: path.clone(),
                    frame: None,
                    position: Some(Duration::from_secs(1)),
                    duration: None,
                    ended: true,
                    error: None,
                };
                if !publish_before_loss {
                    app.handle_focus_change(false);
                }
                app.video_events.publish(event);
                let ctx = egui::Context::default();
                frame_output(&mut app, &ctx, false, 0.0);
                assert!(app.active_video.as_ref().unwrap().ended);
                assert!(app.active_video.as_ref().unwrap().handle.is_paused());
                frame_output(&mut app, &ctx, true, 1.0);
                assert_eq!(app.active_video.as_ref().unwrap().handle.id(), id);
                assert!(!app.active_video_is_playing());
                assert_eq!(app.state.current_path(), Some(path));
                assert_eq!(app.repeat_video, repeat);
                // Even an already-unfocused UI update enforces the invariant.
                app.window_focused = false;
                app.active_video.as_mut().unwrap().ended = false;
                app.active_video.as_ref().unwrap().handle.set_paused(false);
                frame_output(&mut app, &ctx, false, 2.0);
                assert!(app.active_video.as_ref().unwrap().handle.is_paused());
            }
        }
    }

    fn finish_reads(app: &mut GuiApp) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.pending_read.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "read did not finish: {}",
                app.status
            );
            app.resume_read();
            app.drain_refresh_results();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn run_frame(app: &mut GuiApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            events,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| app.update_impl(ctx));
    }

    fn key_press(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn e_key_opens_browser_after_folder_launch() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        touch(&folder.join("image.jpg"));
        let launch = resolve_launch(std::slice::from_ref(&folder), None).unwrap();
        let entries = scan_launch_entries(&launch, false, false, &["jpg".to_owned()]).unwrap();
        let mut state = AppState::new(
            launch.directory().to_path_buf(),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            entries,
        );
        state.mode = initial_view_mode_for_launch(&launch);
        let mut app = GuiApp::new(state, None, true, false, folder.clone());

        let ctx = egui::Context::default();
        run_frame(&mut app, &ctx, Vec::new());
        run_frame(&mut app, &ctx, vec![key_press(egui::Key::E)]);
        finish_reads(&mut app);

        assert!(
            app.state.browser.is_some(),
            "e should open the browser after a folder launch; status: {}",
            app.status
        );
    }

    #[test]
    fn browser_folder_selection_loads_right_pane_gallery() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let image = folder.join("image.jpg");
        touch(&image);
        let mut app = app_for_browser_tests(temp.path());
        app.state.browser = Some(
            BrowserState::for_directory(
                temp.path().canonicalize().unwrap(),
                Some(folder.canonicalize().unwrap()),
                HashMap::new(),
                false,
                &["jpg".to_owned()],
            )
            .unwrap(),
        );

        app.apply_browser_selection();
        finish_reads(&mut app);

        assert_eq!(app.state.directory, folder.canonicalize().unwrap());
        assert_eq!(app.state.mode, ViewMode::Grid);
        assert_eq!(app.state.entries.len(), 1);
        assert_eq!(app.state.entries[0].path, image.canonicalize().unwrap());
    }

    #[test]
    fn browser_empty_folder_selection_shows_empty_folder_gallery() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let mut app = app_for_browser_tests(temp.path());
        app.open_browser_directory(folder.canonicalize().unwrap(), None);
        finish_reads(&mut app);

        assert_eq!(app.state.directory, folder.canonicalize().unwrap());
        assert_eq!(app.state.mode, ViewMode::Grid);
        assert!(app.state.entries.is_empty());
    }

    #[test]
    fn failed_browser_scan_does_not_commit_partial_right_pane_state() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let original_directory = temp.path().canonicalize().unwrap();
        let mut app = app_for_browser_tests(temp.path());
        app.state.browser = Some(
            BrowserState::for_directory(
                original_directory.clone(),
                Some(folder.canonicalize().unwrap()),
                HashMap::new(),
                false,
                &["jpg".to_owned()],
            )
            .unwrap(),
        );
        fs::remove_dir(&folder).unwrap();

        app.apply_browser_selection();
        finish_reads(&mut app);

        assert_eq!(app.state.directory, original_directory);
        assert!(
            app.right_pane_error.as_deref().is_some_and(|error| {
                error.contains("Failed to scan") && error.contains("folder")
            })
        );
    }

    #[test]
    fn disappeared_browser_file_does_not_replace_the_right_pane() {
        let temp = tempdir().unwrap();
        let old_folder = temp.path().join("old");
        let new_folder = temp.path().join("new");
        fs::create_dir(&old_folder).unwrap();
        fs::create_dir(&new_folder).unwrap();
        let old_image = old_folder.join("old.jpg");
        let selected_image = new_folder.join("selected.jpg");
        touch(&old_image);
        touch(&selected_image);

        let old_folder = old_folder.canonicalize().unwrap();
        let new_folder = new_folder.canonicalize().unwrap();
        let selected_image = selected_image.canonicalize().unwrap();
        let mut app = app_for_browser_tests(&old_folder);
        app.load_right_pane_directory(old_folder.clone());
        finish_reads(&mut app);
        app.state.browser = Some(
            BrowserState::for_directory(
                new_folder,
                Some(selected_image.clone()),
                HashMap::new(),
                false,
                &["jpg".to_owned()],
            )
            .unwrap(),
        );
        fs::remove_file(&selected_image).unwrap();

        app.apply_browser_selection_with_force(true);
        finish_reads(&mut app);

        assert_eq!(app.state.directory, old_folder);
        assert_eq!(
            app.state.current_path(),
            Some(old_image.canonicalize().unwrap())
        );
        assert!(
            app.right_pane_error
                .as_deref()
                .is_some_and(|error| error.contains("no longer available"))
        );
    }

    #[test]
    fn browser_file_selection_loads_right_pane_preview() {
        let temp = tempdir().unwrap();
        let image = temp.path().join("image.jpg");
        touch(&image);
        let mut app = app_for_browser_tests(temp.path());
        app.state.browser = Some(
            BrowserState::for_directory(
                temp.path().canonicalize().unwrap(),
                Some(image.canonicalize().unwrap()),
                HashMap::new(),
                false,
                &["jpg".to_owned()],
            )
            .unwrap(),
        );

        app.apply_browser_selection();
        finish_reads(&mut app);

        assert_eq!(app.state.mode, ViewMode::Preview);
        assert_eq!(
            app.state.current_path(),
            Some(image.canonicalize().unwrap())
        );
    }

    #[test]
    fn browser_unsupported_selection_disables_current_video_context() {
        let temp = tempdir().unwrap();
        let unsupported = temp.path().join("note.txt");
        touch(&unsupported);
        let video_entry = media_entry(
            temp.path().join("clip.mp4"),
            MediaKind::Video(VideoKind::Mp4),
            0,
        );
        let mut state = app_state(vec![video_entry], 0);
        state.browser = Some(
            BrowserState::for_directory(
                temp.path().canonicalize().unwrap(),
                Some(unsupported.canonicalize().unwrap()),
                HashMap::new(),
                false,
                &["jpg".to_owned(), "mp4".to_owned()],
            )
            .unwrap(),
        );
        let app = GuiApp::new(state, None, true, false, temp.path().to_path_buf());

        assert!(app.browser_selected_is_unsupported());
        assert!(!app.current_is_video());
    }

    #[test]
    fn browser_focus_allows_selected_video_context() {
        let temp = tempdir().unwrap();
        let video = temp.path().join("clip.mp4");
        touch(&video);
        let video_entry = media_entry(video.clone(), MediaKind::Video(VideoKind::Mp4), 0);
        let mut state = app_state(vec![video_entry], 0);
        state.browser = Some(
            BrowserState::for_directory(
                temp.path().canonicalize().unwrap(),
                Some(video.canonicalize().unwrap()),
                HashMap::new(),
                false,
                &["mp4".to_owned()],
            )
            .unwrap(),
        );
        let app = GuiApp::new(state, None, true, false, temp.path().to_path_buf());

        assert!(app.browser_is_focused());
        assert!(app.current_is_video());
    }

    #[test]
    fn browser_focused_name_sort_changes_browser_not_right_pane() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join("a.jpg"));
        touch(&temp.path().join("b.jpg"));
        let mut app = app_for_browser_tests(temp.path());
        app.state.browser = Some(
            BrowserState::for_directory(
                temp.path().canonicalize().unwrap(),
                None,
                HashMap::new(),
                false,
                &["jpg".to_owned()],
            )
            .unwrap(),
        );

        let ctx = egui::Context::default();
        run_frame(&mut app, &ctx, vec![key_press(egui::Key::N)]);
        finish_reads(&mut app);

        let browser = app.state.browser.as_ref().unwrap();
        assert_eq!(browser.sort_mode, SortMode::NameDesc);
        assert_eq!(app.state.sort_mode, SortMode::Discovered);
        assert_eq!(
            browser
                .entries
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["b.jpg", "a.jpg"]
        );
    }

    #[test]
    fn right_pane_focused_name_sort_changes_media_not_browser() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join("a.jpg"));
        touch(&temp.path().join("b.jpg"));
        let mut app = app_for_browser_tests(temp.path());
        app.state.browser = Some(
            BrowserState::for_directory(
                temp.path().canonicalize().unwrap(),
                None,
                HashMap::new(),
                false,
                &["jpg".to_owned()],
            )
            .unwrap(),
        );
        app.state.browser.as_mut().unwrap().focus = BrowserPaneFocus::Preview;

        let ctx = egui::Context::default();
        run_frame(&mut app, &ctx, vec![key_press(egui::Key::N)]);
        finish_reads(&mut app);

        let browser = app.state.browser.as_ref().unwrap();
        assert_eq!(browser.sort_mode, SortMode::NameAsc);
        assert_eq!(app.state.sort_mode, SortMode::NameAsc);
    }

    #[test]
    fn resolve_launch_rejects_directories_in_selected_file_scope() {
        let temp = tempdir().unwrap();
        let image = temp.path().join("image.jpg");
        touch(&image);

        let error = resolve_launch(&[image, temp.path().to_path_buf()], None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("multiple path launch accepts files only")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_launch_rejects_symlinks_in_selected_file_scope() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let first = temp.path().join("first.jpg");
        let target = temp.path().join("target.jpg");
        let link = temp.path().join("link.jpg");
        touch(&first);
        touch(&target);
        symlink(&target, &link).unwrap();

        let error = resolve_launch(&[first, link], None).unwrap_err();

        assert!(error.to_string().contains("refusing to open symlink"));
        assert!(target.exists());
    }

    #[test]
    fn direct_file_launch_errors_when_the_file_is_filtered_out() {
        let temp = tempdir().unwrap();
        let video = temp.path().join("clip.mp4");
        let image = temp.path().join("image.jpg");
        touch(&video);
        touch(&image);
        let launch = resolve_launch(std::slice::from_ref(&video), None).unwrap();

        let error = scan_launch_entries(&launch, false, false, &["jpg".to_owned()]).unwrap_err();

        assert!(error.to_string().contains("excluded by the active filters"));
        assert!(error.to_string().contains("clip.mp4"));
    }

    #[test]
    fn selected_launch_entries_filter_and_follow_argument_order() {
        let temp = tempdir().unwrap();
        let image = temp.path().join("image.jpg");
        let video = temp.path().join("clip.mp4");
        let ignored = temp.path().join("ignored.txt");
        touch(&image);
        touch(&video);
        touch(&ignored);
        let launch = resolve_launch(&[video.clone(), ignored, image.clone()], None).unwrap();

        let entries =
            scan_launch_entries(&launch, false, false, &["jpg".to_owned(), "mp4".to_owned()])
                .unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![video.canonicalize().unwrap(), image.canonicalize().unwrap()]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.discovered_order)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn selected_file_scope_rescans_only_original_selected_files() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first.jpg");
        let second = temp.path().join("second.jpg");
        let unselected = temp.path().join("unselected.jpg");
        touch(&first);
        touch(&second);
        touch(&unselected);
        let launch = resolve_launch(&[second.clone(), first.clone()], None).unwrap();
        let LaunchTarget::SelectedFiles { files, .. } = launch else {
            panic!("expected selected files launch");
        };
        let mut state = AppState::new(
            temp.path().canonicalize().unwrap(),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            Vec::new(),
        );
        state.selected_files = Some(files.iter().cloned().collect());

        let entries = scan_state_entries(&state).unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![
                second.canonicalize().unwrap(),
                first.canonicalize().unwrap()
            ]
        );
    }

    #[test]
    fn rescan_invalidates_decodes_and_clears_pending_deletions() {
        let temp = tempdir().unwrap();
        let image = temp.path().join("image.jpg");
        touch(&image);
        let entries = scan_directory(ScanOptions {
            root: temp.path().canonicalize().unwrap(),
            recursive: false,
            include_hidden: false,
            extensions: vec!["jpg".to_owned()],
        })
        .unwrap();
        let mut state = AppState::new(
            temp.path().canonicalize().unwrap(),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            entries,
        );
        state.delete_queue.insert(image.clone());
        let mut app = GuiApp::new(state, None, true, false, temp.path().to_path_buf());
        let generation = app.decode_generation;
        fs::write(&image, b"replacement content").unwrap();

        app.rescan();
        finish_reads(&mut app);

        assert_eq!(app.decode_generation, generation.wrapping_add(1));
        assert!(app.state.delete_queue.is_empty());
        assert_eq!(
            app.state.entries[0].file_len,
            b"replacement content".len() as u64
        );
        assert!(app.status.contains("cleared 1 queued deletion"));
    }

    #[test]
    fn live_reconciliation_preserves_unchanged_queue_and_unqueues_invalidated_files() {
        let temp = tempdir().unwrap();
        let unchanged = temp.path().join("unchanged.jpg");
        let modified = temp.path().join("modified.jpg");
        let removed = temp.path().join("removed.jpg");
        let added = temp.path().join("added.jpg");
        touch(&unchanged);
        touch(&modified);
        touch(&removed);
        let entries = scan_directory(ScanOptions {
            root: temp.path().canonicalize().unwrap(),
            recursive: false,
            include_hidden: false,
            extensions: vec!["jpg".to_owned()],
        })
        .unwrap();
        let mut state = AppState::new(
            temp.path().canonicalize().unwrap(),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            entries,
        );
        for entry in &state.entries {
            state.delete_queue.insert(entry.path.clone());
        }
        let unchanged = unchanged.canonicalize().unwrap();
        let modified = modified.canonicalize().unwrap();
        let removed = removed.canonicalize().unwrap();
        let mut app = GuiApp::new(state, None, true, false, temp.path().to_path_buf());

        fs::write(&modified, b"replacement content with a different length").unwrap();
        fs::remove_file(&removed).unwrap();
        touch(&added);
        let added = added.canonicalize().unwrap();
        app.reconcile_live_media();

        assert_eq!(app.state.delete_queue.len(), 1);
        assert!(app.state.delete_queue.contains(&unchanged));
        assert!(!app.state.delete_queue.contains(&modified));
        assert!(!app.state.delete_queue.contains(&removed));
        assert!(app.state.entries.iter().any(|entry| entry.path == added));
        assert!(app.deferred_preview_prefetch.contains(&added));
        assert!(app.status.contains("live update: +1 -1 ~1"));
        assert!(app.status.contains("unqueued 2 changed deletion"));
    }

    #[test]
    fn decode_key_changes_when_scanned_file_identity_changes() {
        let mut entry = media_entry(
            PathBuf::from("/tmp/media/image.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        let before = TexKey::for_entry(&entry, Variant::Fit, 0);
        entry.file_len = 42;
        let after = TexKey::for_entry(&entry, Variant::Fit, 0);

        assert_ne!(before, after);
    }

    #[test]
    fn live_event_scope_respects_recursive_and_selected_file_modes() {
        let directory = PathBuf::from("/tmp/media");
        let mut state = AppState::new(
            directory.clone(),
            false,
            false,
            MediaMode::Both,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            Vec::new(),
        );
        assert!(path_affects_media_state(
            Path::new("/tmp/media/image.jpg"),
            &state
        ));
        assert!(!path_affects_media_state(
            Path::new("/tmp/media/nested/image.jpg"),
            &state
        ));

        state.recursive = true;
        assert!(path_affects_media_state(
            Path::new("/tmp/media/nested/image.jpg"),
            &state
        ));

        state.selected_files = Some(IndexSet::from([directory.join("chosen.jpg")]));
        assert!(path_affects_media_state(
            Path::new("/tmp/media/chosen.jpg"),
            &state
        ));
        assert!(!path_affects_media_state(
            Path::new("/tmp/media/unrelated.jpg"),
            &state
        ));
    }

    #[test]
    fn stale_background_refresh_result_is_ignored() {
        let original = media_entry(
            PathBuf::from("/tmp/media/original.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        let replacement = media_entry(
            PathBuf::from("/tmp/media/replacement.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        let state = app_state(vec![original.clone()], 0);
        let mut app = GuiApp::new(state, None, true, false, PathBuf::from("/tmp/media"));
        app.media_refresh_generation = 2;
        let scope = app.media_refresh_scope();

        app.handle_refresh_result(RefreshResult::Media {
            generation: 1,
            scope: scope.clone(),
            sort_mode: SortMode::Discovered,
            result: Ok(prepare_media_refresh(
                vec![original.clone()],
                vec![replacement.clone()],
                SortMode::Discovered,
                None,
            )),
        });
        assert_eq!(app.state.entries[0].path, original.path);

        app.handle_refresh_result(RefreshResult::Media {
            generation: 2,
            scope,
            sort_mode: SortMode::Discovered,
            result: Ok(prepare_media_refresh(
                vec![original.clone()],
                vec![replacement.clone()],
                SortMode::Discovered,
                None,
            )),
        });
        assert_eq!(app.state.entries[0].path, replacement.path);
    }

    #[test]
    fn background_refresh_with_stale_sort_is_ignored_and_rescheduled() {
        let original = media_entry(
            PathBuf::from("/tmp/media/original.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        let replacement = media_entry(
            PathBuf::from("/tmp/media/replacement.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        let mut state = app_state(vec![original.clone()], 0);
        state.sort_mode = SortMode::NameAsc;
        let mut app = GuiApp::new(state, None, true, false, PathBuf::from("/tmp/media"));
        app.media_refresh_generation = 2;
        let scope = app.media_refresh_scope();

        app.handle_refresh_result(RefreshResult::Media {
            generation: 2,
            scope,
            sort_mode: SortMode::Discovered,
            result: Ok(prepare_media_refresh(
                vec![original.clone()],
                vec![replacement],
                SortMode::Discovered,
                None,
            )),
        });

        assert_eq!(app.state.entries[0].path, original.path);
        assert_eq!(app.media_refresh_generation, 3);
    }

    #[test]
    fn paused_video_suspends_preview_work_but_applies_live_refresh() {
        let video_path = PathBuf::from("/tmp/media/current.mp4");
        let current = media_entry(video_path.clone(), MediaKind::Video(VideoKind::Mp4), 0);
        let added = media_entry(
            PathBuf::from("/tmp/media/added.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            1,
        );
        let state = app_state(vec![current.clone()], 0);
        let mut app = GuiApp::new(state, None, true, false, PathBuf::from("/tmp/media"));
        app.state.mode = ViewMode::Preview;
        app.active_video = Some(ActiveVideo {
            handle: video::PlaybackHandle::test_handle(video_path, true),
            texture: None,
            position: Duration::ZERO,
            duration: None,
            ended: false,
        });
        app.sync_work_suspension_state();
        assert!(app.preview_work_suspended.load(Ordering::Relaxed));
        assert!(app.live_refresh_repaint_enabled.load(Ordering::Relaxed));
        app.media_refresh_generation = 1;
        let scope = app.media_refresh_scope();

        app.handle_refresh_result(RefreshResult::Media {
            generation: 1,
            scope,
            sort_mode: SortMode::Discovered,
            result: Ok(prepare_media_refresh(
                vec![current.clone()],
                vec![current, added],
                SortMode::Discovered,
                None,
            )),
        });

        assert_eq!(app.state.entries.len(), 2);
        assert!(!app.deferred_live_refresh_pending);
        assert!(app.preview_work_suspended.load(Ordering::Relaxed));
    }

    #[test]
    fn boundary_navigation_waits_for_a_newer_live_entry() {
        let current_path = PathBuf::from("/tmp/media/current.mp4");
        let older_path = PathBuf::from("/tmp/media/older.mp4");
        let newer_path = PathBuf::from("/tmp/media/newer.mp4");
        let mut current = media_entry(current_path.clone(), MediaKind::Video(VideoKind::Mp4), 1);
        let older = media_entry(older_path, MediaKind::Video(VideoKind::Mp4), 0);
        current.modified = Some(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(2));
        let mut state = app_state(vec![current.clone(), older.clone()], 0);
        state.sort_mode = SortMode::Newest;
        let mut app = GuiApp::new(state, None, true, false, PathBuf::from("/tmp/media"));
        app.active_video = Some(ActiveVideo {
            handle: video::PlaybackHandle::test_handle(current_path, true),
            texture: None,
            position: Duration::ZERO,
            duration: None,
            ended: false,
        });
        app.sync_work_suspension_state();
        app.media_refresh_generation = 1;
        app.media_refresh_inflight = true;

        app.move_by(-1);
        assert_eq!(app.state.current_path().as_ref(), Some(&current.path));

        let mut newer = media_entry(newer_path.clone(), MediaKind::Video(VideoKind::Mp4), 0);
        newer.modified = Some(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(3));
        let scope = app.media_refresh_scope();
        app.handle_refresh_result(RefreshResult::Media {
            generation: 1,
            scope,
            sort_mode: SortMode::Newest,
            result: Ok(prepare_media_refresh(
                vec![current.clone(), older.clone()],
                vec![current, older, newer],
                SortMode::Newest,
                None,
            )),
        });

        assert_eq!(app.state.current_path().as_ref(), Some(&newer_path));
        assert!(app.active_video.is_none());
    }

    #[test]
    fn focus_gain_allows_live_refresh_while_focus_paused_video_stays_paused() {
        let video_path = PathBuf::from("/tmp/media/current.mp4");
        let current = media_entry(video_path.clone(), MediaKind::Video(VideoKind::Mp4), 0);
        let state = app_state(vec![current], 0);
        let mut app = GuiApp::new(state, None, true, false, PathBuf::from("/tmp/media"));
        app.active_video = Some(ActiveVideo {
            handle: video::PlaybackHandle::test_handle(video_path, false),
            texture: None,
            position: Duration::ZERO,
            duration: None,
            ended: false,
        });
        app.media_refresh_inflight = true;

        app.handle_focus_change(false);

        assert!(app.preview_work_suspended.load(Ordering::Relaxed));
        assert!(!app.live_refresh_repaint_enabled.load(Ordering::Relaxed));
        assert!(!app.media_refresh_inflight);
        assert!(app.deferred_live_refresh_pending);

        app.handle_focus_change(true);
        app.flush_deferred_live_refresh();

        assert!(app.preview_work_suspended.load(Ordering::Relaxed));
        assert!(!app.live_refresh_suspended.load(Ordering::Relaxed));
        assert!(app.live_refresh_repaint_enabled.load(Ordering::Relaxed));
        assert!(!app.deferred_live_refresh_pending);
        assert_eq!(app.media_refresh_generation, 1);
        assert!(app.active_video.as_ref().unwrap().handle.is_paused());
    }

    #[test]
    fn initial_video_autoplay_only_for_matching_direct_video_file() {
        let image_path = PathBuf::from("/tmp/media/image.jpg");
        let video_path = PathBuf::from("/tmp/media/video.mp4");
        let image = media_entry(image_path.clone(), MediaKind::Image(ImageKind::Jpeg), 0);
        let video = media_entry(video_path.clone(), MediaKind::Video(VideoKind::Mp4), 1);

        let mut state = app_state(vec![image.clone(), video.clone()], 1);
        assert!(should_autoplay_initial_video(&state, Some(&video_path)));

        assert!(!should_autoplay_initial_video(&state, None));

        state = app_state(vec![image.clone()], 0);
        assert!(!should_autoplay_initial_video(&state, Some(&image_path)));

        state = app_state(vec![image, video], 0);
        assert!(!should_autoplay_initial_video(&state, Some(&video_path)));

        state = app_state(Vec::new(), 0);
        assert!(!should_autoplay_initial_video(&state, Some(&video_path)));
    }

    #[test]
    fn direct_video_autoplay_waits_for_a_focused_frame() {
        let video_path = PathBuf::from("/tmp/media/video.mp4");
        let state = app_state(
            vec![media_entry(
                video_path.clone(),
                MediaKind::Video(VideoKind::Mp4),
                0,
            )],
            0,
        );
        let mut pending = Some(video_path.clone());

        assert_eq!(
            take_pending_initial_video_autoplay(false, &mut pending, &state),
            None
        );
        assert_eq!(pending.as_ref(), Some(&video_path));
        assert_eq!(
            take_pending_initial_video_autoplay(true, &mut pending, &state),
            Some(video_path)
        );
        assert!(pending.is_none());
    }

    #[test]
    fn selection_autoplay_stays_armed_across_images_until_pause() {
        let image = media_entry(
            PathBuf::from("/tmp/media/image.jpg"),
            MediaKind::Image(ImageKind::Jpeg),
            0,
        );
        let video = media_entry(
            PathBuf::from("/tmp/media/video.mp4"),
            MediaKind::Video(VideoKind::Mp4),
            1,
        );

        assert!(selection_autoplay_after_playback_start());
        assert!(!should_autoplay_selected_video(true, Some(&image)));
        assert!(should_autoplay_selected_video(true, Some(&video)));
        assert!(!selection_autoplay_after_pause_state(true));
        assert!(selection_autoplay_after_pause_state(false));
    }

    #[test]
    fn video_seek_target_moves_by_ten_percent_and_clamps() {
        let duration = Duration::from_secs(100);

        assert_eq!(
            video_seek_target(
                Duration::from_secs(50),
                Some(duration),
                VideoSeekDirection::Backward
            ),
            Some(Duration::from_secs(40))
        );
        assert_eq!(
            video_seek_target(
                Duration::from_secs(50),
                Some(duration),
                VideoSeekDirection::Forward
            ),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            video_seek_target(
                Duration::from_secs(5),
                Some(duration),
                VideoSeekDirection::Backward
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(
            video_seek_target(
                Duration::from_secs(95),
                Some(duration),
                VideoSeekDirection::Forward
            ),
            Some(duration)
        );
    }

    #[test]
    fn video_seek_target_requires_known_nonzero_duration() {
        assert_eq!(
            video_seek_target(Duration::from_secs(5), None, VideoSeekDirection::Forward),
            None
        );
        assert_eq!(
            video_seek_target(
                Duration::from_secs(5),
                Some(Duration::ZERO),
                VideoSeekDirection::Backward
            ),
            None
        );
    }

    #[test]
    fn video_seek_preserves_playing_session_but_not_paused_session() {
        assert!(selection_autoplay_after_seek(false));
        assert!(!selection_autoplay_after_seek(true));
    }

    #[test]
    fn contextual_u_rewinds_active_video_otherwise_unqueues() {
        assert_eq!(contextual_u_key_action(true), UKeyAction::SeekBackward);
        assert_eq!(contextual_u_key_action(false), UKeyAction::Unqueue);
    }

    #[test]
    fn y_confirms_delete_only_inside_confirmation_modal() {
        assert_eq!(y_key_action(true), YKeyAction::ConfirmDelete);
        assert_eq!(y_key_action(false), YKeyAction::ShowProgress);
    }

    #[test]
    fn video_progress_fraction_clamps_and_requires_duration() {
        let duration = Duration::from_secs(100);

        assert_eq!(
            video_progress(Duration::from_secs(25), Some(duration)),
            Some(VideoProgress {
                position: Duration::from_secs(25),
                duration,
                fraction: 0.25,
            })
        );
        assert_eq!(
            video_progress(Duration::from_secs(125), Some(duration)),
            Some(VideoProgress {
                position: duration,
                duration,
                fraction: 1.0,
            })
        );
        assert_eq!(video_progress(Duration::from_secs(1), None), None);
        assert_eq!(
            video_progress(Duration::from_secs(1), Some(Duration::ZERO)),
            None
        );
    }

    #[test]
    fn progress_overlay_trigger_extends_deadline_for_manual_or_seek_reveal() {
        let first = video_progress_overlay_deadline_for_trigger(10.0, true);
        let second = video_progress_overlay_deadline_for_trigger(11.5, true);

        assert_eq!(first, Some(12.0));
        assert_eq!(second, Some(13.5));
        assert!(second > first);
        assert_eq!(
            video_progress_overlay_deadline_for_trigger(10.0, false),
            None
        );
    }

    #[test]
    fn progress_overlay_visibility_uses_deadline() {
        assert!(video_progress_overlay_target_visible(11.99, 12.0));
        assert!(!video_progress_overlay_target_visible(12.0, 12.0));
        assert!(!video_progress_overlay_target_visible(12.01, 12.0));
    }

    #[test]
    fn video_timestamp_formats_minutes_and_hours() {
        assert_eq!(format_video_timestamp(Duration::from_secs(65)), "1:05");
        assert_eq!(format_video_timestamp(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn auto_next_finds_next_video_without_wrapping() {
        let entries = vec![
            media_entry(
                PathBuf::from("/tmp/media/first.mp4"),
                MediaKind::Video(VideoKind::Mp4),
                0,
            ),
            media_entry(
                PathBuf::from("/tmp/media/image.jpg"),
                MediaKind::Image(ImageKind::Jpeg),
                1,
            ),
            media_entry(
                PathBuf::from("/tmp/media/second.mov"),
                MediaKind::Video(VideoKind::Mov),
                2,
            ),
        ];

        assert_eq!(next_video_index_after(&entries, 0), Some(2));
        assert_eq!(next_video_index_after(&entries, 1), Some(2));
        assert_eq!(next_video_index_after(&entries, 2), None);
        assert_eq!(next_video_index_after(&entries, usize::MAX), None);
    }

    #[test]
    fn repeat_takes_precedence_over_auto_next_and_never_retries_failures() {
        assert_eq!(
            playback_end_action(true, false, true, true),
            PlaybackEndAction::RepeatCurrent
        );
        assert_eq!(
            playback_end_action(true, false, false, true),
            PlaybackEndAction::Advance
        );
        assert_eq!(
            playback_end_action(true, true, true, true),
            PlaybackEndAction::None
        );
        assert_eq!(
            playback_end_action(false, false, true, true),
            PlaybackEndAction::None
        );
    }

    #[test]
    fn p_toggles_video_repeat_and_selection_keeps_it_sticky() {
        let entries = vec![
            media_entry(
                PathBuf::from("/tmp/media/first.mp4"),
                MediaKind::Video(VideoKind::Mp4),
                0,
            ),
            media_entry(
                PathBuf::from("/tmp/media/second.mp4"),
                MediaKind::Video(VideoKind::Mp4),
                1,
            ),
        ];
        let state = app_state(entries, 0);
        let mut app = GuiApp::new(state, None, true, false, PathBuf::from("/tmp/media"));
        let ctx = egui::Context::default();

        run_frame(&mut app, &ctx, vec![key_press(egui::Key::P)]);
        assert!(app.repeat_video);

        app.set_current_index(1);
        assert!(app.repeat_video);

        run_frame(&mut app, &ctx, vec![key_press(egui::Key::P)]);
        assert!(!app.repeat_video);
    }

    #[test]
    fn playback_errors_are_not_overwritten_by_the_ended_status() {
        assert_eq!(
            playback_terminal_status(true, Some("corrupt stream")),
            Some("video failed: corrupt stream".to_owned())
        );
        assert_eq!(
            playback_terminal_status(true, None),
            Some("video ended".to_owned())
        );
    }

    #[test]
    fn focus_loss_pauses_video_and_clears_selection_autoplay() {
        assert_eq!(
            focus_playback_effect(true, false, true, true),
            FocusPlaybackEffect {
                pause_video: true,
                selection_autoplay_armed: false,
            }
        );
    }

    #[test]
    fn focus_gain_does_not_auto_resume() {
        assert_eq!(
            focus_playback_effect(false, true, false, false),
            FocusPlaybackEffect {
                pause_video: false,
                selection_autoplay_armed: false,
            }
        );
    }

    #[test]
    fn unchanged_focus_state_preserves_playback_state() {
        assert_eq!(
            focus_playback_effect(true, true, true, true),
            FocusPlaybackEffect {
                pause_video: false,
                selection_autoplay_armed: true,
            }
        );
        assert_eq!(
            focus_playback_effect(false, false, false, false),
            FocusPlaybackEffect {
                pause_video: false,
                selection_autoplay_armed: false,
            }
        );
    }

    fn touch(path: &Path) {
        fs::write(path, b"not actually decoded").unwrap();
    }

    fn app_state(entries: Vec<MediaEntry>, current_index: usize) -> AppState {
        let mut state = AppState::new(
            PathBuf::from("/tmp/media"),
            false,
            false,
            MediaMode::Both,
            vec!["jpg".to_owned(), "mp4".to_owned()],
            SortMode::Discovered,
            entries,
        );
        state.current_index = current_index;
        state
    }

    fn app_for_browser_tests(directory: &Path) -> GuiApp {
        let state = AppState::new(
            directory.canonicalize().unwrap(),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            Vec::new(),
        );
        GuiApp::new(state, None, true, false, directory.to_path_buf())
    }

    fn media_entry(path: PathBuf, media_kind: MediaKind, discovered_order: usize) -> MediaEntry {
        let file_name = path
            .file_name()
            .map(|value| value.to_os_string())
            .unwrap_or_else(|| OsString::from("media"));
        let display_name = file_name.to_string_lossy().into_owned();
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().into_owned());

        MediaEntry {
            path,
            file_name,
            display_name,
            extension,
            file_len: 0,
            created: None,
            modified: None,
            discovered_order,
            dimensions: None,
            media_kind,
            exif_date: None,
            exif_orientation: None,
            dimensions_attempted: false,
            exif_attempted: false,
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    let launch = resolve_launch(&cli.paths, cli.directory.as_deref())?;
    let extensions = cli.resolved_extensions();
    let sort_mode = cli.initial_sort_mode();
    let mut entries = scan_launch_entries(&launch, cli.recursive, cli.hidden, &extensions)?;
    sorter::sort_entries(&mut entries, sort_mode, cli.locale.as_deref());
    let directory = launch.directory().to_path_buf();
    let initial_file = launch.initial_file().cloned();
    let empty_media_target = launch.empty_media_target();

    let mut state = AppState::new(
        directory,
        cli.recursive,
        cli.hidden,
        MediaMode::from(cli.media),
        extensions,
        sort_mode,
        entries,
    );
    if let LaunchTarget::SelectedFiles { files, .. } = &launch {
        state.selected_files = Some(files.iter().cloned().collect());
    }
    state.mode = initial_view_mode_for_launch(&launch);
    // An unreadable listing shouldn't abort a launch that would otherwise
    // succeed — fall back to the normal view and report it in the status bar.
    let mut browser_launch_error = None;
    if cli.view == CliViewMode::Browser {
        match browser_state_for_launch(&launch, cli.hidden, &state.extensions) {
            Ok(browser) => state.browser = browser,
            Err(error) => {
                tracing::warn!(%error, "browser view unavailable at launch");
                browser_launch_error = Some(error);
            }
        }
    }
    // Start positioned on the requested file (after sorting).
    if let Some(file) = &initial_file
        && let Some(index) = state.entries.iter().position(|entry| entry.path == *file)
    {
        state.current_index = index;
    }
    let autoplay_initial_video = should_autoplay_initial_video(&state, initial_file.as_deref());
    tracing::debug!(
        directory = %state.directory.display(),
        file = ?initial_file,
        selected_files = state.selected_files.as_ref().map(IndexSet::len).unwrap_or(0),
        start_index = state.current_index,
        count = state.entries.len(),
        "opened"
    );
    let mut app = GuiApp::new(
        state,
        cli.locale.clone(),
        cli.dry_run_delete,
        cli.auto_next,
        empty_media_target,
    );
    if autoplay_initial_video {
        app.pending_initial_video_autoplay = app.state.current_path();
    }
    if let Some(error) = browser_launch_error {
        app.status = format!("browser failed: {error:#}");
    }

    let options = eframe::NativeOptions {
        // Avoid the NVIDIA/Wayland swap_buffers wait observed during workspace switches.
        vsync: false,
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_IDENTITY)
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_IDENTITY,
        options,
        Box::new(move |cc| {
            app.attach_watcher(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow!("eframe failed: {error}"))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchTarget {
    Directory {
        directory: PathBuf,
        initial_file: Option<PathBuf>,
    },
    SelectedFiles {
        directory: PathBuf,
        files: Vec<PathBuf>,
    },
}

impl LaunchTarget {
    fn directory(&self) -> &Path {
        match self {
            Self::Directory { directory, .. } | Self::SelectedFiles { directory, .. } => directory,
        }
    }

    fn initial_file(&self) -> Option<&PathBuf> {
        match self {
            Self::Directory { initial_file, .. } => initial_file.as_ref(),
            Self::SelectedFiles { .. } => None,
        }
    }

    fn empty_media_target(&self) -> PathBuf {
        match self {
            Self::Directory {
                directory,
                initial_file,
            } => initial_file.clone().unwrap_or_else(|| directory.clone()),
            Self::SelectedFiles { directory, files } => {
                files.first().cloned().unwrap_or_else(|| directory.clone())
            }
        }
    }
}

fn resolve_launch(paths: &[PathBuf], directory: Option<&Path>) -> Result<LaunchTarget> {
    if paths.len() > 1 {
        return resolve_selected_files(paths);
    }

    let input = paths.first().map(PathBuf::as_path).or(directory);
    let (directory, initial_file) = resolve_input(input)?;
    Ok(LaunchTarget::Directory {
        directory,
        initial_file,
    })
}

fn resolve_selected_files(paths: &[PathBuf]) -> Result<LaunchTarget> {
    let mut seen = IndexSet::new();
    let mut files = Vec::new();

    for path in paths {
        let input_metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if input_metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "refusing to open symlink as a selected file: {}",
                path.display()
            ));
        }
        let canonical = path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        if canonical.is_dir() {
            return Err(anyhow!(
                "{} is a directory; multiple path launch accepts files only",
                canonical.display()
            ));
        }
        if !canonical.is_file() {
            return Err(anyhow!("{} is not a file", canonical.display()));
        }
        if seen.insert(canonical.clone()) {
            files.push(canonical);
        }
    }

    let first = files
        .first()
        .context("selected file launch requires at least one file")?;
    let directory = first
        .parent()
        .context("selected file has no parent directory")?
        .to_path_buf();

    Ok(LaunchTarget::SelectedFiles { directory, files })
}

fn scan_launch_entries(
    launch: &LaunchTarget,
    recursive: bool,
    include_hidden: bool,
    extensions: &[String],
) -> Result<Vec<MediaEntry>> {
    let entries = match launch {
        LaunchTarget::Directory { directory, .. } => scan_directory(ScanOptions {
            root: directory.clone(),
            recursive,
            include_hidden,
            extensions: extensions.to_vec(),
        }),
        LaunchTarget::SelectedFiles { files, .. } => scan_files(files, extensions),
    }?;
    if let LaunchTarget::Directory {
        initial_file: Some(initial_file),
        ..
    } = launch
        && !entries.iter().any(|entry| &entry.path == initial_file)
    {
        return Err(anyhow!(
            "requested file is unsupported or excluded by the active filters: {}",
            initial_file.display()
        ));
    }
    Ok(entries)
}

#[cfg(test)]
fn scan_state_entries(state: &AppState) -> Result<Vec<MediaEntry>> {
    if let Some(selected_files) = &state.selected_files {
        let files = selected_files.iter().cloned().collect::<Vec<_>>();
        scan_files(&files, &state.extensions)
    } else {
        scan_directory(ScanOptions {
            root: state.directory.clone(),
            recursive: state.recursive,
            include_hidden: state.include_hidden,
            extensions: state.extensions.clone(),
        })
    }
}

fn path_affects_media_state(path: &Path, state: &AppState) -> bool {
    if let Some(selected_files) = &state.selected_files {
        return selected_files
            .iter()
            .any(|file| path == file || file.parent().is_some_and(|parent| path == parent));
    }
    if state.recursive {
        path_is_within(path, &state.directory)
    } else {
        path_affects_flat_directory(path, &state.directory)
    }
}

fn path_affects_flat_directory(path: &Path, directory: &Path) -> bool {
    path == directory || path.parent().is_some_and(|parent| parent == directory)
}

fn initial_view_mode_for_launch(launch: &LaunchTarget) -> ViewMode {
    match launch {
        LaunchTarget::Directory { initial_file, .. } => initial_view_mode(initial_file.as_deref()),
        LaunchTarget::SelectedFiles { .. } => ViewMode::Grid,
    }
}

/// Resolve the input path into a directory to scan plus, if a file was given,
/// the specific file to open. A file implies its parent directory.
fn resolve_input(input: Option<&Path>) -> Result<(PathBuf, Option<PathBuf>)> {
    let path = match input {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    let input_metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;

    if canonical.is_dir() {
        Ok((canonical, None))
    } else if canonical.is_file() {
        if input_metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "refusing to open symlink as a media file: {}",
                path.display()
            ));
        }
        let directory = canonical
            .parent()
            .context("file has no parent directory")?
            .to_path_buf();
        Ok((directory, Some(canonical)))
    } else {
        Err(anyhow!(
            "{} is not a file or directory",
            canonical.display()
        ))
    }
}
