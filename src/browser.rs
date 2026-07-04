//! File browser pane: directory listing, entry classification, selection
//! state, and the pane's icons. The GUI keeps the pane layout and input
//! routing; everything browser-specific that doesn't need `GuiApp` lives here.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use eframe::egui;

use crate::{
    scanner,
    state::{AppState, BrowserEntry, BrowserEntryKind, BrowserPaneFocus, BrowserState, MediaKind, ViewMode},
};

pub(crate) const BROWSER_DEFAULT_WIDTH: f32 = 320.0;
pub(crate) const BROWSER_ROW_HEIGHT: f32 = 26.0;

impl BrowserState {
    pub(crate) fn for_directory(
        listed_directory: PathBuf,
        preferred_selection: Option<PathBuf>,
        mut remembered_selection: HashMap<PathBuf, PathBuf>,
        include_hidden: bool,
        extensions: &[String],
    ) -> Result<Self> {
        let entries = read_browser_entries(&listed_directory, include_hidden, extensions)?;
        let remembered = remembered_selection.get(&listed_directory).cloned();
        let selected_index = preferred_browser_index(
            &entries,
            preferred_selection.as_ref().or(remembered.as_ref()),
            0,
        );
        if let Some(entry) = entries.get(selected_index) {
            remembered_selection.insert(listed_directory.clone(), entry.path.clone());
        }
        Ok(Self {
            listed_directory,
            entries,
            selected_index,
            focus: BrowserPaneFocus::Browser,
            remembered_selection,
            scroll_to_selection: true,
        })
    }

    /// Browser state for whatever the app is currently looking at: the current
    /// file in preview mode, otherwise the scanned directory.
    pub(crate) fn for_current_view(state: &AppState) -> Result<Self> {
        let target = if matches!(state.mode, ViewMode::Preview) {
            state
                .current_path()
                .unwrap_or_else(|| state.directory.clone())
        } else {
            state.directory.clone()
        };
        let listed_directory = listed_directory_for_target(&target);
        Self::for_directory(
            listed_directory,
            Some(target),
            HashMap::new(),
            state.include_hidden,
            &state.extensions,
        )
    }

    pub(crate) fn selected_path(&self) -> Option<PathBuf> {
        self.entries
            .get(self.selected_index)
            .map(|entry| entry.path.clone())
    }

    pub(crate) fn remember_current_selection(&mut self) {
        if let Some(path) = self.selected_path() {
            self.remembered_selection
                .insert(self.listed_directory.clone(), path);
        }
    }
}

pub(crate) fn listed_directory_for_target(target: &Path) -> PathBuf {
    target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| target.to_path_buf())
}

pub(crate) fn read_browser_entries(
    directory: &Path,
    include_hidden: bool,
    extensions: &[String],
) -> Result<Vec<BrowserEntry>> {
    let mut entries = Vec::new();
    for dir_entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let dir_entry = match dir_entry {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "skipping unreadable browser entry");
                continue;
            }
        };
        let path = dir_entry.path();
        if !include_hidden && is_hidden_browser_path(&path) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping unreadable browser metadata");
                continue;
            }
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        let kind = if file_type.is_dir() {
            BrowserEntryKind::Directory
        } else if file_type.is_file() {
            browser_file_kind(&path, extensions)
        } else {
            continue;
        };
        let display_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        entries.push(BrowserEntry {
            path,
            display_name,
            kind,
        });
    }
    sort_browser_entries(&mut entries);
    Ok(entries)
}

fn browser_file_kind(path: &Path, extensions: &[String]) -> BrowserEntryKind {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    let allowed = extension
        .as_ref()
        .is_some_and(|ext| extensions.iter().any(|allowed| allowed == ext));
    if allowed && let Some(media_kind) = MediaKind::from_extension(extension.as_deref()) {
        // `.ts` is ambiguous (TypeScript vs MPEG transport stream); reuse the
        // scanner's content probe so the listing matches what a scan accepts.
        if extension.as_deref() == Some("ts") && !scanner::looks_like_plain_mpeg_ts(path) {
            return BrowserEntryKind::UnsupportedFile;
        }
        BrowserEntryKind::Media(media_kind)
    } else {
        BrowserEntryKind::UnsupportedFile
    }
}

