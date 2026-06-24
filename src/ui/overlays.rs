use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    metadata::effective_date,
    state::{AppState, ImageEntry},
};

pub fn draw_info(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(entry) = state.current_entry() else {
        return;
    };
    let rect = floating_rect(area, 64, 46);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title("Info")
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .style(Style::default().fg(Color::White));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = info_lines(entry, state);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
}

pub fn draw_help(frame: &mut Frame, area: Rect) {
    let rect = floating_rect(area, 58, 70);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title("Help")
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from("Navigation"),
        Line::from("  j/k        next/previous"),
        Line::from("  g          grid/preview"),
        Line::from("  ctrl+d/u   next/previous page"),
        Line::from("  q          quit"),
        Line::from(""),
        Line::from("Culling"),
        Line::from("  d/space    queue for deletion"),
        Line::from("  u          unqueue"),
        Line::from("  D          show delete queue"),
        Line::from("  ctrl+r     permanently delete queued images"),
        Line::from(""),
        Line::from("View"),
        Line::from("  z          fit/original pixels"),
        Line::from("  i          image info"),
        Line::from("  f          full-terminal UI"),
        Line::from("  h/?        help"),
        Line::from(""),
        Line::from("Sorting/scan"),
        Line::from("  r          recursive on/off"),
        Line::from("  R          rescan"),
        Line::from("  t          newest/oldest"),
        Line::from("  n          name/name reversed"),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
}

fn info_lines(entry: &ImageEntry, state: &AppState) -> Vec<Line<'static>> {
    let dimensions = entry
        .dimensions
        .map(|(width, height)| {
            let mp = (width as f64 * height as f64) / 1_000_000.0;
            format!("{width} x {height} ({mp:.2} MP)")
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let kind = entry
        .image_type
        .as_ref()
        .map(|kind| kind.as_str().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let date = effective_date(entry)
        .and_then(format_system_time)
        .unwrap_or_else(|| "unknown".to_owned());
    let queued = if state.delete_queue.contains(&entry.path) {
        "yes"
    } else {
        "no"
    };
    let index = if state.entries.is_empty() {
        "0 / 0".to_owned()
    } else {
        format!("{} / {}", state.current_index + 1, state.entries.len())
    };

    vec![
        Line::from(format!("filename: {}", entry.display_name)),
        Line::from(format!("dimensions: {dimensions}")),
        Line::from(format!("type: {kind}")),
        Line::from(format!("file size: {} bytes", entry.file_len)),
        Line::from(format!("date: {date}")),
        Line::from(format!("path: {}", entry.path.display())),
        Line::from(format!("delete queued: {queued}")),
        Line::from(format!("index: {index}")),
    ]
}

fn format_system_time(value: std::time::SystemTime) -> Option<String> {
    let date_time: OffsetDateTime = value.into();
    date_time.format(&Rfc3339).ok()
}

fn floating_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let target_width = area.width.saturating_mul(width_percent) / 100;
    let target_height = area.height.saturating_mul(height_percent) / 100;
    let width = target_width
        .max(1)
        .min(area.width.max(1))
        .max(area.width.min(24));
    let height = target_height
        .max(1)
        .min(area.height.max(1))
        .max(area.height.min(8));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}
