use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event};

use crate::{
    cli::{BackendChoice, Cli},
    delete,
    input::{Action, action_for_key},
    metadata,
    renderer::{ImageRenderer, NativeRatatuiImageRenderer, chafa},
    scanner::{ScanOptions, scan_directory},
    sorter,
    state::{AppState, SortMode, ViewMode, ZoomMode},
    thumbnail::ThumbnailService,
    ui,
};

pub struct App {
    cli: Cli,
    pub state: AppState,
    renderer: NativeRatatuiImageRenderer,
    thumbnails: ThumbnailService,
}

impl App {
    pub fn new(cli: Cli) -> Result<Self> {
        let directory = resolve_directory(cli.directory.as_deref())?;
        let extensions = cli.resolved_extensions();
        let sort_mode = cli.initial_sort_mode();
        let mut entries = scan_entries(
            &directory,
            cli.recursive,
            cli.hidden,
            extensions.clone(),
            sort_mode,
            cli.locale.as_deref(),
        )?;

        sorter::sort_entries(&mut entries, sort_mode, cli.locale.as_deref());
        let state = AppState::new(
            directory,
            cli.recursive,
            cli.hidden,
            extensions,
            sort_mode,
            entries,
        );
        let renderer = NativeRatatuiImageRenderer::new(
            cli.backend,
            cli.allow_symbol_fallback || cli.backend == BackendChoice::Chafa,
        );
        let thumbnails = ThumbnailService::new(cli.cache_mb);

        Ok(Self {
            cli,
            state,
            renderer,
            thumbnails,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        if self.state.entries.is_empty() {
            println!("No images found in {}", self.state.directory.display());
            return Ok(());
        }
        if !std::io::stdout().is_terminal() {
            return Err(anyhow!("stdout is not an interactive terminal"));
        }
        let _size = crossterm::terminal::size().context("failed to query terminal size")?;

        let mut terminal = ratatui::init();
        let result = (|| {
            terminal.hide_cursor()?;
            self.preflight_renderer()?;
            self.run_loop(&mut terminal)
        })();
        ratatui::restore();
        result
    }

    fn preflight_renderer(&mut self) -> Result<()> {
        if self.cli.backend == BackendChoice::Chafa {
            match chafa::preflight() {
                Ok(version) => {
                    self.state.status_message =
                        Some(format!("{version}; drawing via native TUI path"));
                }
                Err(error) => {
                    return Err(error.context("forced --backend chafa failed preflight"));
                }
            }
        } else if self.cli.backend == BackendChoice::Auto && chafa::is_available() {
            tracing::debug!("chafa is available as an external fallback");
        }

        let capabilities = self.renderer.preflight()?;
        self.thumbnails.configure_renderer(
            self.renderer.picker_clone()?,
            self.renderer.backend_id().to_owned(),
        );
        if !capabilities.graphics_protocol
            && !self.cli.allow_symbol_fallback
            && self.cli.backend != BackendChoice::Chafa
        {
            return Err(anyhow!(
                "no graphics-capable terminal backend was detected; pass --allow-symbol-fallback to use halfblocks"
            ));
        }
        Ok(())
    }

    fn run_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut needs_draw = true;
        loop {
            if self
                .thumbnails
                .poll_finished(self.state.thumbnail_generation)
            {
                needs_draw = true;
            }

            if needs_draw {
                terminal.draw(|frame| {
                    ui::draw(
                        frame,
                        &mut self.state,
                        &mut self.renderer,
                        &mut self.thumbnails,
                    )
                })?;
                needs_draw = false;
            }

            if event::poll(self.event_poll_timeout())? {
                match event::read()? {
                    Event::Key(key) => {
                        let action = action_for_key(key, self.state.confirm_delete);
                        if action != Action::Noop {
                            if !self.handle_action(action)? {
                                break;
                            }
                            needs_draw = true;
                        }
                    }
                    Event::Resize(_, _) => {
                        self.state.bump_generation();
                        self.state.forget_render_layout();
                        self.renderer.clear()?;
                        self.thumbnails.clear_for_new_generation();
                        self.state.status_message = Some("terminal resized".to_owned());
                        needs_draw = true;
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn event_poll_timeout(&self) -> Duration {
        if self.thumbnails.has_inflight() {
            Duration::from_millis(5)
        } else {
            Duration::from_millis(250)
        }
    }

    fn handle_action(&mut self, action: Action) -> Result<bool> {
        let previous_index = self.state.current_index;
        let previous_mode = self.state.mode;
        let previous_generation = self.state.thumbnail_generation;

        match action {
            Action::Quit => return Ok(false),
            Action::Next => self.state.next(),
            Action::Previous => self.state.previous(),
            Action::First => self.state.first(),
            Action::Last => self.state.last(),
            Action::ToggleQueueCurrent => {
                self.state.toggle_queue_current();
                self.state.status_message = Some(format!("queued: {}", self.state.queue_count()));
            }
            Action::UnqueueCurrent => {
                self.state.unqueue_current();
                self.state.status_message = Some(format!("queued: {}", self.state.queue_count()));
            }
            Action::ShowDeleteQueueGrid => {
                self.state.enter_delete_queue_grid();
                if self.state.queue_count() == 0 {
                    self.state.status_message = Some("delete queue is empty".to_owned());
                }
            }
            Action::ToggleGrid => {
                self.state.mode = match self.state.mode {
                    ViewMode::Preview => ViewMode::Grid,
                    ViewMode::Grid | ViewMode::DeleteQueueGrid => ViewMode::Preview,
                };
            }
            Action::OpenHighlighted => {
                self.state.mode = ViewMode::Preview;
            }
            Action::PageDown => self.state.page_by(1),
            Action::PageUp => self.state.page_by(-1),
            Action::ConfirmDeleteQueued => {
                if self.state.queue_count() == 0 {
                    self.state.status_message = Some("delete queue is empty".to_owned());
                } else {
                    self.state.confirm_delete = true;
                }
            }
            Action::ConfirmYes => {
                self.state.confirm_delete = false;
                let report = delete::delete_queued(&mut self.state, self.cli.dry_run_delete);
                if report.failed.is_empty() {
                    let verb = if report.dry_run {
                        "would delete"
                    } else {
                        "deleted"
                    };
                    self.state.status_message =
                        Some(format!("{verb} {} files", report.deleted.len()));
                } else {
                    self.state.status_message = Some(format!(
                        "deleted {}; failed {}",
                        report.deleted.len(),
                        report.failed.len()
                    ));
                }
                self.renderer.clear()?;
                self.thumbnails.clear_for_new_generation();
            }
            Action::ConfirmNo => {
                self.state.confirm_delete = false;
                self.state.status_message = Some("delete cancelled".to_owned());
            }
            Action::ToggleFullscreenUi => {
                self.state.fullscreen_ui = !self.state.fullscreen_ui;
                self.state.bump_generation();
                self.state.forget_render_layout();
                self.renderer.clear()?;
                self.thumbnails.clear_for_new_generation();
            }
            Action::ToggleRecursive => {
                self.state.recursive = !self.state.recursive;
                self.rescan_preserving_current()?;
            }
            Action::Rescan => {
                self.rescan_preserving_current()?;
            }
            Action::ToggleTimeSort => {
                self.state.sort_mode = sorter::next_time_sort(self.state.sort_mode);
                self.resort_preserving_current();
            }
            Action::ToggleNameSort => {
                self.state.sort_mode = sorter::next_name_sort(self.state.sort_mode);
                self.resort_preserving_current();
            }
            Action::ToggleInfoOverlay => {
                self.state.show_info_overlay = !self.state.show_info_overlay;
                if self.state.show_info_overlay {
                    self.enrich_current_entry();
                }
            }
            Action::ToggleHelpOverlay => {
                self.state.show_help_overlay = !self.state.show_help_overlay;
            }
            Action::ToggleZoom => {
                self.state.zoom_mode = match self.state.zoom_mode {
                    ZoomMode::Fit => ZoomMode::OriginalPixels,
                    ZoomMode::OriginalPixels => ZoomMode::Fit,
                };
                self.state.bump_generation();
                self.renderer.clear()?;
                self.thumbnails.clear_for_new_generation();
            }
            Action::Noop => {}
        }

        if self.state.current_index != previous_index
            || self.state.mode != previous_mode
            || self.state.thumbnail_generation != previous_generation
        {
            self.prefetch_current_from_last_known_area();
        }

        Ok(true)
    }

    fn enrich_current_entry(&mut self) {
        if let Some(entry) = self.state.current_entry_mut() {
            let started = Instant::now();
            metadata::enrich_entry(entry);
            tracing::debug!(
                path = %entry.path.display(),
                metadata_ms = started.elapsed().as_millis(),
                "enriched current image metadata"
            );
        }
    }

    fn prefetch_current_from_last_known_area(&mut self) {
        match self.state.mode {
            ViewMode::Preview => self.prefetch_current_preview_from_last_size(),
            ViewMode::Grid | ViewMode::DeleteQueueGrid => {
                self.prefetch_current_thumbnail_from_last_cell();
            }
        }
    }

    fn prefetch_current_preview_from_last_size(&mut self) {
        let Some((width, height)) = self.state.last_preview_size else {
            return;
        };
        let Some(entry) = self.state.current_entry().cloned() else {
            return;
        };
        let _ = self.thumbnails.prefetch_preview(
            &entry,
            width,
            height,
            self.state.zoom_mode,
            self.state.thumbnail_generation,
        );
    }

    fn prefetch_current_thumbnail_from_last_cell(&mut self) {
        let Some((width, height)) = self.state.last_grid_cell_size else {
            return;
        };
        let Some(entry) = self.state.current_entry().cloned() else {
            return;
        };
        let _ = self.thumbnails.prefetch_thumbnail(
            &entry,
            width,
            height,
            self.state.thumbnail_generation,
        );
    }

    fn rescan_preserving_current(&mut self) -> Result<()> {
        let previous = self.state.current_path();
        let entries = scan_entries(
            &self.state.directory,
            self.state.recursive,
            self.state.include_hidden,
            self.state.extensions.clone(),
            self.state.sort_mode,
            self.cli.locale.as_deref(),
        )?;
        self.state.set_entries_preserving_current(entries, previous);
        self.state.status_message = Some(format!(
            "scanned {} images{}",
            self.state.entries.len(),
            if self.state.recursive {
                " recursively"
            } else {
                ""
            }
        ));
        self.renderer.clear()?;
        self.thumbnails.clear_for_new_generation();
        Ok(())
    }

    fn resort_preserving_current(&mut self) {
        let previous = self.state.current_path();
        sorter::sort_entries(
            &mut self.state.entries,
            self.state.sort_mode,
            self.cli.locale.as_deref(),
        );
        let entries = std::mem::take(&mut self.state.entries);
        self.state.set_entries_preserving_current(entries, previous);
        self.state.status_message = Some(format!("sort: {:?}", self.state.sort_mode));
        let _ = self.renderer.clear();
        self.thumbnails.clear_for_new_generation();
    }
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

fn scan_entries(
    directory: &Path,
    recursive: bool,
    include_hidden: bool,
    extensions: Vec<String>,
    sort_mode: SortMode,
    locale: Option<&str>,
) -> Result<Vec<crate::state::ImageEntry>> {
    let started = Instant::now();
    let mut entries = scan_directory(ScanOptions {
        root: directory.to_path_buf(),
        recursive,
        include_hidden,
        extensions,
    })?;
    sorter::sort_entries(&mut entries, sort_mode, locale);
    tracing::debug!(
        directory = %directory.display(),
        entries = entries.len(),
        recursive,
        scan_ms = started.elapsed().as_millis(),
        "scanned image entries"
    );
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli::BackendChoice, input::Action};
    use ratatui_image::picker::Picker;
    use std::fs;
    use tempfile::tempdir;

    fn cli_for(path: &Path) -> Cli {
        Cli {
            directory: Some(path.to_path_buf()),
            recursive: false,
            file_ext: Some("jpg".to_owned()),
            sort: None,
            backend: BackendChoice::Auto,
            allow_symbol_fallback: true,
            locale: None,
            cache_mb: 1,
            dry_run_delete: false,
            hidden: false,
        }
    }

    fn app_with_files() -> App {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("a.jpg"), b"not decoded in this test").unwrap();
        fs::write(temp.path().join("b.jpg"), b"not decoded in this test").unwrap();
        App::new(cli_for(temp.path())).unwrap()
    }

    #[test]
    fn ordinary_navigation_does_not_enrich_metadata() {
        let mut app = app_with_files();

        app.handle_action(Action::Next).unwrap();

        let entry = app.state.current_entry().unwrap();
        assert!(!entry.dimensions_attempted);
        assert!(!entry.exif_attempted);
    }

    #[test]
    fn navigation_prefetches_current_preview_when_layout_is_known() {
        let mut app = app_with_files();
        app.thumbnails
            .configure_renderer(Picker::halfblocks(), "native:Halfblocks".to_owned());
        app.state.last_preview_size = Some((40, 20));

        app.handle_action(Action::Next).unwrap();

        assert_eq!(app.thumbnails.preview_inflight_len(), 1);
    }
}
