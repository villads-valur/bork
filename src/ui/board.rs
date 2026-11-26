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

    let viewport = compute_viewport(selected_row, count, max_cards, card_h, inner.height);
    let (viewport_start, viewport_end) = (viewport.start, viewport.end);

    let has_below = viewport_end < count;

    if viewport.show_above {
        let indicator = Line::from(Span::styled(
            format!("  {} more above", viewport_start),
            styles::dim_style(),
        ));
        let indicator_area = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(Paragraph::new(indicator), indicator_area);
    }

    for (visible_idx, (_global_idx, issue)) in
        issues[viewport_start..viewport_end].iter().enumerate()
    {
        let card_y = inner.y + viewport.card_offset(visible_idx, card_h);
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

struct Viewport {
    start: usize,
    end: usize,
    show_above: bool,
}

impl Viewport {
    fn card_offset(&self, visible_idx: usize, card_h: u16) -> u16 {
        u16::from(self.show_above) + visible_idx as u16 * card_h
    }
}

/// Compute the `[start, end)` window of cards to render, keeping
/// `selected_row` visible.
///
/// `max_cards` is a floor division of `inner_height`, so a scrolled window
/// with zero slack has no row for the "N more above" indicator. It then drops
/// its top card to free one; with a single card there is nothing to drop, and
/// the indicator is hidden so the selected card still fits.
fn compute_viewport(
    selected_row: usize,
    count: usize,
    max_cards: usize,
    card_h: u16,
    inner_height: u16,
) -> Viewport {
    let start = if selected_row < max_cards {
        0
    } else {
        selected_row - max_cards + 1
    };
    let end = (start + max_cards).min(count);

    if start == 0 {
        return Viewport {
            start,
            end,
            show_above: false,
        };
    }

    // Once scrolled, the selection is the bottom row of a full window.
    let has_slack = max_cards as u16 * card_h < inner_height;
    if has_slack {
        return Viewport {
            start,
            end,
            show_above: true,
        };
    }

    if max_cards > 1 {
        return Viewport {
            start: start + 1,
            end,
            show_above: true,
        };
    }

    Viewport {
        start,
        end,
        show_above: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_selected_card_fits(
        selected_row: usize,
        count: usize,
        card_h: u16,
        inner_height: u16,
    ) -> Viewport {
        let max_cards = (inner_height / card_h) as usize;
        let viewport = compute_viewport(selected_row, count, max_cards, card_h, inner_height);
        assert!(
            (viewport.start..viewport.end).contains(&selected_row),
            "selection {selected_row} outside window {}..{}",
            viewport.start,
            viewport.end
        );
        let card_y = viewport.card_offset(selected_row - viewport.start, card_h);
        assert!(
            card_y + card_h <= inner_height,
            "selected card at row {card_y} (height {card_h}) overflows inner_height {inner_height}"
        );
        viewport
    }

    #[test]
    fn viewport_with_slack_keeps_the_full_window() {
        let viewport = assert_selected_card_fits(9, 10, CARD_HEIGHT, 29);
        assert_eq!(
            (viewport.start, viewport.end, viewport.show_above),
            (6, 10, true)
        );
    }

    #[test]
    fn viewport_without_slack_drops_a_card_for_the_above_indicator() {
        let viewport = assert_selected_card_fits(9, 10, CARD_HEIGHT, 28);
        assert_eq!(
            (viewport.start, viewport.end, viewport.show_above),
            (7, 10, true)
        );
    }

    #[test]
    fn viewport_at_the_scroll_boundary_stays_unscrolled() {
        let viewport = assert_selected_card_fits(3, 10, CARD_HEIGHT, 28);
        assert_eq!(
            (viewport.start, viewport.end, viewport.show_above),
            (0, 4, false)
        );
    }

    #[test]
    fn single_card_viewport_without_slack_hides_the_above_indicator() {
        let viewport = assert_selected_card_fits(3, 10, CARD_HEIGHT, CARD_HEIGHT);
        assert_eq!(
            (viewport.start, viewport.end, viewport.show_above),
            (3, 4, false)
        );
    }

    #[test]
    fn mid_column_selection_shows_both_indicators() {
        let viewport = assert_selected_card_fits(5, 10, CARD_HEIGHT, 28);
        assert_eq!(
            (viewport.start, viewport.end, viewport.show_above),
            (3, 6, true)
        );
        assert!(viewport.end < 10);
    }

    #[test]
    fn medium_cards_without_slack_drop_a_card_for_the_above_indicator() {
        let viewport = assert_selected_card_fits(9, 10, CARD_HEIGHT_MEDIUM, 24);
        assert_eq!(
            (viewport.start, viewport.end, viewport.show_above),
            (7, 10, true)
        );
    }

    #[test]
    fn single_medium_card_without_slack_hides_the_above_indicator() {
        let viewport = assert_selected_card_fits(3, 10, CARD_HEIGHT_MEDIUM, CARD_HEIGHT_MEDIUM);
        assert_eq!(
            (viewport.start, viewport.end, viewport.show_above),
            (3, 4, false)
        );
    }
}
