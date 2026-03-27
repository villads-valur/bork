use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::types::Column;
use crate::ui::card::{self, CARD_HEIGHT};
use crate::ui::styles;

pub fn render_board(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(area);

    for (col_idx, column) in Column::ALL.iter().enumerate() {
        let is_selected_col = col_idx == app.selected_column;
        render_column(frame, app, *column, columns[col_idx], is_selected_col);
    }
}

fn render_column(frame: &mut Frame, app: &App, column: Column, area: Rect, is_selected_col: bool) {
    let issues = app.issues_in_column(column);
    let count = issues.len();
    let selected_row = app.selected_row[column.index()];

    let border_style = if is_selected_col {
        styles::card_border_style(true)
    } else {
        styles::card_border_style(false)
    };

    let title_style = styles::column_header_style(is_selected_col);
    let title = format!(" {} ({}) ", column, count);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title, title_style));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 || count == 0 {
        if count == 0 {
            let empty_msg = Line::from(Span::styled(
                "no issues",
                styles::dim_style().add_modifier(Modifier::ITALIC),
            ));
            let paragraph = Paragraph::new(empty_msg);
            frame.render_widget(paragraph, inner);
        }
        return;
    }

    // Figure out how many cards we can show
    let max_cards = (inner.height / CARD_HEIGHT) as usize;
    if max_cards == 0 {
        return;
    }

    // Calculate viewport: ensure selected card is visible
    let viewport_start = if selected_row < max_cards {
        0
    } else {
        selected_row - max_cards + 1
    };
    let viewport_end = (viewport_start + max_cards).min(count);

    // Show overflow indicators
    let has_above = viewport_start > 0;
    let has_below = viewport_end < count;

    let mut y_offset = 0u16;

    if has_above {
        let indicator = Line::from(Span::styled(
            format!("  {} more above", viewport_start),
            styles::dim_style(),
        ));
        let indicator_area = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(Paragraph::new(indicator), indicator_area);
        y_offset += 1;
    }

    for (visible_idx, &(_global_idx, ref issue)) in
        issues[viewport_start..viewport_end].iter().enumerate()
    {
        let card_y = inner.y + y_offset + (visible_idx as u16 * CARD_HEIGHT);
        if card_y + CARD_HEIGHT > inner.y + inner.height {
            break;
        }

        let card_area = Rect::new(inner.x, card_y, inner.width, CARD_HEIGHT);
        let is_selected = is_selected_col && (viewport_start + visible_idx) == selected_row;
        let session_alive = app.is_session_alive(&issue.session_name());

        card::render_card(frame, issue, card_area, is_selected, session_alive);
    }

    if has_below {
        let remaining = count - viewport_end;
        let indicator_y = inner.y + inner.height - 1;
        let indicator = Line::from(Span::styled(
            format!("  {} more below", remaining),
            styles::dim_style(),
        ));
        let indicator_area = Rect::new(inner.x, indicator_y, inner.width, 1);
        frame.render_widget(Paragraph::new(indicator), indicator_area);
    }
}
