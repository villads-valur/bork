use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, ImportSource, LinearPickerContext};
use crate::ui::styles;

const PICKER_MIN_WIDTH: u16 = 50;
const PICKER_MAX_WIDTH: u16 = 100;
const VISIBLE_ITEMS: usize = 10;

pub fn render_import_picker(frame: &mut Frame, app: &App) {
    let picker = match &app.linear_picker {
        Some(p) => p,
        None => return,
    };

    let has_linear = !app.active_project().live.linear_issues.is_empty();
    let has_github = app.active_project().has_github_prs();
    let show_tabs = has_linear && has_github;

    let area = frame.area();
    let width = (area.width * 70 / 100).clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH);
    let height = (VISIBLE_ITEMS as u16 + if show_tabs { 10 } else { 7 }).min(area.height);
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;

    let picker_area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, picker_area);

    let picker_title = match (app.linear_picker_context, app.picker_tab) {
        (LinearPickerContext::Attach, ImportSource::GitHub) => " Attach GitHub PRs ",
        (LinearPickerContext::Attach, ImportSource::Linear) => " Attach Linear Issues ",
        (_, ImportSource::GitHub) => " Import GitHub PR ",
        (_, ImportSource::Linear) => " Import Linear Issue ",
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
    let mut row_y = inner.y + 1;

    if show_tabs {
        let tab_area = Rect::new(inner.x + 1, row_y, inner.width - 2, 1);
        render_tab_bar(frame, app.picker_tab, tab_area);
        row_y += 2;
    }

    let search_area = Rect::new(inner.x + 1, row_y, inner.width - 2, 1);
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
    row_y += 1;

    let divider_area = Rect::new(inner.x + 1, row_y, inner.width - 2, 1);
    let divider = Line::from(Span::styled(
        "\u{2500}".repeat(field_width),
        styles::dim_style(),
    ));
    frame.render_widget(Paragraph::new(divider), divider_area);
    row_y += 1;

    let list_start_y = row_y;
    let available_rows = inner.height.saturating_sub(row_y - inner.y + 2) as usize;
    let visible_count = available_rows.min(VISIBLE_ITEMS);

    match app.picker_tab {
        ImportSource::Linear => render_linear_list(
            frame,
            app,
            picker,
            list_start_y,
            inner,
            field_width,
            visible_count,
        ),
        ImportSource::GitHub => render_github_list(
            frame,
            app,
            picker,
            list_start_y,
            inner,
            field_width,
            visible_count,
        ),
    }

    let footer_y = inner.y + inner.height - 1;
    let footer_area = Rect::new(inner.x + 1, footer_y, inner.width - 2, 1);

    let count = match app.picker_tab {
        ImportSource::Linear => app.filtered_linear_issues().len(),
        ImportSource::GitHub => app.filtered_github_prs().len(),
    };

    let select_hint = if app.linear_picker_context == LinearPickerContext::Attach {
        ":toggle  "
    } else {
        ":import  "
    };

    let mut footer_spans = vec![
        Span::styled("Enter", styles::statusbar_key_style()),
        Span::styled(select_hint, styles::statusbar_desc_style()),
        Span::styled("\u{2191}\u{2193}", styles::statusbar_key_style()),
        Span::styled(":navigate  ", styles::statusbar_desc_style()),
    ];

    if show_tabs {
        footer_spans.push(Span::styled(
            "\u{2190}\u{2192}",
            styles::statusbar_key_style(),
        ));
        footer_spans.push(Span::styled(":switch  ", styles::statusbar_desc_style()));
    }

    footer_spans.push(Span::styled("Ctrl+r", styles::statusbar_key_style()));
    footer_spans.push(Span::styled(":refresh  ", styles::statusbar_desc_style()));
    footer_spans.push(Span::styled("Esc", styles::statusbar_key_style()));
    footer_spans.push(Span::styled(":close", styles::statusbar_desc_style()));
    footer_spans.push(Span::styled(
        format!("  {}/{}", count.min(picker.selected + 1), count),
        styles::dim_style(),
    ));

    frame.render_widget(Paragraph::new(Line::from(footer_spans)), footer_area);
}

