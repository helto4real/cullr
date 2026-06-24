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
    state::{AppState, ViewMode, ZoomMode},
};

/// Long-edge cap for fit-to-window decodes. A fit view never needs more pixels
/// than a high-DPI monitor; the GPU handles any further downscaling.
const FIT_CAP: u32 = 3840;
/// Long-edge cap for grid thumbnails.
const THUMB_CAP: u32 = 320;
/// How many images on each side of the current one to decode ahead in preview.
const PREFETCH_RADIUS: usize = 4;
/// Resident texture budgets (previews are large, thumbnails small).
const PREVIEW_CACHE: usize = 32;
const THUMB_CACHE: usize = 512;
/// Grid cell edge in points.
const CELL: f32 = 168.0;

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
                    let image =
                        decode_rgba_capped(&job.key.path, job.key.variant.cap()).map_err(|e| format!("{e:#}"));
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
    previews: LruCache<TexKey, egui::TextureHandle>,
    thumbs: LruCache<TexKey, egui::TextureHandle>,
    inflight: HashSet<TexKey>,
    failed: HashMap<TexKey, String>,
    locale: Option<String>,
    dry_run: bool,
    status: String,
    grid_cols: usize,
    grid_rows: usize,
    scroll_to_current: bool,
    fullscreen: bool,
    // Deferred from inside the input closure (which borrows ctx immutably).
    pending_sort: Option<crate::state::SortMode>,
    pending_enrich: bool,
    pending_rescan: bool,
}

