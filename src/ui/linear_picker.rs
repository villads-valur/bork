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
    let width = (area.width * 70 / 100)
        .clamp(PICKER_MIN_WIDTH, PICKER_MAX_WIDTH)
        .min(area.width);
    let height = (VISIBLE_ITEMS as u16 + if show_tabs { 11 } else { 8 }).min(area.height);
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
    let available_rows = inner.height.saturating_sub(row_y - inner.y + 3) as usize;
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

    if app.picker_tab == ImportSource::GitHub {
        let hint = if app.active_project().live.gh_missing {
            ""
        } else if let Some((
            message,
            crate::app::MessageKind::Warning | crate::app::MessageKind::Error,
        )) = &app.message
        {
            message.as_str()
        } else if let Some(error) = &app.active_project().live.github_error {
            error.as_str()
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(hint).style(styles::dim_style()),
            Rect::new(inner.x + 1, inner.y + inner.height - 2, inner.width - 2, 1),
        );
    }
    let footer_y = inner.y + inner.height - 1;
    let footer_area = Rect::new(inner.x + 1, footer_y, inner.width - 2, 1);
    let loading =
        app.picker_tab == ImportSource::GitHub && app.active_project().live.github_fetching();
    let mut content_area = footer_area;
    if loading {
        content_area.width = content_area
            .width
            .saturating_sub(super::status_bar::SPINNER_WIDTH);
    }

    let count = match app.picker_tab {
        ImportSource::Linear => app.filtered_linear_issues().len(),
        ImportSource::GitHub => app.filtered_github_prs().len(),
    };

    let footer = picker_footer(
        app.picker_tab,
        app.linear_picker_context,
        count.min(picker.selected + 1),
        count,
        content_area.width as usize,
        app.active_project().live.stacks_available && !app.active_project().live.gh_missing,
    );
    frame.render_widget(Paragraph::new(footer), content_area);
    if loading {
        super::status_bar::render_loading_spinner(frame, app, footer_area);
    }
}

fn picker_footer(
    source: ImportSource,
    context: LinearPickerContext,
    selected: usize,
    count: usize,
    width: usize,
    stacks_available: bool,
) -> Line<'static> {
    let select = if context == LinearPickerContext::Attach {
        "toggle"
    } else {
        "import"
    };
    let counter = format!("{selected}/{count}");
    let mut bindings = vec![("Enter", select)];
    if source == ImportSource::GitHub && stacks_available {
        bindings.push(("Ctrl+s", "stack"));
    }
    let required = bindings
        .iter()
        .map(|(key, label)| key.len() + label.len() + 3)
        .sum::<usize>()
        + "Esc close".len()
        + counter.len()
        + 2;
    if required + "Ctrl+r refresh  ".len() <= width {
        bindings.push(("Ctrl+r", "refresh"));
    }
    bindings.push(("Esc", "close"));
    let mut spans = Vec::new();
    for (index, (key, label)) in bindings.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(*key, styles::statusbar_key_style()));
        spans.push(Span::styled(
            format!(" {label}"),
            styles::statusbar_desc_style(),
        ));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used + counter.len()).max(1)),
    ));
    spans.push(Span::styled(counter, styles::dim_style()));
    Line::from(spans)
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
        .active_project()
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
        .active_project()
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
            } else if pr.status.is_some_and(|status| status.is_draft) {
                "\u{25cb} "
            } else {
                "  "
            };

            let number_str = format!("#{}", pr.number);
            let author_str = pr
                .status
                .map(|status| format!(" @{}", status.author))
                .unwrap_or_default();
            let stack = app
                .active_project()
                .stack_for_pr(pr.number)
                .filter(|_| app.active_project().live.stacks_available);
            let stack_str = stack
                .map(|stack| {
                    let attached = if is_attach {
                        app.dialog
                            .as_ref()
                            .is_some_and(|dialog| dialog.github_stack == Some(stack.number))
                    } else {
                        app.active_project()
                            .issues
                            .iter()
                            .any(|issue| issue.github_stack == Some(stack.number))
                    };
                    let marker = if attached { "attached" } else { "stack" };
                    format!(
                        " {marker} #{} · {} PRs",
                        stack.number,
                        stack.pull_requests.len()
                    )
                })
                .unwrap_or_default();

            let status_suffix = if is_imported {
                " \u{25cf} on board".to_string()
            } else {
                match pr.status.map(|status| status.state) {
                    Some(crate::types::PrState::Merged) => " \u{25cf} merged".to_string(),
                    Some(crate::types::PrState::Closed) => " \u{25cf} closed".to_string(),
                    _ => String::new(),
                }
            };

            let overhead = 2
                + status_str.len()
                + number_str.len()
                + 1
                + author_str.len()
                + stack_str.chars().count()
                + status_suffix.len();
            let title_budget = field_width.saturating_sub(overhead);
            let title = if app.active_project().live.gh_missing
                || (pr.status.is_none() && app.active_project().live.github_loading())
            {
                ""
            } else if pr.status.is_none() {
                app.active_project().live.missing_github_status()
            } else {
                pr.title()
            };
            let title = styles::truncate(title, title_budget);

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
                Span::styled(stack_str, Style::default().fg(styles::ACCENT)),
                Span::styled(status_suffix, styles::dim_style()),
            ]);

            frame.render_widget(Paragraph::new(line), row_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_footer_hides_optional_stack_shortcut() {
        let line = picker_footer(
            ImportSource::GitHub,
            LinearPickerContext::Attach,
            1,
            3,
            76,
            false,
        );
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains("Ctrl+s"));
        assert!(text.contains("Enter toggle"));
    }

    #[test]
    fn github_footer_keeps_primary_actions_and_counter_at_narrow_widths() {
        for width in [46, 76, 96] {
            let line = picker_footer(
                ImportSource::GitHub,
                LinearPickerContext::Attach,
                1,
                554,
                width,
                true,
            );
            assert!(line.width() <= width);
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.contains("Enter toggle"));
            assert!(text.contains("Ctrl+s stack"));
            assert!(text.contains("Esc close"));
            assert!(text.ends_with("1/554"));
            assert_eq!(text.matches("Enter").count(), 1);
        }
    }
}