fn render_tab_bar(frame: &mut Frame, active: ImportSource, area: Rect) {
    let active_style = Style::default()
        .fg(styles::ACCENT)
        .add_modifier(Modifier::BOLD);
    let bracket_style = Style::default().fg(styles::ACCENT);
    let inactive_style = styles::dim_style();

    let mut spans = Vec::new();
    if active == ImportSource::Linear {
        spans.push(Span::styled("[", bracket_style));
        spans.push(Span::styled("Linear", active_style));
        spans.push(Span::styled("]", bracket_style));
        spans.push(Span::raw("  "));
        spans.push(Span::styled("GitHub", inactive_style));
    } else {
        spans.push(Span::styled("Linear", inactive_style));
        spans.push(Span::raw("  "));
        spans.push(Span::styled("[", bracket_style));
        spans.push(Span::styled("GitHub", active_style));
        spans.push(Span::styled("]", bracket_style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_linear_list(
    frame: &mut Frame,
    app: &App,
    picker: &crate::app::LinearPickerState,
    list_start_y: u16,
    inner: Rect,
    field_width: usize,
    visible_count: usize,
) {
    let filtered = app.filtered_linear_issues();
    let count = filtered.len();

    let imported_ids: HashSet<&str> = app
        .project()
        .issues
        .iter()
        .flat_map(|i| i.linear_links.iter().map(|l| l.id.as_str()))
        .collect();

    let dialog_selected_ids: HashSet<&str> = app
        .dialog
        .as_ref()
        .map(|d| d.linear_issues.iter().map(|l| l.id.as_str()).collect())
        .unwrap_or_default();
    let is_attach = app.linear_picker_context == LinearPickerContext::Attach;

    let scroll = if visible_count == 0 || picker.selected < visible_count {
        0
    } else {
        picker.selected - visible_count + 1
    };

    if count == 0 {
        let empty_area = Rect::new(inner.x + 1, list_start_y, inner.width - 2, 1);
        let msg = if app.active_project().live.linear_issues.is_empty() {
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
            let is_dialog_selected = is_attach && dialog_selected_ids.contains(issue.id.as_str());
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

            let priority_str = if is_dialog_selected {
                "\u{25cf}   "
            } else if is_imported {
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
            let title = styles::truncate(&issue.title, title_budget);

            let title_style = if is_imported {
                styles::dim_style()
            } else {
                Style::default().fg(styles::TEXT)
            };

            let priority_style = if is_dialog_selected || is_imported {
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
}

fn render_github_list(
    frame: &mut Frame,
    app: &App,
    picker: &crate::app::LinearPickerState,
    list_start_y: u16,
    inner: Rect,
    field_width: usize,
    visible_count: usize,
) {
    let filtered = app.filtered_github_prs();
    let count = filtered.len();

    let imported_pr_numbers: HashSet<u32> = app
        .project()
        .issues
        .iter()
        .flat_map(|i| i.pr_numbers())
        .collect();

    let dialog_selected_prs: HashSet<u32> = app
        .dialog
        .as_ref()
        .map(|d| d.github_prs.iter().map(|p| p.number).collect())
        .unwrap_or_default();
    let is_attach = app.linear_picker_context == LinearPickerContext::Attach;

    let scroll = if visible_count == 0 || picker.selected < visible_count {
        0
    } else {
        picker.selected - visible_count + 1
    };

    if count == 0 {
        let empty_area = Rect::new(inner.x + 1, list_start_y, inner.width - 2, 1);
        let msg = if app.active_project().live.pr_statuses.is_empty() {
            "No PRs loaded"
        } else {
            "No matching PRs"
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

            let pr = filtered[idx];
            let is_selected = idx == picker.selected;
            let is_imported = imported_pr_numbers.contains(&pr.number);
            let is_dialog_selected = is_attach && dialog_selected_prs.contains(&pr.number);
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

            let status_str = if is_dialog_selected {
                "\u{25cf} "
            } else if is_imported {
                "\u{2713} "
            } else if pr.is_draft {
                "\u{25cb} "
            } else {
                "  "
            };

            let number_str = format!("#{}", pr.number);
            let author_str = format!(" @{}", pr.author);
            let diff_str = format!(" +{}/-{}", pr.additions, pr.deletions);

            let status_suffix = if is_imported {
                " \u{25cf} on board".to_string()
            } else {
                match pr.state {
                    crate::types::PrState::Merged => " \u{25cf} merged".to_string(),
                    crate::types::PrState::Closed => " \u{25cf} closed".to_string(),
                    _ => String::new(),
                }
            };

            let overhead = 2
                + status_str.len()
                + number_str.len()
                + 1
                + author_str.len()
                + diff_str.len()
                + status_suffix.len();
            let title_budget = field_width.saturating_sub(overhead);
            let title = styles::truncate(&pr.title, title_budget);

            let title_style = if is_imported {
                styles::dim_style()
            } else {
                Style::default().fg(styles::TEXT)
            };

            let status_style = if is_dialog_selected || is_imported {
                Style::default().fg(styles::ACCENT)
            } else {
                Style::default()
            };

            let line = Line::from(vec![
                Span::styled(pointer, pointer_style),
                Span::styled(status_str, status_style),
                Span::styled(number_str, styles::dim_style()),
                Span::raw(" "),
                Span::styled(title, title_style),
                Span::styled(author_str, styles::dim_style()),
                Span::styled(diff_str, styles::dim_style()),
                Span::styled(status_suffix, styles::dim_style()),
            ]);

            frame.render_widget(Paragraph::new(line), row_area);
        }
    }
}
