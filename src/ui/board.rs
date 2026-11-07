use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, CardSize, Project};
use crate::types::Column;
use crate::ui::card::{self, CARD_HEIGHT, CARD_HEIGHT_MEDIUM};
use crate::ui::styles;

const MIN_SWIMLANE_HEIGHT: u16 = 3;

fn effective_card_height(card_size: CardSize) -> u16 {
    match card_size {
        CardSize::Full => CARD_HEIGHT,
        CardSize::Medium => CARD_HEIGHT_MEDIUM,
    }
}

pub fn render_board(
    frame: &mut Frame,
    project: &Project,
    app: &App,
    area: Rect,
    card_size: CardSize,
    is_focused_lane: bool,
) {
    let swimlane_count = app.visible_swimlane_count();
    let search_query = &app.search_query;

    if swimlane_count > 1 {
        if area.height < MIN_SWIMLANE_HEIGHT {
            return;
        }
        let header_area = Rect::new(area.x, area.y, area.width, 1);
        let board_area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);

        let border_color = if is_focused_lane {
            styles::ACCENT
        } else {
            Color::DarkGray
        };
        let name_style = if is_focused_lane {
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title = Span::styled(
            format!("\u{2500}\u{2500} {} ", project.display_name()),
            name_style,
        );
        // Display width, not bytes: names can hold non-ASCII characters.
        let separator = "\u{2500}".repeat(area.width.saturating_sub(title.width() as u16) as usize);
        let header_line = Line::from(vec![
            title,
            Span::styled(separator, Style::default().fg(border_color)),
        ]);
        frame.render_widget(Paragraph::new(header_line), header_area);

        render_columns(
            frame,
            project,
            board_area,
            card_size,
            is_focused_lane,
            search_query,
        );
    } else {
        render_columns(
            frame,
            project,
            area,
            card_size,
            is_focused_lane,
            search_query,
        );
    }
}

fn render_columns(
    frame: &mut Frame,
    project: &Project,
    area: Rect,
    card_size: CardSize,
    is_focused_lane: bool,
    search_query: &str,
) {
    let columns = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(area);

    for (col_idx, column) in Column::ALL.iter().enumerate() {
        let is_selected_col = is_focused_lane && col_idx == project.selected_column;
        render_column(
            frame,
            project,
            *column,
            columns[col_idx],
            is_selected_col,
            card_size,
            search_query,
        );
    }
}

fn render_column(
    frame: &mut Frame,
    project: &Project,
    column: Column,
    area: Rect,
    is_selected_col: bool,
    card_size: CardSize,
    search_query: &str,
) {
    let issues = project.issues_in_column(column, search_query);
    let count = issues.len();
    let selected_row = project.selected_row[column.index()];
    let card_h = effective_card_height(card_size);

    let border_style = styles::card_border_style(is_selected_col, false);
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
            frame.render_widget(Paragraph::new(empty_msg), inner);
        }
        return;
    }

    let max_cards = (inner.height / card_h) as usize;
    if max_cards == 0 {
        return;
    }

    let (viewport_start, viewport_end) =
        compute_viewport(selected_row, count, max_cards, card_h, inner.height);

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

    for (visible_idx, (_global_idx, issue)) in
        issues[viewport_start..viewport_end].iter().enumerate()
    {
        let card_y = inner.y + y_offset + (visible_idx as u16 * card_h);
        if card_y + card_h > inner.y + inner.height {
            break;
        }

        let card_area = Rect::new(inner.x, card_y, inner.width, card_h);
        let is_selected = is_selected_col && (viewport_start + visible_idx) == selected_row;
        let marked = project.marked_issues.contains(&issue.id.to_lowercase());

        let pr = project.pr_for(issue);
        let ctx = card::CardContext {
            issue,
            selected: is_selected,
            marked,
            session_alive: project
                .is_session_alive(&issue.session_name(&project.config.project_name)),
            agent_status: project.resolved_agent_status(issue),
            activity: project.resolved_activity(issue),
            git_status: project.worktree_status_for(issue),
            pr,
            project,
            ports: project.listening_ports_for(issue),
            search_query,
        };

        card::render_card(frame, &ctx, card_area, card_size);
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

/// Compute the `[start, end)` window of cards to render, keeping
/// `selected_row` visible.
///
/// `max_cards` is `inner_height / card_h` (a floor division), which leaves no
/// room for the "N more above" indicator row drawn whenever the window is
/// scrolled past row 0. When there isn't slack to absorb that row, the
/// window drops its top card to free a full card's height — more than the
/// single row the indicator needs, but the smallest unit available. Skipped
/// when `selected_row` is already the window's top row: keeping the
/// selection visible outranks the indicator.
fn compute_viewport(
    selected_row: usize,
    count: usize,
    max_cards: usize,
    card_h: u16,
    inner_height: u16,
) -> (usize, usize) {
    let mut start = if selected_row < max_cards {
        0
    } else {
        selected_row - max_cards + 1
    };
    let end = (start + max_cards).min(count);

    if start > 0 {
        let rows_needed = (end - start) as u16 * card_h + 1;
        if rows_needed > inner_height && start < selected_row {
            start += 1;
        }
    }

    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_with_slack_needs_no_shrink() {
        // inner_height has slack beyond max_cards * card_h (29 vs 28), which
        // covers the above-indicator's row for free.
        let (start, end) = compute_viewport(9, 10, 4, 7, 29);
        assert_eq!((start, end), (6, 10));
    }

    #[test]
    fn viewport_without_slack_shrinks_for_the_above_indicator_row() {
        // 10 issues, 4-card window, scrolled to the last row, zero slack
        // (28 == 4 * 7): the naive window leaves no room for the "more
        // above" indicator, so it drops to 3 cards to free one.
        let (start, end) = compute_viewport(9, 10, 4, 7, 28);
        assert_eq!((start, end), (7, 10));
    }

    #[test]
    fn viewport_no_scroll_needed_is_unaffected() {
        // Everything fits already (count <= max_cards): no indicator row is
        // ever drawn, so nothing should shrink.
        let (start, end) = compute_viewport(4, 5, 10, 7, 70);
        assert_eq!((start, end), (0, 5));
    }

    /// Mirrors `render_column`'s per-card row math to prove the selected
    /// card's `card_y + card_h` always stays within `inner_height`. Before
    /// this fix, scrolling to the last card in a zero-slack column computed
    /// a window whose bottom card overflowed once the above-indicator was
    /// drawn, and the render loop's overflow guard silently dropped it — the
    /// selection simply never appeared.
    #[test]
    fn selected_card_row_never_overflows_inner_height() {
        let (max_cards, card_h, inner_height) = (4u16, 7u16, 28u16);
        let (start, end) = compute_viewport(9, 10, max_cards as usize, card_h, inner_height);
        let has_above = start > 0;
        let y_offset = if has_above { 1 } else { 0 };
        let selected_visible_idx = (9 - start) as u16;
        let card_y = y_offset + selected_visible_idx * card_h;
        assert!(end > 9, "selection must stay inside the rendered window");
        assert!(
            card_y + card_h <= inner_height,
            "selected card at row {card_y} (height {card_h}) overflows inner_height {inner_height}"
        );
    }
}
