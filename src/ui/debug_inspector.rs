use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, InputMode};
use crate::ui::styles;

const INSPECTOR_WIDTH_PCT: u16 = 80;
const INSPECTOR_HEIGHT_PCT: u16 = 80;

pub fn render_debug_inspector(frame: &mut Frame, app: &App) {
    if app.input_mode != InputMode::DebugInspector {
        return;
    }

    let Some(ref json) = app.debug_inspector_json else {
        return;
    };

    let area = frame.area();
    let width = (area.width * INSPECTOR_WIDTH_PCT / 100)
        .max(40)
        .min(area.width);
    let height = (area.height * INSPECTOR_HEIGHT_PCT / 100)
        .max(10)
        .min(area.height);
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;

    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(styles::ACCENT))
        .title(Span::styled(
            " Debug: Issue ",
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 3 || inner.width < 10 {
        return;
    }

    let content_height = inner.height.saturating_sub(1) as usize; // reserve last row for footer
    let lines: Vec<&str> = json.lines().collect();
    let total = lines.len();

    let max_scroll = total.saturating_sub(content_height);
    let scroll = app.debug_inspector_scroll.min(max_scroll);

    let visible = &lines[scroll..total.min(scroll + content_height)];

    for (i, line_text) in visible.iter().enumerate() {
        let line = Line::from(Span::styled(
            format!(" {}", line_text),
            Style::default().fg(styles::TEXT),
        ));
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
        );
    }

    let scroll_indicator = if total > content_height {
        format!(" [{}/{}]", scroll + 1, max_scroll + 1)
    } else {
        String::new()
    };

    let footer = Line::from(vec![
        Span::styled(" Esc", styles::statusbar_key_style()),
        Span::styled(":close  ", styles::statusbar_desc_style()),
        Span::styled("j/k", styles::statusbar_key_style()),
        Span::styled(":scroll", styles::statusbar_desc_style()),
        Span::styled(scroll_indicator, styles::dim_style()),
    ]);
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}
