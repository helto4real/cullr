use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::Paragraph,
};

use crate::{
    renderer::NativeRatatuiImageRenderer,
    state::{AppState, ImageEntry, PreviewPrefetchMarker, ZoomMode},
    thumbnail::{PreviewStatus, ThumbnailService},
};

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    renderer: &mut NativeRatatuiImageRenderer,
    thumbnails: &mut ThumbnailService,
) {
    state.last_preview_size = Some((area.width, area.height));
    let Some(entry) = state.current_entry() else {
        frame.render_widget(Paragraph::new("No images found."), area);
        return;
    };

    let display_name = entry.display_name.clone();

    match thumbnails.get_or_request_preview(
        entry,
        area.width,
        area.height,
        state.zoom_mode,
        state.thumbnail_generation,
    ) {
        PreviewStatus::Ready { protocol, .. } => {
            renderer.render_preview_protocol(frame, area, protocol.as_ref());
        }
        PreviewStatus::Loading => {
            frame.render_widget(
                Paragraph::new(format!("Loading {display_name}"))
                    .style(Style::default().fg(Color::Gray))
                    .centered(),
                area,
            );
        }
        PreviewStatus::Failed(error) => {
            frame.render_widget(
                Paragraph::new(format!("Failed to render {display_name}\n{error}")),
                area,
            );
        }
    }

    if !state.fullscreen_ui {
        let zoom = match state.zoom_mode {
            ZoomMode::Fit => "fit",
            ZoomMode::OriginalPixels => "original",
        };
        let overlay = format!(" {display_name} | {zoom} ");
        let label_area = Rect::new(area.x, area.y, area.width.min(overlay.len() as u16), 1);
        frame.render_widget(Paragraph::new(overlay), label_area);
    }

    prefetch_after_current(state, area, thumbnails);
}

fn prefetch_after_current(state: &mut AppState, area: Rect, thumbnails: &mut ThumbnailService) {
    if state.entries.is_empty() {
        return;
    }

    if should_eager_prefetch_previews(&state.entries, thumbnails.preview_budget_bytes()) {
        let marker = PreviewPrefetchMarker {
            generation: state.thumbnail_generation,
            width_cells: area.width.max(1),
            height_cells: area.height.max(1),
            zoom_mode: state.zoom_mode,
            entry_count: state.entries.len(),
        };
        if state.eager_preview_prefetch != Some(marker) {
            thumbnails.ensure_preview_capacity(state.entries.len().max(5));
            for (index, entry) in state.entries.iter().enumerate() {
                if index == state.current_index {
                    continue;
                }
                let _ = thumbnails.prefetch_preview(
                    entry,
                    area.width,
                    area.height,
                    state.zoom_mode,
                    state.thumbnail_generation,
                );
            }
            state.eager_preview_prefetch = Some(marker);
        }
        return;
    }

    let generation = state.thumbnail_generation;
    let current = state.current_index;
    for index in neighbor_indices(current, state.entries.len()) {
        if let Some(entry) = state.entries.get(index) {
            thumbnails.prefetch_preview(
                entry,
                area.width,
                area.height,
                state.zoom_mode,
                generation,
            );
        }
    }
}

fn neighbor_indices(current: usize, len: usize) -> Vec<usize> {
    let mut indices = Vec::new();
    for distance in 1..=2 {
        if let Some(previous) = current.checked_sub(distance) {
            indices.push(previous);
        }
        let next = current + distance;
        if next < len {
            indices.push(next);
        }
    }
    indices
}

pub fn should_eager_prefetch_previews(entries: &[ImageEntry], budget_bytes: usize) -> bool {
    if entries.is_empty() {
        return false;
    }
    if entries.len() <= 16 {
        return true;
    }
    let total_bytes = entries
        .iter()
        .map(|entry| u128::from(entry.file_len))
        .sum::<u128>();
    total_bytes <= budget_bytes as u128
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ImageKind;
    use std::{ffi::OsString, path::PathBuf};

    fn entry(index: usize, file_len: u64) -> ImageEntry {
        ImageEntry {
            path: PathBuf::from(format!("{index}.jpg")),
            file_name: OsString::from(format!("{index}.jpg")),
            display_name: format!("{index}.jpg"),
            extension: Some("jpg".to_owned()),
            file_len,
            created: None,
            modified: None,
            discovered_order: index,
            dimensions: None,
            image_type: Some(ImageKind::Jpeg),
            exif_date: None,
            exif_orientation: None,
            dimensions_attempted: false,
            exif_attempted: false,
        }
    }

    #[test]
    fn preview_prefetch_orders_current_then_neighbors() {
        assert_eq!(neighbor_indices(3, 10), vec![2, 4, 1, 5]);
        assert_eq!(neighbor_indices(0, 3), vec![1, 2]);
    }

    #[test]
    fn eager_prefetches_small_folders_or_budget_sized_sets() {
        let small = (0..5).map(|index| entry(index, 10)).collect::<Vec<_>>();
        let many_small = (0..20).map(|index| entry(index, 10)).collect::<Vec<_>>();
        let many_large = (0..20)
            .map(|index| entry(index, 1024 * 1024))
            .collect::<Vec<_>>();

        assert!(should_eager_prefetch_previews(&small, 1));
        assert!(should_eager_prefetch_previews(&many_small, 1024));
        assert!(!should_eager_prefetch_previews(&many_large, 1024));
    }
}