fn sort_browser_entries(entries: &mut [BrowserEntry]) {
    entries.sort_by(|a, b| {
        browser_kind_rank(&a.kind)
            .cmp(&browser_kind_rank(&b.kind))
            .then_with(|| {
                a.display_name
                    .to_ascii_lowercase()
                    .cmp(&b.display_name.to_ascii_lowercase())
            })
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
}

fn browser_kind_rank(kind: &BrowserEntryKind) -> u8 {
    match kind {
        BrowserEntryKind::Directory => 0,
        BrowserEntryKind::Media(_) => 1,
        BrowserEntryKind::UnsupportedFile => 2,
    }
}

pub(crate) fn preferred_browser_index(
    entries: &[BrowserEntry],
    preferred_path: Option<&PathBuf>,
    fallback: usize,
) -> usize {
    if entries.is_empty() {
        return 0;
    }
    preferred_path
        .and_then(|path| entries.iter().position(|entry| &entry.path == path))
        .unwrap_or_else(|| fallback.min(entries.len() - 1))
}

pub(crate) fn browser_move_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

fn is_hidden_browser_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

/// Elide `text` from the left so its tail fits in `max_width`, keeping the
/// deepest path segments visible.
pub(crate) fn left_elided_text(
    ui: &egui::Ui,
    text: &str,
    font: &egui::FontId,
    max_width: f32,
) -> String {
    let fits = |candidate: &str| text_width(ui, candidate, font) <= max_width;
    if fits(text) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    // Binary search the smallest prefix drop whose "…" + suffix fits:
    // `lo` chars dropped never fits, `hi` chars dropped always fits.
    let mut lo = 0;
    let mut hi = chars.len();
    while lo + 1 < hi {
        let mid = usize::midpoint(lo, hi);
        let candidate: String = std::iter::once('…')
            .chain(chars[mid..].iter().copied())
            .collect();
        if fits(&candidate) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    std::iter::once('…')
        .chain(chars[hi..].iter().copied())
        .collect()
}

fn text_width(ui: &egui::Ui, text: &str, font: &egui::FontId) -> f32 {
    ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
            .rect
            .width()
    })
}

pub(crate) fn draw_browser_icon(ui: &egui::Ui, rect: egui::Rect, kind: &BrowserEntryKind) {
    let painter = ui.painter();
    let stroke = egui::Stroke::new(1.3, ui.visuals().strong_text_color());
    match kind {
        BrowserEntryKind::Directory => {
            let tab = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 1.0, rect.top() + 3.0),
                egui::pos2(rect.left() + 7.0, rect.top() + 6.0),
            );
            let body = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 1.0, rect.top() + 5.0),
                egui::pos2(rect.right() - 1.0, rect.bottom() - 2.0),
            );
            painter.rect_filled(tab, 1.0, egui::Color32::from_rgb(220, 180, 72));
            painter.rect_filled(body, 2.0, egui::Color32::from_rgb(216, 165, 56));
            painter.rect_stroke(body, 2.0, stroke, egui::StrokeKind::Inside);
        }
        BrowserEntryKind::Media(media_kind) if media_kind.is_video() => {
            painter.rect_stroke(rect.shrink(1.5), 2.0, stroke, egui::StrokeKind::Inside);
            let center = rect.center();
            let points = vec![
                egui::pos2(center.x - 3.0, center.y - 5.0),
                egui::pos2(center.x - 3.0, center.y + 5.0),
                egui::pos2(center.x + 5.0, center.y),
            ];
            painter.add(egui::Shape::convex_polygon(
                points,
                ui.visuals().strong_text_color(),
                egui::Stroke::NONE,
            ));
        }
        BrowserEntryKind::Media(_) => {
            let frame = rect.shrink(1.5);
            painter.rect_stroke(frame, 2.0, stroke, egui::StrokeKind::Inside);
            painter.circle_filled(
                egui::pos2(frame.left() + 4.0, frame.top() + 4.0),
                1.5,
                ui.visuals().strong_text_color(),
            );
            painter.line_segment(
                [
                    egui::pos2(frame.left() + 3.0, frame.bottom() - 3.0),
                    egui::pos2(frame.center().x - 1.0, frame.center().y + 1.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(frame.center().x - 1.0, frame.center().y + 1.0),
                    egui::pos2(frame.right() - 3.0, frame.bottom() - 3.0),
                ],
                stroke,
            );
        }
        BrowserEntryKind::UnsupportedFile => {
            let page = rect.shrink(1.5);
            painter.rect_stroke(page, 1.5, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(page.left() + 3.0, page.center().y),
                    egui::pos2(page.right() - 3.0, page.center().y),
                ],
                stroke,
            );
        }
    }
}

