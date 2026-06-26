use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::SystemTime,
};

use indexmap::IndexSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaEntry {
    pub path: PathBuf,
    pub file_name: OsString,
    pub display_name: String,
    pub extension: Option<String>,
    pub file_len: u64,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub discovered_order: usize,
    pub dimensions: Option<(u32, u32)>,
    pub media_kind: MediaKind,
    pub exif_date: Option<SystemTime>,
    pub exif_orientation: Option<u16>,
    pub dimensions_attempted: bool,
    pub exif_attempted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    Both,
    Image,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image(ImageKind),
    Video(VideoKind),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageKind {
    Jpeg,
    Png,
    WebP,
    Gif,
    Bmp,
    Tiff,
    Avif,
    Qoi,
    Ico,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VideoKind {
    Mp4,
    M4v,
    Mov,
    Mkv,
    WebM,
    Avi,
    Mpeg,
    M2v,
    TransportStream,
    Wmv,
    Flv,
    ThreeGp,
    Ogv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Preview,
    Grid,
    DeleteQueueGrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Discovered,
    Newest,
    Oldest,
    NameAsc,
    NameDesc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZoomMode {
    Fit,
    OriginalPixels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewPrefetchMarker {
    pub generation: u64,
    pub width_cells: u16,
    pub height_cells: u16,
    pub zoom_mode: ZoomMode,
    pub entry_count: usize,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub directory: PathBuf,
    pub recursive: bool,
    pub include_hidden: bool,
    pub media_mode: MediaMode,
    pub extensions: Vec<String>,
    pub entries: Vec<MediaEntry>,
    pub current_index: usize,
    pub mode: ViewMode,
    pub sort_mode: SortMode,
    pub zoom_mode: ZoomMode,
    pub delete_queue: IndexSet<PathBuf>,
    pub show_info_overlay: bool,
    pub show_help_overlay: bool,
    pub fullscreen_ui: bool,
    pub grid_page: usize,
    pub status_message: Option<String>,
    pub confirm_delete: bool,
    pub thumbnail_generation: u64,
    pub last_grid_page_size: usize,
    pub last_preview_size: Option<(u16, u16)>,
    pub last_grid_cell_size: Option<(u16, u16)>,
    pub eager_preview_prefetch: Option<PreviewPrefetchMarker>,
}

impl AppState {
    pub fn new(
        directory: PathBuf,
        recursive: bool,
        include_hidden: bool,
        media_mode: MediaMode,
        extensions: Vec<String>,
        sort_mode: SortMode,
        entries: Vec<MediaEntry>,
    ) -> Self {
        Self {
            directory,
            recursive,
            include_hidden,
            media_mode,
            extensions,
            entries,
            current_index: 0,
            mode: ViewMode::Preview,
            sort_mode,
            zoom_mode: ZoomMode::Fit,
            delete_queue: IndexSet::new(),
            show_info_overlay: false,
            show_help_overlay: false,
            fullscreen_ui: false,
            grid_page: 0,
            status_message: None,
            confirm_delete: false,
            thumbnail_generation: 0,
            last_grid_page_size: 1,
            last_preview_size: None,
            last_grid_cell_size: None,
            eager_preview_prefetch: None,
        }
    }

    pub fn current_entry(&self) -> Option<&MediaEntry> {
        self.entries.get(self.current_index)
    }

    pub fn current_entry_mut(&mut self) -> Option<&mut MediaEntry> {
        self.entries.get_mut(self.current_index)
    }

    pub fn current_path(&self) -> Option<PathBuf> {
        self.current_entry().map(|entry| entry.path.clone())
    }

    pub fn queue_count(&self) -> usize {
        self.delete_queue.len()
    }

    pub fn is_queued<P: AsRef<Path>>(&self, path: P) -> bool {
        self.delete_queue.contains(path.as_ref())
    }

    pub fn toggle_queue_current(&mut self) {
        if let Some(path) = self.current_path() {
            if !self.delete_queue.shift_remove(&path) {
                self.delete_queue.insert(path);
            }
        }
    }

    pub fn unqueue_current(&mut self) {
        if let Some(path) = self.current_path() {
            self.delete_queue.shift_remove(&path);
        }
    }

    pub fn next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.mode == ViewMode::DeleteQueueGrid {
            self.move_in_queue(1);
            return;
        }
        self.current_index = (self.current_index + 1).min(self.entries.len() - 1);
        self.sync_grid_page();
    }

    pub fn previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.mode == ViewMode::DeleteQueueGrid {
            self.move_in_queue(-1);
            return;
        }
        self.current_index = self.current_index.saturating_sub(1);
        self.sync_grid_page();
    }

    pub fn first(&mut self) {
        self.current_index = 0;
        self.sync_grid_page();
    }

    pub fn last(&mut self) {
        if !self.entries.is_empty() {
            self.current_index = self.entries.len() - 1;
            self.sync_grid_page();
        }
    }

    pub fn page_by(&mut self, delta_pages: isize) {
        let page_size = self.last_grid_page_size.max(1);
        if self.mode == ViewMode::DeleteQueueGrid {
            self.page_queue_by(delta_pages, page_size);
            return;
        }
        if self.entries.is_empty() {
            return;
        }
        let delta = delta_pages.unsigned_abs().saturating_mul(page_size);
        if delta_pages.is_negative() {
            self.current_index = self.current_index.saturating_sub(delta);
        } else {
            self.current_index = (self.current_index + delta).min(self.entries.len() - 1);
        }
        self.sync_grid_page();
    }

    pub fn set_entries_preserving_current(
        &mut self,
        entries: Vec<MediaEntry>,
        previous: Option<PathBuf>,
    ) {
        self.entries = entries;
        self.delete_queue
            .retain(|path| self.entries.iter().any(|entry| &entry.path == path));

        self.current_index = previous
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.current_index.min(self.entries.len().saturating_sub(1)));
        self.sync_grid_page();
        self.bump_generation();
    }

    pub fn clamp_current_index(&mut self) {
        if self.entries.is_empty() {
            self.current_index = 0;
        } else {
            self.current_index = self.current_index.min(self.entries.len() - 1);
        }
        self.sync_grid_page();
    }

    pub fn bump_generation(&mut self) {
        self.thumbnail_generation = self.thumbnail_generation.wrapping_add(1);
        self.eager_preview_prefetch = None;
    }

    pub fn forget_render_layout(&mut self) {
        self.last_preview_size = None;
        self.last_grid_cell_size = None;
        self.eager_preview_prefetch = None;
    }

    pub fn queued_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| self.delete_queue.contains(&entry.path).then_some(index))
            .collect()
    }

    pub fn queue_position(&self) -> Option<usize> {
        self.queued_indices()
            .iter()
            .position(|&index| index == self.current_index)
    }

    pub fn enter_delete_queue_grid(&mut self) {
        self.mode = ViewMode::DeleteQueueGrid;
        if !self.delete_queue.is_empty() && !self.is_queued_current() {
            if let Some(first_index) = self.queued_indices().first().copied() {
                self.current_index = first_index;
            }
        }
        self.sync_grid_page();
    }

    pub fn is_queued_current(&self) -> bool {
        self.current_entry()
            .map(|entry| self.delete_queue.contains(&entry.path))
            .unwrap_or(false)
    }

    pub fn sync_grid_page(&mut self) {
        self.grid_page = if self.last_grid_page_size == 0 {
            0
        } else {
            self.current_index / self.last_grid_page_size.max(1)
        };
    }

    fn move_in_queue(&mut self, delta: isize) {
        let indices = self.queued_indices();
        if indices.is_empty() {
            return;
        }
        let current = indices
            .iter()
            .position(|&index| index == self.current_index)
            .unwrap_or(0);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            (current + delta as usize).min(indices.len() - 1)
        };
        self.current_index = indices[next];
        self.sync_grid_page();
    }

    fn page_queue_by(&mut self, delta_pages: isize, page_size: usize) {
        let indices = self.queued_indices();
        if indices.is_empty() {
            return;
        }
        let current = indices
            .iter()
            .position(|&index| index == self.current_index)
            .unwrap_or(0);
        let delta = delta_pages.unsigned_abs().saturating_mul(page_size);
        let next = if delta_pages.is_negative() {
            current.saturating_sub(delta)
        } else {
            (current + delta).min(indices.len() - 1)
        };
        self.current_index = indices[next];
        self.sync_grid_page();
    }
}

