//! GPU-windowed frontend (`cullr-gui`).
//!
//! Replaces the terminal UI with a native window. Reuses the whole non-terminal
//! core — directory scanning, sorting, the delete queue, navigation/zoom state,
//! and the JPEG-accelerated decode pipeline. Images decode once on worker
//! threads and upload as GPU textures, so the GPU rescales to any window size
//! for free (no terminal graphics protocol, no per-frame re-transmission).

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    path::{Path, PathBuf},
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
        preferred_browser_index, read_browser_entries,
    },
    cli::{Cli, CliViewMode},
    decode::decode_rgba_capped,
    delete, metadata,
    scanner::{ScanOptions, scan_directory, scan_files},
    sorter,
    state::{
        AppState, BrowserEntry, BrowserEntryKind, BrowserPaneFocus, BrowserState, MediaEntry,
        MediaKind, MediaMode, ViewMode, ZoomMode,
    },
    video,
};

/// Long-edge cap for fit-to-window decodes. A fit view never needs more pixels
/// than a high-DPI monitor; the GPU handles any further downscaling.
const FIT_CAP: u32 = 3840;
/// Long-edge cap for grid thumbnails.
const THUMB_CAP: u32 = 320;
/// How many media files on each side of the current one to decode ahead in preview.
const PREFETCH_RADIUS: usize = 4;
/// Resident texture budgets (previews are large, thumbnails small).
const PREVIEW_CACHE: usize = 32;
const THUMB_CACHE: usize = 512;
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Clone, PartialEq, Eq, Hash)]
struct TexKey {
    path: PathBuf,
    variant: Variant,
    media_kind: MediaKind,
}

struct DecodeRequest {
    key: TexKey,
}

struct DecodeResult {
    key: TexKey,
    image: std::result::Result<image::RgbaImage, String>,
}

/// A small worker pool that turns paths into oriented RGBA buffers.
struct DecodeService {
    job_tx: Sender<DecodeRequest>,
    result_rx: Receiver<DecodeResult>,
}

impl DecodeService {
    fn new() -> Self {
        let (job_tx, job_rx) = flume::unbounded::<DecodeRequest>();
        let (result_tx, result_rx) = flume::unbounded::<DecodeResult>();
        let workers = thread::available_parallelism()
            .map(|count| count.get().clamp(2, 6))
            .unwrap_or(2);
        for _ in 0..workers {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            thread::spawn(move || {
                for job in job_rx.iter() {
                    let image = match &job.key.media_kind {
                        MediaKind::Image(_) => {
                            decode_rgba_capped(&job.key.path, job.key.variant.cap())
                        }
                        MediaKind::Video(_) => {
                            video::decode_first_frame_rgba(&job.key.path, job.key.variant.cap())
                        }
                    }
                    .map_err(|e| format!("{e:#}"));
                    tracing::debug!(path = %job.key.path.display(), ok = image.is_ok(), "decoded");
                    let _ = result_tx.send(DecodeResult {
                        key: job.key,
                        image,
                    });
                }
            });
        }
        Self { job_tx, result_rx }
    }

    fn request(&self, key: TexKey) {
        let _ = self.job_tx.send(DecodeRequest { key });
    }
}

