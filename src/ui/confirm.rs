use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
};

pub fn draw(frame: &mut Frame, area: Rect, queued: usize) {
    let width = area.width.min(54).max(area.width.min(24));
    let height = area.height.min(7).max(area.height.min(5));
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title("Confirm delete")
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let text = Text::from(vec![
        Line::from(format!("Permanently delete {queued} files?")),
        Line::from("This cannot be undone."),
        Line::from(""),
        Line::from("Press y to delete, n/Esc to cancel.")
            .style(Style::default().add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(text), inner);
}
