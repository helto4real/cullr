use ratatui::{Frame, layout::Rect, widgets::Paragraph};

use crate::{
    renderer::NativeRatatuiImageRenderer,
    state::{AppState, ZoomMode},
};

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    renderer: &mut NativeRatatuiImageRenderer,
) {
    let Some(entry) = state.current_entry().cloned() else {
        frame.render_widget(Paragraph::new("No images found."), area);
        return;
    };

    renderer.render_preview(frame, area, &entry, state.zoom_mode);

    if !state.fullscreen_ui {
        let zoom = match state.zoom_mode {
            ZoomMode::Fit => "fit",
            ZoomMode::OriginalPixels => "original",
        };
        let overlay = format!(" {} | {} ", entry.display_name, zoom);
        let label_area = Rect::new(area.x, area.y, area.width.min(overlay.len() as u16), 1);
        frame.render_widget(Paragraph::new(overlay), label_area);
    }
}
