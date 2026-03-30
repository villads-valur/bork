use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, LinearPickerContext};
use crate::ui::styles;

const PICKER_MIN_WIDTH: u16 = 50;
const PICKER_MAX_WIDTH: u16 = 100;
const VISIBLE_ITEMS: usize = 10;

pub fn render_linear_picker(frame: &mut Frame, app: &App) {
    let picker = match &app.linear_picker {
        Some(p) => p,
        None => return,
    };

    let filtered = app.filtered_linear_issues();
    let count = filtered.len();

    let imported_ids: HashSet<&str> = app
        .issues
        .iter()
        .filter_map(|i| i.linear_id.as_deref())
        .collect();

    let area = frame.area();
    let width = (area.width * 70 / 100).clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH);
    let height = (VISIBLE_ITEMS as u16 + 7).min(area.height);
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;

    let picker_area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, picker_area);

    let picker_title = if app.linear_picker_context == LinearPickerContext::Attach {
        " Attach Linear Issue "
    } else {
        " Import Linear Issue "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(styles::ACCENT))
        .title(Span::styled(
            picker_title,
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(picker_area);
    frame.render_widget(block, picker_area);

    if inner.height < 4 || inner.width < 10 {
        return;
    }

    let field_width = inner.width.saturating_sub(2) as usize;

    // Search field
    let search_area = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, 1);
    let max_search_chars = field_width.saturating_sub(10);
    let char_count = picker.search.chars().count();
    let search_display = if char_count > max_search_chars && max_search_chars > 3 {
        let skip = char_count - (max_search_chars - 3);
        let tail: String = picker.search.chars().skip(skip).collect();
        format!("...{}", tail)
    } else {
        picker.search.clone()
    };

    let search_line = Line::from(vec![
        Span::styled(
            "Search: ",
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(&search_display, Style::default().fg(styles::TEXT)),
        Span::styled("\u{2588}", Style::default().fg(styles::ACCENT)),
    ]);
    frame.render_widget(Paragraph::new(search_line), search_area);

    // Divider
    let divider_area = Rect::new(inner.x + 1, inner.y + 2, inner.width - 2, 1);
    let divider = Line::from(Span::styled(
        "\u{2500}".repeat(field_width),
        styles::dim_style(),
    ));
    frame.render_widget(Paragraph::new(divider), divider_area);

    // Issue list
    let list_start_y = inner.y + 3;
    let available_rows = (inner.height.saturating_sub(5)) as usize;
    let visible_count = available_rows.min(VISIBLE_ITEMS);

    let scroll = if visible_count == 0 || picker.selected < visible_count {
        0
    } else {
        picker.selected - visible_count + 1
    };

    if count == 0 {
        let empty_area = Rect::new(inner.x + 1, list_start_y, inner.width - 2, 1);
        let msg = if app.linear_issues.is_empty() {
            "No issues loaded"
        } else {
            "No matching issues"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, styles::dim_style()))),
            empty_area,
        );
    } else {
        for i in 0..visible_count {
            let idx = scroll + i;
            if idx >= count {
                break;
            }

            let issue = filtered[idx];
            let is_selected = idx == picker.selected;
            let is_imported = imported_ids.contains(issue.id.as_str());
            let y = list_start_y + i as u16;
            let row_area = Rect::new(inner.x + 1, y, inner.width - 2, 1);

            let pointer = if is_selected { "\u{25b8} " } else { "  " };
            let pointer_style = if is_selected {
                Style::default()
                    .fg(styles::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let priority_str = if is_imported {
                "\u{2713}   "
            } else {
                match issue.priority {
                    1 => "!!! ",
                    2 => "!!  ",
                    3 => "!   ",
                    _ => "    ",
                }
            };

            let id_width = issue.identifier.len();
            let state_str = if is_imported {
                " \u{25cf} on board".to_string()
            } else {
                format!(" \u{25cf} {}", issue.state_name)
            };
            let overhead = 2 + priority_str.len() + id_width + 1 + state_str.len();
            let title_budget = field_width.saturating_sub(overhead);
            let title = truncate(&issue.title, title_budget);

            let title_style = if is_imported {
                styles::dim_style()
            } else {
                Style::default().fg(styles::TEXT)
            };

            let priority_style = if is_imported {
                Style::default().fg(styles::ACCENT)
            } else {
                Style::default().fg(ratatui::style::Color::Yellow)
            };

            let line = Line::from(vec![
                Span::styled(pointer, pointer_style),
                Span::styled(priority_str, priority_style),
                Span::styled(&issue.identifier, styles::dim_style()),
                Span::raw(" "),
                Span::styled(title, title_style),
                Span::styled(state_str, styles::dim_style()),
            ]);

            frame.render_widget(Paragraph::new(line), row_area);
        }
    }

    // Footer
    let footer_y = inner.y + inner.height - 1;
    let footer_area = Rect::new(inner.x + 1, footer_y, inner.width - 2, 1);
    let select_hint = if app.linear_picker_context == LinearPickerContext::Attach {
        ":attach  "
    } else {
        ":import  "
    };
    let footer = Line::from(vec![
        Span::styled("Enter", styles::statusbar_key_style()),
        Span::styled(select_hint, styles::statusbar_desc_style()),
        Span::styled("\u{2191}\u{2193}", styles::statusbar_key_style()),
        Span::styled(":navigate  ", styles::statusbar_desc_style()),
        Span::styled("^R", styles::statusbar_key_style()),
        Span::styled(":refresh  ", styles::statusbar_desc_style()),
        Span::styled("Esc", styles::statusbar_key_style()),
        Span::styled(":close", styles::statusbar_desc_style()),
        Span::styled(
            format!("  {}/{}", count.min(picker.selected + 1), count),
            styles::dim_style(),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max > 3 {
        let end: String = s.chars().take(max - 3).collect();
        format!("{}...", end)
    } else {
        s.chars().take(max).collect()
    }
}
