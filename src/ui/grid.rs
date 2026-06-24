use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    renderer::{ImageRenderer, NativeRatatuiImageRenderer},
    state::AppState,
    thumbnail::{ThumbnailService, ThumbnailStatus},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridLayout {
    pub cols: usize,
    pub rows: usize,
    pub page_size: usize,
    pub cell_width: u16,
    pub cell_height: u16,
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    renderer: &mut NativeRatatuiImageRenderer,
    thumbnails: &mut ThumbnailService,
    delete_queue_only: bool,
) {
    let layout = compute_grid_layout(area);
    state.last_grid_page_size = layout.page_size.max(1);

    let indices: Vec<usize> = if delete_queue_only {
        state.queued_indices()
    } else {
        (0..state.entries.len()).collect()
    };

    if indices.is_empty() {
        let message = if delete_queue_only {
            "Delete queue is empty."
        } else {
            "No images found."
        };
        frame.render_widget(Paragraph::new(message).centered(), area);
        return;
    }

    let current_pos = indices
        .iter()
        .position(|&index| index == state.current_index)
        .unwrap_or(0);
    let page = current_pos / layout.page_size.max(1);
    state.grid_page = page;
    let start = page * layout.page_size;
    let end = (start + layout.page_size).min(indices.len());

    for (visible_index, entry_index) in indices[start..end].iter().copied().enumerate() {
        let row = visible_index / layout.cols;
        let col = visible_index % layout.cols;
        let cell = Rect::new(
            area.x + (col as u16 * layout.cell_width),
            area.y + (row as u16 * layout.cell_height),
            layout
                .cell_width
                .min(area.width.saturating_sub(col as u16 * layout.cell_width)),
            layout
                .cell_height
                .min(area.height.saturating_sub(row as u16 * layout.cell_height)),
        );
        draw_cell(frame, cell, state, renderer, thumbnails, entry_index);
    }
}

pub fn compute_grid_layout(area: Rect) -> GridLayout {
    let target_cell_width = 18;
    let target_cell_height = 10;
    let cols = usize::from((area.width / target_cell_width).max(1));
    let rows = usize::from((area.height / target_cell_height).max(1));
    let page_size = cols.saturating_mul(rows).max(1);
    let cell_width = (area.width / cols as u16).max(1);
    let cell_height = (area.height / rows as u16).max(1);

    GridLayout {
        cols,
        rows,
        page_size,
        cell_width,
        cell_height,
    }
}

fn draw_cell(
    frame: &mut Frame,
    cell: Rect,
    state: &AppState,
    renderer: &mut NativeRatatuiImageRenderer,
    thumbnails: &mut ThumbnailService,
    entry_index: usize,
) {
    let Some(entry) = state.entries.get(entry_index) else {
        return;
    };
    let current = entry_index == state.current_index;
    let queued = state.delete_queue.contains(&entry.path);
    let title = cell_title(&entry.display_name, queued, cell.width);
    let style = if current {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if queued {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title);
    let inner = block.inner(cell);
    frame.render_widget(block, cell);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let status = thumbnails.get_or_request(
        entry,
        inner.width,
        inner.height,
        state.thumbnail_generation,
        renderer.backend_id(),
    );
    match status {
        ThumbnailStatus::Ready { key, image } => {
            renderer.render_thumbnail(frame, inner, &key, image);
        }
        ThumbnailStatus::Loading => {
            frame.render_widget(
                Paragraph::new(truncate(&entry.display_name, inner.width)),
                inner,
            );
        }
        ThumbnailStatus::Failed(error) => {
            frame.render_widget(
                Paragraph::new(format!("decode failed\n{}", truncate(&error, inner.width))),
                inner,
            );
        }
    }
}

fn cell_title(name: &str, queued: bool, width: u16) -> String {
    let prefix = if queued { "DEL " } else { "" };
    let max = width.saturating_sub(2) as usize;
    let mut value = format!("{prefix}{name}");
    if value.len() > max {
        value.truncate(max.saturating_sub(1));
        value.push('~');
    }
    value
}

fn truncate(value: &str, width: u16) -> String {
    let max = width as usize;
    if value.len() <= max {
        value.to_owned()
    } else {
        let mut text = value.to_owned();
        text.truncate(max.saturating_sub(1));
        text.push('~');
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_calculation_never_returns_zero() {
        let layout = compute_grid_layout(Rect::new(0, 0, 1, 1));

        assert_eq!(layout.page_size, 1);
    }

    #[test]
    fn expected_grid_page_size() {
        let layout = compute_grid_layout(Rect::new(0, 0, 80, 24));

        assert_eq!(layout.cols, 4);
        assert_eq!(layout.rows, 2);
        assert_eq!(layout.page_size, 8);
    }
}
