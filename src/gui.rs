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
};

use anyhow::{Context, Result, anyhow};
use eframe::egui;
use flume::{Receiver, Sender};
use lru::LruCache;

use crate::{
    cli::Cli,
    decode::decode_rgba_capped,
    delete, metadata,
    scanner::{ScanOptions, scan_directory},
    sorter,
    state::{AppState, MediaKind, MediaMode, ViewMode, ZoomMode},
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
    last_visible_rows: std::ops::Range<usize>,
    scroll_to_current: bool,
    fullscreen: bool,
    active_video: Option<ActiveVideo>,
    video_muted: bool,
    media_type_badges_visible: bool,
    empty_media_target: PathBuf,
    // Deferred from inside the input closure (which borrows ctx immutably).
    pending_sort: Option<crate::state::SortMode>,
    pending_enrich: bool,
    pending_rescan: bool,
}

struct ActiveVideo {
    handle: video::PlaybackHandle,
    texture: Option<egui::TextureHandle>,
    ended: bool,
}

impl GuiApp {
    fn new(
        state: AppState,
        locale: Option<String>,
        dry_run: bool,
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
            last_visible_rows: 0..0,
            scroll_to_current: false,
            fullscreen: false,
            active_video: None,
            video_muted: true,
            media_type_badges_visible: true,
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

    fn ensure_requested(&mut self) {
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
            if active.handle.path() != event.path {
                continue;
            }
            if let Some(error) = event.error {
                active.ended = true;
                self.status = format!("video failed: {error}");
            }
            if let Some(image) = event.frame {
                let size = [image.width() as usize, image.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
                active.texture = Some(ctx.load_texture(
                    format!("video-playback:{}", event.path.display()),
                    color,
                    egui::TextureOptions::LINEAR,
                ));
                ctx.request_repaint();
            }
            if event.ended {
                active.ended = true;
                active.handle.set_paused(true);
                self.status = "video ended".to_owned();
            }
        }
    }

    fn current_is_video(&self) -> bool {
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

    fn stop_active_video(&mut self) {
        if let Some(active) = self.active_video.take() {
            active.handle.stop();
        }
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
            self.status = if paused {
                "video paused".to_owned()
            } else {
                "video playing".to_owned()
            };
            return;
        }

        self.stop_active_video();
        let handle = video::spawn_playback(
            path.clone(),
            FIT_CAP,
            self.video_muted,
            self.video_tx.clone(),
        );
        self.active_video = Some(ActiveVideo {
            handle,
            texture: None,
            ended: false,
        });
        self.status = if self.video_muted {
            "video playing muted".to_owned()
        } else {
            "video playing with audio".to_owned()
        };
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

    fn set_current_index(&mut self, index: usize) {
        if index != self.state.current_index {
            self.stop_active_video();
        }
        self.state.current_index = index.min(self.state.entries.len().saturating_sub(1));
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
        self.stop_active_video();
        let previous = self.state.current_path();
        let result = scan_directory(ScanOptions {
            root: self.state.directory.clone(),
            recursive: self.state.recursive,
            include_hidden: self.state.include_hidden,
            extensions: self.state.extensions.clone(),
        });
        match result {
            Ok(mut entries) => {
                sorter::sort_entries(&mut entries, self.state.sort_mode, self.locale.as_deref());
                self.state.set_entries_preserving_current(entries, previous);
                if self.state.entries.is_empty() {
                    self.status = empty_media_status(&self.empty_media_target);
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
        self.scroll_to_current = true;
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        use egui::Key;
        // Confirmation modal swallows everything except y/n/esc.
        if self.state.confirm_delete {
            ctx.input(|i| {
                if i.key_pressed(Key::Y) {
                    self.confirm_delete();
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
                } else if in_grid {
                    self.state.mode = ViewMode::Preview;
                } else {
                    close = true;
                }
            }

            if in_grid {
                // Vim-style spatial navigation in the library grid:
                // h/l move within a row, j/k move between rows, ctrl+d/u half-page.
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
                let half_page = (rows / 2).max(1) as isize * cols as isize;
                if i.modifiers.ctrl && i.key_pressed(Key::D) {
                    self.move_by(half_page);
                }
                if i.modifiers.ctrl && i.key_pressed(Key::U) {
                    self.move_by(-half_page);
                }
            } else {
                // Preview is a one-wide list: h/k/←/↑ previous, l/j/→/↓ next.
                if i.key_pressed(Key::L)
                    || i.key_pressed(Key::J)
                    || i.key_pressed(Key::ArrowRight)
                    || i.key_pressed(Key::ArrowDown)
                {
                    let before = self.state.current_index;
                    self.state.next();
                    if self.state.current_index != before {
                        self.stop_active_video();
                    }
                    self.scroll_to_current = true;
                }
                if i.key_pressed(Key::H)
                    || i.key_pressed(Key::K)
                    || i.key_pressed(Key::ArrowLeft)
                    || i.key_pressed(Key::ArrowUp)
                {
                    let before = self.state.current_index;
                    self.state.previous();
                    if self.state.current_index != before {
                        self.stop_active_video();
                    }
                    self.scroll_to_current = true;
                }
            }
            if i.key_pressed(Key::Home) {
                if self.state.current_index != 0 {
                    self.stop_active_video();
                }
                self.state.first();
                self.scroll_to_current = true;
            }
            if i.key_pressed(Key::End) {
                let before = self.state.current_index;
                self.state.last();
                if self.state.current_index != before {
                    self.stop_active_video();
                }
                self.scroll_to_current = true;
            }

            // Delete queue and video playback.
            if i.modifiers.shift && i.key_pressed(Key::D) {
                self.state.enter_delete_queue_grid();
                self.scroll_to_current = true;
            } else if i.key_pressed(Key::Space) && self.current_is_video() {
                self.toggle_current_video_playback();
            } else if i.key_pressed(Key::D) && !i.modifiers.ctrl {
                self.state.toggle_queue_current();
                self.status = format!("queued: {}", self.state.queue_count());
            }
            if i.key_pressed(Key::U) && !i.modifiers.ctrl {
                self.state.unqueue_current();
                self.status = format!("queued: {}", self.state.queue_count());
            }
            if i.key_pressed(Key::R) {
                if i.modifiers.ctrl {
                    // Ctrl+R: delete the queued files (with confirmation).
                    if self.state.queue_count() == 0 {
                        self.status = "delete queue is empty".to_owned();
                    } else {
                        self.state.confirm_delete = true;
                    }
                } else if i.modifiers.shift {
                    // Shift+R: rescan the directory.
                    self.pending_rescan = true;
                } else {
                    // r: toggle recursive scanning, then rescan.
                    self.state.recursive = !self.state.recursive;
                    self.pending_rescan = true;
                }
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
            if i.key_pressed(Key::F) {
                toggle_fullscreen = true;
            }
            if i.key_pressed(Key::M) {
                self.toggle_video_mute();
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
            if i.key_pressed(Key::Questionmark) {
                self.state.show_help_overlay = !self.state.show_help_overlay;
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

        if let Some(handle) = self
            .active_video_for(&entry.path)
            .and_then(|active| active.texture.as_ref())
            .cloned()
            .or_else(|| self.previews.get(&key).cloned())
        {
            let avail = ui.available_size();
            let tex = handle.size_vec2();
            let scale = match self.state.zoom_mode {
                ZoomMode::Fit => (avail.x / tex.x).min(avail.y / tex.y),
                ZoomMode::OriginalPixels => 1.0,
            };
            let draw = tex * scale;
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new(&handle).fit_to_exact_size(draw));
            });
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

    // Scratch fields driven by input, applied after the input closure.
    // (declared here as struct fields below via Default-like init)
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
home / end      first / last
g               toggle grid
space           play / pause videos
d               toggle delete queue
u               unqueue current
shift+D         show delete queue
ctrl+R          delete queued (confirm)
z               toggle fit / original zoom
f               toggle fullscreen window
m               mute / unmute video audio
b               show / hide media badges
t / n           cycle time / name sort
r               toggle recursive scan
shift+R         rescan directory
i               info overlay
?               this help
q / esc         quit / close";

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_input(ctx);
        self.drain_results(ctx);
        self.drain_video_events(ctx);
        self.ensure_requested();

        let mode = match self.state.mode {
            ViewMode::Preview => "preview",
            ViewMode::Grid => "grid",
            ViewMode::DeleteQueueGrid => "delete-queue",
        };
        let zoom = match self.state.zoom_mode {
            ZoomMode::Fit => "fit",
            ZoomMode::OriginalPixels => "original",
        };
        let position = if self.state.entries.is_empty() {
            "0 / 0".to_owned()
        } else {
            format!(
                "{} / {}",
                self.state.current_index + 1,
                self.state.entries.len()
            )
        };
        let status = format!(
            "{mode}  |  {position}  |  {zoom}  |  queued: {}  |  sort: {:?}  |  audio: {}  |  {}",
            self.state.queue_count(),
            self.state.sort_mode,
            if self.video_muted { "muted" } else { "on" },
            self.status,
        );

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| ui.label(status));
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.state.mode {
            ViewMode::Preview => self.draw_preview(ui),
            ViewMode::Grid | ViewMode::DeleteQueueGrid => self.draw_grid(ui),
        });
        self.draw_overlays(ctx);

        self.scroll_to_current = false;
        if !self.inflight.is_empty()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ImageKind, VideoKind};

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
}

pub fn run(cli: Cli) -> Result<()> {
    let input = cli.path.as_deref().or(cli.directory.as_deref());
    let (directory, initial_file) = resolve_input(input)?;
    let extensions = cli.resolved_extensions();
    let sort_mode = cli.initial_sort_mode();
    let mut entries = scan_directory(ScanOptions {
        root: directory.clone(),
        recursive: cli.recursive,
        include_hidden: cli.hidden,
        extensions: extensions.clone(),
    })?;
    sorter::sort_entries(&mut entries, sort_mode, cli.locale.as_deref());
    let empty_media_target = initial_file.clone().unwrap_or_else(|| directory.clone());

    let mut state = AppState::new(
        directory,
        cli.recursive,
        cli.hidden,
        MediaMode::from(cli.media),
        extensions,
        sort_mode,
        entries,
    );
    // Start positioned on the requested file (after sorting).
    if let Some(file) = &initial_file
        && let Some(index) = state.entries.iter().position(|entry| entry.path == *file)
    {
        state.current_index = index;
    }
    tracing::debug!(
        directory = %state.directory.display(),
        file = ?initial_file,
        start_index = state.current_index,
        count = state.entries.len(),
        "opened"
    );
    let app = GuiApp::new(
        state,
        cli.locale.clone(),
        cli.dry_run_delete,
        empty_media_target,
    );

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