impl GuiApp {
    fn new(state: AppState, locale: Option<String>, dry_run: bool) -> Self {
        Self {
            state,
            decoder: DecodeService::new(),
            previews: LruCache::new(NonZeroUsize::new(PREVIEW_CACHE).unwrap()),
            thumbs: LruCache::new(NonZeroUsize::new(THUMB_CACHE).unwrap()),
            inflight: HashSet::new(),
            failed: HashMap::new(),
            locale,
            dry_run,
            status: String::new(),
            grid_cols: 1,
            grid_rows: 1,
            scroll_to_current: false,
            fullscreen: false,
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
        self.state.current_index = target as usize;
        self.scroll_to_current = true;
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
                        self.request(TexKey { path, variant });
                    }
                }
            }
            ViewMode::Grid | ViewMode::DeleteQueueGrid => {
                for path in self.visible_grid_paths() {
                    self.request(TexKey {
                        path,
                        variant: Variant::Thumb,
                    });
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

    fn visible_grid_paths(&self) -> Vec<PathBuf> {
        self.grid_indices()
            .into_iter()
            .filter_map(|index| self.state.entries.get(index))
            .map(|entry| entry.path.clone())
            .collect()
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

    fn resort(&mut self, new_mode: crate::state::SortMode) {
        let previous = self.state.current_path();
        self.state.sort_mode = new_mode;
        sorter::sort_entries(&mut self.state.entries, new_mode, self.locale.as_deref());
        let entries = std::mem::take(&mut self.state.entries);
        self.state.set_entries_preserving_current(entries, previous);
        self.status = format!("sort: {:?}", new_mode);
        self.scroll_to_current = true;
    }

    fn rescan(&mut self) {
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
                self.status = format!(
                    "scanned {} images{}",
                    self.state.entries.len(),
                    if self.state.recursive { " recursively" } else { "" }
                );
                self.scroll_to_current = true;
            }
            Err(error) => {
                self.status = format!("rescan failed: {error}");
            }
        }
    }

    fn confirm_delete(&mut self) {
        self.state.confirm_delete = false;
        let report = delete::delete_queued(&mut self.state, self.dry_run);
        let verb = if report.dry_run { "would delete" } else { "deleted" };
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
        let in_grid = matches!(
            self.state.mode,
            ViewMode::Grid | ViewMode::DeleteQueueGrid
        );

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
                    self.state.next();
                    self.scroll_to_current = true;
                }
                if i.key_pressed(Key::H)
                    || i.key_pressed(Key::K)
                    || i.key_pressed(Key::ArrowLeft)
                    || i.key_pressed(Key::ArrowUp)
                {
                    self.state.previous();
                    self.scroll_to_current = true;
                }
            }
            if i.key_pressed(Key::Home) {
                self.state.first();
                self.scroll_to_current = true;
            }
            if i.key_pressed(Key::End) {
                self.state.last();
                self.scroll_to_current = true;
            }

            // Delete queue. Plain d/space toggle; shift+D opens the queue view.
            if i.modifiers.shift && i.key_pressed(Key::D) {
                self.state.enter_delete_queue_grid();
                self.scroll_to_current = true;
            } else if i.key_pressed(Key::Space) || (i.key_pressed(Key::D) && !i.modifiers.ctrl) {
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn draw_preview(&mut self, ui: &mut egui::Ui) {
        let variant = self.preview_variant();
        let Some(entry) = self.state.current_entry() else {
            ui.centered_and_justified(|ui| {
                ui.label("No images found.");
            });
            return;
        };
        let key = TexKey {
            path: entry.path.clone(),
            variant,
        };
        let name = entry.display_name.clone();

        if let Some(handle) = self.previews.get(&key) {
            let avail = ui.available_size();
            let tex = handle.size_vec2();
            let scale = match self.state.zoom_mode {
                ZoomMode::Fit => (avail.x / tex.x).min(avail.y / tex.y),
                ZoomMode::OriginalPixels => 1.0,
            };
            let draw = tex * scale;
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new(handle).fit_to_exact_size(draw));
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
            ui.centered_and_justified(|ui| {
                ui.label(if self.state.mode == ViewMode::DeleteQueueGrid {
                    "Delete queue is empty."
                } else {
                    "No images found."
                });
            });
            return;
        }

        let spacing = 8.0;
        let cols = (((ui.available_width() + spacing) / (CELL + spacing)).floor() as usize).max(1);
        let rows = (((ui.available_height() + spacing) / (CELL + spacing)).floor() as usize).max(1);
        self.grid_cols = cols;
        self.grid_rows = rows;

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("thumb_grid")
                .spacing([spacing, spacing])
                .show(ui, |ui| {
                    for (slot, &index) in indices.iter().enumerate() {
                        self.draw_cell(ui, index);
                        if (slot + 1) % cols == 0 {
                            ui.end_row();
                        }
                    }
                });
        });
    }

    fn draw_cell(&mut self, ui: &mut egui::Ui, index: usize) {
        let Some(entry) = self.state.entries.get(index) else {
            return;
        };
        let path = entry.path.clone();
        let is_current = index == self.state.current_index;
        let is_queued = self.state.delete_queue.contains(&path);
        let key = TexKey {
            path: path.clone(),
            variant: Variant::Thumb,
        };

        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(CELL, CELL), egui::Sense::click());

        if let Some(handle) = self.thumbs.get(&key) {
            let tex = handle.size_vec2();
            let scale = ((CELL - 8.0) / tex.x).min((CELL - 8.0) / tex.y);
            let draw = tex * scale;
            let image_rect = egui::Rect::from_center_size(rect.center(), draw);
            egui::Image::new(handle).paint_at(ui, image_rect);
        } else {
            ui.painter().rect_filled(
                rect,
                4.0,
                ui.visuals().extreme_bg_color,
            );
        }

        let stroke = if is_current {
            egui::Stroke::new(3.0, egui::Color32::from_rgb(240, 200, 0))
        } else if is_queued {
            egui::Stroke::new(2.0, egui::Color32::from_rgb(220, 60, 60))
        } else {
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
        };
        ui.painter().rect_stroke(rect, 4.0, stroke, egui::StrokeKind::Inside);

        if is_queued {
            ui.painter().text(
                rect.left_top() + egui::vec2(6.0, 6.0),
                egui::Align2::LEFT_TOP,
                "DEL",
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(220, 60, 60),
            );
        }

        if response.clicked() {
            self.state.current_index = index;
        }
        if response.double_clicked() {
            self.state.current_index = index;
            self.state.mode = ViewMode::Preview;
        }
        if is_current && self.scroll_to_current {
            response.scroll_to_me(Some(egui::Align::Center));
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
            let kind = entry
                .image_type
                .as_ref()
                .map(|k| k.as_str().to_owned())
                .unwrap_or_else(|| "—".to_owned());
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
  h / l           left / right one image
  j / k           down / up one row
  ctrl+d / ctrl+u half page down / up
  enter           open highlighted
preview mode:
  h / k / ← / ↑   previous
  l / j / → / ↓   next
home / end      first / last
g               toggle grid
space / d       toggle delete queue
u               unqueue current
shift+D         show delete queue
ctrl+R          delete queued (confirm)
z               toggle fit / original zoom
f               toggle fullscreen window
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
            format!("{} / {}", self.state.current_index + 1, self.state.entries.len())
        };
        let status = format!(
            "{mode}  |  {position}  |  {zoom}  |  queued: {}  |  sort: {:?}  |  {}",
            self.state.queue_count(),
            self.state.sort_mode,
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
        if !self.inflight.is_empty() {
            ctx.request_repaint();
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    let directory = resolve_directory(cli.directory.as_deref())?;
    let extensions = cli.resolved_extensions();
    let sort_mode = cli.initial_sort_mode();
    let mut entries = scan_directory(ScanOptions {
        root: directory.clone(),
        recursive: cli.recursive,
        include_hidden: cli.hidden,
        extensions: extensions.clone(),
    })?;
    sorter::sort_entries(&mut entries, sort_mode, cli.locale.as_deref());

    if entries.is_empty() {
        println!("No images found in {}", directory.display());
        return Ok(());
    }

    let state = AppState::new(directory, cli.recursive, cli.hidden, extensions, sort_mode, entries);
    let app = GuiApp::new(state, cli.locale.clone(), cli.dry_run_delete);

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

fn resolve_directory(directory: Option<&Path>) -> Result<PathBuf> {
    let path = match directory {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    if !canonical.is_dir() {
        return Err(anyhow!("{} is not a directory", canonical.display()));
    }
    Ok(canonical)
}