impl MediaKind {
    pub fn from_extension(ext: Option<&str>) -> Option<Self> {
        ImageKind::from_extension(ext)
            .map(Self::Image)
            .or_else(|| VideoKind::from_extension(ext).map(Self::Video))
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image(_))
    }

    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video(_))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Image(kind) => kind.as_str(),
            Self::Video(kind) => kind.as_str(),
        }
    }
}

impl ImageKind {
    pub fn from_extension(ext: Option<&str>) -> Option<Self> {
        match ext?.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "webp" => Some(Self::WebP),
            "gif" => Some(Self::Gif),
            "bmp" => Some(Self::Bmp),
            "tif" | "tiff" => Some(Self::Tiff),
            "avif" => Some(Self::Avif),
            "qoi" => Some(Self::Qoi),
            "ico" => Some(Self::Ico),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::WebP => "WebP",
            Self::Gif => "GIF",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
            Self::Avif => "AVIF",
            Self::Qoi => "QOI",
            Self::Ico => "ICO",
        }
    }
}

impl VideoKind {
    pub fn from_extension(ext: Option<&str>) -> Option<Self> {
        match ext?.to_ascii_lowercase().as_str() {
            "mp4" => Some(Self::Mp4),
            "m4v" => Some(Self::M4v),
            "mov" => Some(Self::Mov),
            "mkv" => Some(Self::Mkv),
            "webm" => Some(Self::WebM),
            "avi" => Some(Self::Avi),
            "mpg" | "mpeg" => Some(Self::Mpeg),
            "m2v" => Some(Self::M2v),
            "ts" | "m2ts" | "mts" => Some(Self::TransportStream),
            "wmv" => Some(Self::Wmv),
            "flv" => Some(Self::Flv),
            "3gp" | "3g2" => Some(Self::ThreeGp),
            "ogv" => Some(Self::Ogv),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Mp4 => "MP4",
            Self::M4v => "M4V",
            Self::Mov => "MOV",
            Self::Mkv => "Matroska",
            Self::WebM => "WebM",
            Self::Avi => "AVI",
            Self::Mpeg => "MPEG",
            Self::M2v => "MPEG-2 Video",
            Self::TransportStream => "Transport Stream",
            Self::Wmv => "WMV",
            Self::Flv => "FLV",
            Self::ThreeGp => "3GP",
            Self::Ogv => "Ogg Video",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, order: usize) -> MediaEntry {
        MediaEntry {
            path: PathBuf::from(name),
            file_name: OsString::from(name),
            display_name: name.to_owned(),
            extension: Some("jpg".to_owned()),
            file_len: 0,
            created: None,
            modified: None,
            discovered_order: order,
            dimensions: None,
            media_kind: MediaKind::Image(ImageKind::Jpeg),
            exif_date: None,
            exif_orientation: None,
            dimensions_attempted: false,
            exif_attempted: false,
        }
    }

    #[test]
    fn delete_queue_toggle_is_idempotent() {
        let mut state = AppState::new(
            PathBuf::from("."),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            vec![entry("a.jpg", 0)],
        );

        state.toggle_queue_current();
        state.toggle_queue_current();

        assert_eq!(state.queue_count(), 0);
    }

    #[test]
    fn index_clamps_after_entries_shrink() {
        let mut state = AppState::new(
            PathBuf::from("."),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            vec![entry("a.jpg", 0), entry("b.jpg", 1)],
        );
        state.current_index = 8;

        state.set_entries_preserving_current(vec![entry("a.jpg", 0)], None);

        assert_eq!(state.current_index, 0);
    }

    #[test]
    fn page_movement_uses_last_grid_page_size() {
        let mut state = AppState::new(
            PathBuf::from("."),
            false,
            false,
            MediaMode::Image,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            (0..10).map(|i| entry(&format!("{i}.jpg"), i)).collect(),
        );
        state.last_grid_page_size = 4;

        state.page_by(1);

        assert_eq!(state.current_index, 4);
    }

    #[test]
    fn media_kind_classifies_image_and_video_extensions() {
        assert_eq!(
            MediaKind::from_extension(Some("jpg")),
            Some(MediaKind::Image(ImageKind::Jpeg))
        );
        assert_eq!(
            MediaKind::from_extension(Some("mp4")),
            Some(MediaKind::Video(VideoKind::Mp4))
        );
        assert_eq!(MediaKind::from_extension(Some("txt")), None);
    }

    #[test]
    fn app_state_stores_selected_media_mode() {
        for media_mode in [MediaMode::Both, MediaMode::Image, MediaMode::Video] {
            let state = AppState::new(
                PathBuf::from("."),
                false,
                false,
                media_mode,
                vec!["jpg".to_owned()],
                SortMode::Discovered,
                vec![entry("a.jpg", 0)],
            );

            assert_eq!(state.media_mode, media_mode);
        }
    }
}