pub(crate) fn draw_unsupported_file_message(ui: &mut egui::Ui, target: &Path) {
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("Unsupported file").strong().size(20.0));
            ui.add_space(8.0);
            ui.label(egui::RichText::new(target.display().to_string()).monospace());
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    fn touch(path: &Path) {
        fs::write(path, b"not actually decoded").unwrap();
    }

    fn write_mpeg_ts(path: &Path) {
        const PACKET: usize = 188;
        let mut bytes = vec![0; PACKET * 3];
        for offset in (0..bytes.len()).step_by(PACKET) {
            bytes[offset] = 0x47;
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn browser_listing_sorts_folders_media_and_unsupported_files() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("folder")).unwrap();
        touch(&temp.path().join("image.jpg"));
        touch(&temp.path().join("clip.mp4"));
        touch(&temp.path().join("note.txt"));

        let entries =
            read_browser_entries(temp.path(), false, &["jpg".to_owned(), "mp4".to_owned()])
                .unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["folder", "clip.mp4", "image.jpg", "note.txt"]
        );
        assert_eq!(entries[0].kind, BrowserEntryKind::Directory);
        assert!(matches!(entries[1].kind, BrowserEntryKind::Media(_)));
        assert_eq!(entries[3].kind, BrowserEntryKind::UnsupportedFile);
    }

    #[test]
    fn browser_listing_respects_hidden_toggle() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join(".hidden.jpg"));
        touch(&temp.path().join("visible.jpg"));

        let hidden_off = read_browser_entries(temp.path(), false, &["jpg".to_owned()]).unwrap();
        let hidden_on = read_browser_entries(temp.path(), true, &["jpg".to_owned()]).unwrap();

        assert_eq!(hidden_off.len(), 1);
        assert_eq!(hidden_off[0].display_name, "visible.jpg");
        assert!(
            hidden_on
                .iter()
                .any(|entry| entry.display_name == ".hidden.jpg")
        );
    }

    #[test]
    fn browser_listing_probes_ambiguous_ts_files() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("code.ts");
        let video = temp.path().join("clip.ts");
        fs::write(&source, "export const value = 42;\n".repeat(40)).unwrap();
        write_mpeg_ts(&video);

        let entries = read_browser_entries(temp.path(), false, &["ts".to_owned()]).unwrap();

        let kind_of = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.display_name == name)
                .map(|entry| entry.kind.clone())
                .unwrap()
        };
        assert!(matches!(kind_of("clip.ts"), BrowserEntryKind::Media(_)));
        assert_eq!(kind_of("code.ts"), BrowserEntryKind::UnsupportedFile);
    }

    #[test]
    fn browser_navigation_clamps_to_visible_range() {
        assert_eq!(browser_move_index(0, -1, 4), 0);
        assert_eq!(browser_move_index(1, 2, 4), 3);
        assert_eq!(browser_move_index(3, 8, 4), 3);
        assert_eq!(browser_move_index(0, 1, 0), 0);
    }

    #[test]
    fn browser_state_restores_remembered_child_selection() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("a.jpg");
        let second = temp.path().join("b.jpg");
        touch(&first);
        touch(&second);
        let directory = temp.path().canonicalize().unwrap();
        let remembered = [(directory.clone(), second.canonicalize().unwrap())]
            .into_iter()
            .collect();

        let browser =
            BrowserState::for_directory(directory, None, remembered, false, &["jpg".to_owned()])
                .unwrap();

        assert_eq!(
            browser.selected_path(),
            Some(second.canonicalize().unwrap())
        );
    }
}
