use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::Paragraph,
};

use crate::{
    renderer::{ImageRenderer, NativeRatatuiImageRenderer},
    state::{AppState, ViewMode},
    thumbnail::ThumbnailService,
};

pub mod confirm;
pub mod grid;
pub mod overlays;
pub mod preview;

pub fn draw(
    frame: &mut Frame,
    state: &mut AppState,
    renderer: &mut NativeRatatuiImageRenderer,
    thumbnails: &mut ThumbnailService,
) {
    let area = frame.area();
    let main_area = if state.fullscreen_ui {
        area
    } else {
        let [main, status] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        draw_status(frame, status, state, renderer.backend_id());
        main
    };

    match state.mode {
        ViewMode::Preview => preview::draw(frame, main_area, state, renderer, thumbnails),
        ViewMode::Grid => grid::draw(frame, main_area, state, renderer, thumbnails, false),
        ViewMode::DeleteQueueGrid => {
            grid::draw(frame, main_area, state, renderer, thumbnails, true)
        }
    }

    if state.show_info_overlay {
        overlays::draw_info(frame, area, state);
    }
    if state.show_help_overlay {
        overlays::draw_help(frame, area);
    }
    if state.confirm_delete {
        confirm::draw(frame, area, state.queue_count());
    }
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState, backend_id: &str) {
    let mode = match state.mode {
        ViewMode::Preview => "preview",
        ViewMode::Grid => "grid",
        ViewMode::DeleteQueueGrid => "delete-queue",
    };
    let current = if state.entries.is_empty() {
        "0 / 0".to_owned()
    } else {
        format!("{} / {}", state.current_index + 1, state.entries.len())
    };
    let message = state.status_message.as_deref().unwrap_or("");
    let line = Line::from(format!(
        " {mode} | {current} | queued: {} | sort: {:?} | {backend_id} | {message}",
        state.queue_count(),
        state.sort_mode
    ));
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(Color::Gray)),
        area,
    );
}