struct GuiApp {
    state: AppState,
    decoder: DecodeService,
    video_tx: Sender<video::PlaybackEvent>,
    video_rx: Receiver<video::PlaybackEvent>,
    previews: LruCache<TexKey, egui::TextureHandle>,
    thumbs: LruCache<TexKey, egui::TextureHandle>,
    inflight: HashSet<TexKey>,
    failed: HashMap<TexKey, String>,
    locale: Option<String>,
    dry_run: bool,
    status: String,
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
    video_muted: bool,
    selection_autoplay_armed: bool,
    auto_next: bool,
    media_type_badges_visible: bool,
    video_progress_overlay_visible_until: f64,
    empty_media_target: PathBuf,
    // Deferred from inside the input closure (which borrows ctx immutably).
    pending_sort: Option<crate::state::SortMode>,
    pending_enrich: bool,
    pending_rescan: bool,
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
        let (video_tx, video_rx) = flume::unbounded();
        let status = if state.entries.is_empty() {
            empty_media_status(&empty_media_target)
        } else {
            String::new()
        };
        Self {
            state,
            decoder: DecodeService::new(),
            video_tx,
            video_rx,
            previews: LruCache::new(NonZeroUsize::new(PREVIEW_CACHE).unwrap()),
            thumbs: LruCache::new(NonZeroUsize::new(THUMB_CACHE).unwrap()),
            inflight: HashSet::new(),
            failed: HashMap::new(),
            locale,
            dry_run,
            status,
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
            video_muted: true,
            selection_autoplay_armed: false,
            auto_next,
            media_type_badges_visible: true,
            video_progress_overlay_visible_until: 0.0,
            empty_media_target,
            pending_sort: None,
            pending_enrich: false,
            pending_rescan: false,
        }
    }

    fn preview_variant(&self) -> Variant {
        match self.state.zoom_mode {
            ZoomMode::Fit => Variant::Fit,
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
        self.inflight.insert(key.clone());
        self.decoder.request(key);
    }

    /// Move the selection by `delta` entries, clamped to the list bounds.
    fn move_by(&mut self, delta: isize) {
        let len = self.state.entries.len();
        if len == 0 {
            return;
        }
        let current = self.state.current_index as isize;
        let target = (current + delta).clamp(0, len as isize - 1);
        self.set_current_index(target as usize);
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
        if self.state.browser.is_some() {
            self.state.browser = None;
            self.pending_browser_apply = false;
            self.browser_apply_deadline = None;
            self.status = "browser closed".to_owned();
            return;
        }
        if self.state.is_selected_file_scope() {
            self.status = "browser unavailable for explicit selected-file launches".to_owned();
            return;
        }

        match BrowserState::for_current_view(&self.state) {
            Ok(browser) => {
                self.state.browser = Some(browser);
                self.status = "browser opened".to_owned();
                self.apply_browser_selection();
            }
            Err(error) => {
                self.status = format!("browser failed: {error}");
            }
        }
    }

    fn refresh_browser_listing_preserving_selection(&mut self) -> Result<()> {
        let Some(browser) = self.state.browser.as_mut() else {
            return Ok(());
        };
        let previous = browser.selected_path();
        let fallback = browser
            .remembered_selection
            .get(&browser.listed_directory)
            .cloned();
        let entries = read_browser_entries(
            &browser.listed_directory,
            self.state.include_hidden,
            &self.state.extensions,
        )?;
        browser.entries = entries;
        browser.selected_index = preferred_browser_index(
            &browser.entries,
            previous.as_ref().or(fallback.as_ref()),
            browser.selected_index,
        );
        browser.scroll_to_selection = true;
        browser.remember_current_selection();
        Ok(())
    }

    fn toggle_hidden_files(&mut self) {
        self.state.include_hidden = !self.state.include_hidden;
        let hidden = self.state.include_hidden;
        if self.state.browser.is_some() {
            match self.refresh_browser_listing_preserving_selection() {
                Ok(()) => {
                    self.apply_browser_selection();
                    self.status = if hidden {
                        "hidden files shown".to_owned()
                    } else {
                        "hidden files hidden".to_owned()
                    };
                }
                Err(error) => {
                    self.status = format!("browser refresh failed: {error}");
                }
            }
        } else {
            self.pending_rescan = true;
            self.status = if hidden {
                "hidden files shown".to_owned()
            } else {
                "hidden files hidden".to_owned()
            };
        }
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
        let remembered = self
            .state
            .browser
            .as_ref()
            .map(|browser| browser.remembered_selection.clone())
            .unwrap_or_default();
        match BrowserState::for_directory(
            directory,
            preferred_selection,
            remembered,
            self.state.include_hidden,
            &self.state.extensions,
        ) {
            Ok(browser) => {
                self.state.browser = Some(browser);
                self.apply_browser_selection();
            }
            Err(error) => {
                self.status = format!("browser failed: {error}");
            }
        }
    }

    fn apply_browser_selection(&mut self) {
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
                self.load_right_pane_file(entry.path);
            }
            BrowserEntryKind::UnsupportedFile => {
                self.stop_active_video();
                self.state.mode = ViewMode::Preview;
                self.empty_media_target = entry.path.clone();
                self.status = format!("unsupported file: {}", entry.display_name);
            }
        }
    }

    fn load_right_pane_directory(&mut self, directory: PathBuf) {
        self.stop_active_video();
        self.failed.clear();
        let previous = if self.state.directory == directory {
            self.state.current_path()
        } else {
            None
        };
        self.state.selected_files = None;
        self.state.directory = directory.clone();
        self.state.mode = ViewMode::Grid;
        self.empty_media_target = directory.clone();
        match scan_directory(ScanOptions {
            root: directory.clone(),
            recursive: self.state.recursive,
            include_hidden: self.state.include_hidden,
            extensions: self.state.extensions.clone(),
        }) {
            Ok(mut entries) => {
                sorter::sort_entries(&mut entries, self.state.sort_mode, self.locale.as_deref());
                self.state.set_entries_preserving_current(entries, previous);
                self.status = format!(
                    "scanned {} media files{}",
                    self.state.entries.len(),
                    if self.state.recursive {
                        " recursively"
                    } else {
                        ""
                    }
                );
                self.scroll_to_current = true;
            }
            Err(error) => {
                self.status = format!("scan failed: {error}");
            }
        }
    }

    fn load_right_pane_file(&mut self, path: PathBuf) {
        self.stop_active_video();
        let Some(directory) = path.parent().map(Path::to_path_buf) else {
            self.status = format!("file has no parent: {}", path.display());
            return;
        };
        // Fast path: stepping between files of the already-scanned directory
        // only repositions the selection — no rescan.
        if self.state.selected_files.is_none()
            && self.state.directory == directory
            && let Some(index) = self
                .state
                .entries
                .iter()
                .position(|entry| entry.path == path)
        {
            self.state.mode = ViewMode::Preview;
            self.empty_media_target = path.clone();
            self.state.current_index = index;
            self.status = format!("preview: {}", path.display());
            self.scroll_to_current = true;
            return;
        }
        self.failed.clear();
        self.state.selected_files = None;
        self.state.directory = directory.clone();
        self.state.mode = ViewMode::Preview;
        self.empty_media_target = path.clone();
        match scan_directory(ScanOptions {
            root: directory,
            recursive: self.state.recursive,
            include_hidden: self.state.include_hidden,
            extensions: self.state.extensions.clone(),
        }) {
            Ok(mut entries) => {
                sorter::sort_entries(&mut entries, self.state.sort_mode, self.locale.as_deref());
                self.state
                    .set_entries_preserving_current(entries, Some(path.clone()));
                if let Some(index) = self
                    .state
                    .entries
                    .iter()
                    .position(|entry| entry.path == path)
                {
                    self.state.current_index = index;
                }
                self.status = format!("preview: {}", path.display());
                self.scroll_to_current = true;
            }
            Err(error) => {
                self.status = format!("scan failed: {error}");
            }
        }
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
        if self.browser_selected_is_unsupported() {
            return;
        }
        let len = self.state.entries.len();
        if len == 0 {
            return;
        }
        match self.state.mode {
            ViewMode::Preview => {
                let variant = self.preview_variant();
                let current = self.state.current_index;
                let mut wanted = vec![current];
                for distance in 1..=PREFETCH_RADIUS {
                    wanted.push((current + distance) % len);
                    wanted.push((current + len - (distance % len)) % len);
                }
                for index in wanted {
                    if let Some(entry) = self.state.entries.get(index) {
                        let path = entry.path.clone();
                        self.request(TexKey {
                            path,
                            variant,
                            media_kind: entry.media_kind.clone(),
                        });
                    }
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
                        let path = entry.path.clone();
                        self.request(TexKey {
                            path,
                            variant: Variant::Thumb,
                            media_kind: entry.media_kind.clone(),
                        });
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
        while let Ok(result) = self.decoder.result_rx.try_recv() {
            self.inflight.remove(&result.key);
            match result.image {
                Ok(image) => {
                    let size = [image.width() as usize, image.height() as usize];
                    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
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
        }
    }

    fn drain_video_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.video_rx.try_recv() {
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
            let failed = event.error.is_some();
            if let Some(error) = event.error {
                active.ended = true;
                self.status = format!("video failed: {error}");
            }
            if let Some(image) = event.frame {
                let size = [image.width() as usize, image.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
                if let Some(texture) = active.texture.as_mut() {
                    texture.set(color, egui::TextureOptions::LINEAR);
                } else {
                    active.texture = Some(ctx.load_texture(
                        format!("video-playback:{}", event.path.display()),
                        color,
                        egui::TextureOptions::LINEAR,
                    ));
                }
                ctx.request_repaint();
            }
            if event.ended {
                active.ended = true;
                active.handle.set_paused(true);
                self.status = "video ended".to_owned();
            }
            if event.ended && self.auto_next && !failed {
                self.play_next_video_after_current();
            }
        }
    }

    fn current_is_video(&self) -> bool {
        if self.browser_selected_is_unsupported() {
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

    fn active_video_progress_for(&self, path: &Path) -> Option<VideoProgress> {
        self.active_video_for(path)
            .and_then(|active| video_progress(active.position, active.duration))
    }

    fn active_current_video_can_seek(&self) -> bool {
        if self.browser_selected_is_unsupported() {
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
    }

    fn stop_active_video(&mut self) {
        if let Some(active) = self.active_video.take() {
            active.handle.stop();
        }
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
        self.stop_active_video();
        self.selection_autoplay_armed = selection_autoplay_armed;
        let handle = video::spawn_playback(
            path.clone(),
            FIT_CAP,
            self.video_muted,
            start_at,
            paused,
            self.video_tx.clone(),
        );
        self.active_video = Some(ActiveVideo {
            handle,
            texture: None,
            position: start_at,
            duration: None,
            ended: false,
        });
        self.status = if paused {
            "video paused".to_owned()
        } else if self.video_muted {
            "video playing muted".to_owned()
        } else {
            "video playing with audio".to_owned()
        };
    }

    fn toggle_current_video_playback(&mut self) {
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

    fn set_current_index(&mut self, index: usize) {
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

    fn resort(&mut self, new_mode: crate::state::SortMode) {
        self.stop_active_video();
        let previous = self.state.current_path();
        self.state.sort_mode = new_mode;
        sorter::sort_entries(&mut self.state.entries, new_mode, self.locale.as_deref());
        let entries = std::mem::take(&mut self.state.entries);
        self.state.set_entries_preserving_current(entries, previous);
        self.status = format!("sort: {:?}", new_mode);
        self.scroll_to_current = true;
    }

    fn rescan(&mut self) {
        if self.state.browser.is_some() {
            match self.refresh_browser_listing_preserving_selection() {
                Ok(()) => self.apply_browser_selection(),
                Err(error) => {
                    self.status = format!("browser refresh failed: {error}");
                }
            }
            return;
        }

        self.stop_active_video();
        // Drop remembered decode failures so files that have since been fixed,
        // replaced, or were only transiently unreadable get another attempt.
        self.failed.clear();
        let previous = self.state.current_path();
        let result = scan_state_entries(&self.state);
        match result {
            Ok(mut entries) => {
                sorter::sort_entries(&mut entries, self.state.sort_mode, self.locale.as_deref());
                self.state.set_entries_preserving_current(entries, previous);
                if self.state.entries.is_empty() {
                    self.status = empty_media_status(&self.empty_media_target);
                } else if self.state.is_selected_file_scope() {
                    self.status = format!("scanned {} selected files", self.state.entries.len());
                } else {
                    self.status = format!(
                        "scanned {} media files{}",
                        self.state.entries.len(),
                        if self.state.recursive {
                            " recursively"
                        } else {
                            ""
                        }
                    );
                }
                self.scroll_to_current = true;
            }
            Err(error) => {
                self.status = format!("rescan failed: {error}");
            }
        }
    }

    fn confirm_delete(&mut self) {
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
            let _ = self.refresh_browser_listing_preserving_selection();
            self.apply_browser_selection();
        }
        self.scroll_to_current = true;
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        use egui::Key;
        // Confirmation modal swallows everything except y/n/esc.
        if self.state.confirm_delete {
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
            let right_pane_active = !browser_focused && !unsupported_selected;

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
                    self.set_current_index(0);
                }
                if i.key_pressed(Key::End) {
                    let last = self.state.entries.len().saturating_sub(1);
                    self.set_current_index(last);
                }

                // Delete queue.
                if i.modifiers.shift && i.key_pressed(Key::D) {
                    let before = self.state.current_index;
                    self.state.enter_delete_queue_grid();
                    self.finish_selection_change(before);
                } else if i.key_pressed(Key::D) && !i.modifiers.ctrl {
                    self.state.toggle_queue_current();
                    self.status = format!("queued: {}", self.state.queue_count());
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
                    self.state.zoom_mode = match self.state.zoom_mode {
                        ZoomMode::Fit => ZoomMode::OriginalPixels,
                        ZoomMode::OriginalPixels => ZoomMode::Fit,
                    };
                }
                if i.key_pressed(Key::B) {
                    self.toggle_media_type_badges();
                }
                if i.key_pressed(Key::T) {
                    self.pending_sort = Some(sorter::next_time_sort(self.state.sort_mode));
                }
                if i.key_pressed(Key::N) {
                    self.pending_sort = Some(sorter::next_name_sort(self.state.sort_mode));
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
            metadata::enrich_entry(entry);
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
        let variant = self.preview_variant();
        let Some(entry) = self.state.current_entry() else {
            draw_empty_media_message(ui, &self.empty_media_target);
            return;
        };
        let key = TexKey {
            path: entry.path.clone(),
            variant,
            media_kind: entry.media_kind.clone(),
        };
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
            let scale = match self.state.zoom_mode {
                ZoomMode::Fit => {
                    (container_rect.width() / tex.x).min(container_rect.height() / tex.y)
                }
                ZoomMode::OriginalPixels => 1.0,
            };
            let draw = tex * scale;
            let image_rect = egui::Rect::from_center_size(container_rect.center(), draw);
            egui::Image::new(&handle).paint_at(ui, image_rect);
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
        let key = TexKey {
            path: path.clone(),
            variant: Variant::Thumb,
            media_kind: media_kind.clone(),
        };

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
            egui::Stroke::new(3.0, egui::Color32::from_rgb(240, 200, 0))
        } else if is_queued {
            egui::Stroke::new(2.0, egui::Color32::from_rgb(220, 60, 60))
        } else {
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
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
browser mode:
  e               toggle browser
  ctrl+h / ctrl+l focus browser / preview pane
  j / k           browser down / up
  ctrl+d / ctrl+u browser half page down / up
  l / h           enter folder / parent folder
  enter           open folder / focus preview
  .               show / hide hidden files
home / end      first / last
g               toggle grid
space           play / pause videos
u / o           rewind / fast-forward active video 10%
y               show video progress
d               toggle delete queue
u               unqueue current when no video is active
shift+D         show delete queue
ctrl+R          delete queued (confirm)
z               toggle fit / actual size
f               toggle fullscreen window
m               mute / unmute video audio
a               toggle auto-next video advance
b               show / hide media badges
t / n           cycle time / name sort
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
        let focused = ctx.input(|input| input.focused);
        self.handle_focus_change(focused);
        self.handle_input(ctx);

        // Apply a browser selection only once it has rested briefly, so
        // holding j/k doesn't scan every directory the selection crosses.
        let now = ctx.input(|input| input.time);
        if std::mem::take(&mut self.pending_browser_apply) {
            self.browser_apply_deadline = Some(now + BROWSER_APPLY_DEBOUNCE_SECONDS);
        }
        if let Some(deadline) = self.browser_apply_deadline {
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
        };
        let position = if self.browser_selected_is_unsupported() {
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
            "{mode}  |  {position}  |  {zoom}  |  queued: {}  |  sort: {:?}  |  audio: {}  |  auto-next: {}  |  {}",
            self.state.queue_count(),
            self.state.sort_mode,
            if self.video_muted { "muted" } else { "on" },
            if self.auto_next { "on" } else { "off" },
            self.status,
        );

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| ui.label(status));
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
                if self.state.browser.is_some() {
                    self.draw_browser_right_pane(ui);
                } else {
                    match self.state.mode {
                        ViewMode::Preview => self.draw_preview(ui),
                        ViewMode::Grid | ViewMode::DeleteQueueGrid => self.draw_grid(ui),
                    }
                }
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
        if progress_overlay_visible {
            let remaining = (self.video_progress_overlay_visible_until - now).max(0.0);
            ctx.request_repaint_after(Duration::from_secs_f64(remaining));
        }
        if !self.inflight.is_empty()
            || progress_overlay_visible
            || self
                .active_video
                .as_ref()
                .is_some_and(|active| !active.handle.is_paused() && !active.ended)
        {
            ctx.request_repaint();
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
        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(170)),
        egui::StrokeKind::Inside,
    );

    match media_kind {
        MediaKind::Image(_) => {
            let icon = badge_rect.shrink(4.5);
            let stroke = egui::Stroke::new(1.25, egui::Color32::WHITE);
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
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 2.0),
        egui::Sense::hover(),
    );
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FocusPlaybackEffect {
    pause_video: bool,
    selection_autoplay_armed: bool,
}

fn focus_playback_effect(
    was_focused: bool,
    is_focused: bool,
    active_video_playing: bool,
    selection_autoplay_armed: bool,
) -> FocusPlaybackEffect {
    let pause_video = was_focused && !is_focused && active_video_playing;
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

    fn run_frame(app: &mut GuiApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 800.0),
        ));
        raw.events = events;
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

        assert_eq!(app.state.directory, folder.canonicalize().unwrap());
        assert_eq!(app.state.mode, ViewMode::Grid);
        assert_eq!(app.state.entries.len(), 1);
        assert_eq!(app.state.entries[0].path, image);
    }

    #[test]
    fn browser_empty_folder_selection_shows_empty_folder_gallery() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let mut app = app_for_browser_tests(temp.path());
        app.open_browser_directory(folder.canonicalize().unwrap(), None);

        assert_eq!(app.state.directory, folder.canonicalize().unwrap());
        assert_eq!(app.state.mode, ViewMode::Grid);
        assert!(app.state.entries.is_empty());
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

        assert_eq!(app.state.mode, ViewMode::Preview);
        assert_eq!(app.state.current_path(), Some(image));
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
    if autoplay_initial_video && let Some(path) = app.state.current_path() {
        app.start_video_playback(path);
    }
    if let Some(error) = browser_launch_error {
        app.status = format!("browser failed: {error:#}");
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cullr")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native("cullr", options, Box::new(|_cc| Ok(Box::new(app))))
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
    match launch {
        LaunchTarget::Directory { directory, .. } => scan_directory(ScanOptions {
            root: directory.clone(),
            recursive,
            include_hidden,
            extensions: extensions.to_vec(),
        }),
        LaunchTarget::SelectedFiles { files, .. } => scan_files(files, extensions),
    }
}

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
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;

    if canonical.is_dir() {
        Ok((canonical, None))
    } else if canonical.is_file() {
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
