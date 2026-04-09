use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, CardSize, Project};
use crate::types::Column;
use crate::ui::card::{self, CARD_HEIGHT, CARD_HEIGHT_COMPACT, CARD_HEIGHT_MEDIUM};
use crate::ui::styles;

const MIN_SWIMLANE_HEIGHT: u16 = 3;

fn effective_card_height(card_size: CardSize) -> u16 {
    match card_size {
        CardSize::Full => CARD_HEIGHT,
        CardSize::Medium => CARD_HEIGHT_MEDIUM,
        CardSize::Compact => CARD_HEIGHT_COMPACT,
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
        let separator = "\u{2500}".repeat(
            area.width
                .saturating_sub(project.config.project_name.len() as u16 + 4) as usize,
        );
        let header_line = Line::from(vec![
            Span::styled(
                format!("\u{2500}\u{2500} {} ", project.config.project_name),
                name_style,
            ),
            Span::styled(separator, Style::default().fg(border_color)),
        ]);
        frame.render_widget(Paragraph::new(header_line), header_area);

        render_columns(frame, project, board_area, card_size, is_focused_lane);
    } else {
        render_columns(frame, project, area, card_size, is_focused_lane);
    }
}

fn render_columns(
    frame: &mut Frame,
    project: &Project,
    area: Rect,
    card_size: CardSize,
    is_focused_lane: bool,
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
) {
    let issues = project.issues_in_column(column);
    let count = issues.len();
    let selected_row = project.selected_row[column.index()];
    let card_h = effective_card_height(card_size);

    let border_style = styles::card_border_style(is_selected_col);
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

    let viewport_start = if selected_row < max_cards {
        0
    } else {
        selected_row - max_cards + 1
    };
    let viewport_end = (viewport_start + max_cards).min(count);

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

        let ctx = card::CardContext {
            issue,
            selected: is_selected,
            session_alive: project
                .is_session_alive(&issue.session_name(&project.config.project_name)),
            agent_status: project.resolved_agent_status(issue),
            activity: project.resolved_activity(issue),
            branch: project.branch_for(issue),
            git_status: project.worktree_status_for(issue),
            pr: project.pr_for(issue),
            ports: project.listening_ports_for(issue),
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
