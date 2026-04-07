use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use crate::app::{App, DialogField};
use crate::external::linear::LinearIssue;
use crate::types::{AgentKind, AgentMode, IssueKind, PrStatus};
use crate::ui::styles;

const DIALOG_HEIGHT: u16 = 34;
const DIALOG_MIN_WIDTH: u16 = 44;
const DIALOG_MAX_WIDTH: u16 = 80;
const PROMPT_VISIBLE_LINES: usize = 12;

pub fn render_dialog(frame: &mut Frame, app: &App) {
    let dialog = match &app.dialog {
        Some(d) => d,
        None => return,
    };

    let area = frame.area();
    let width = (area.width * 60 / 100).clamp(DIALOG_MIN_WIDTH, DIALOG_MAX_WIDTH);
    let height = DIALOG_HEIGHT.min(area.height);
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;

    let dialog_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, dialog_area);

    let dialog_title = if dialog.editing_index.is_some() {
        " Edit Issue "
    } else {
        " New Issue "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(styles::ACCENT))
        .title(Span::styled(
            dialog_title,
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    if inner.height < 12 || inner.width < 10 {
        return;
    }

    let label_width = 10;

    let mut next_row: u16 = 1;

    let kind_area = Rect::new(inner.x + 1, inner.y + next_row, inner.width - 2, 1);
    render_kind_field(
        frame,
        dialog.kind,
        kind_area,
        dialog.current_field() == DialogField::Kind,
        label_width,
    );
    next_row += 2;

    if dialog.kind == IssueKind::Agentic {
        let mode_area = Rect::new(inner.x + 1, inner.y + next_row, inner.width - 2, 1);
        render_mode_field(
            frame,
            &dialog.agent_mode,
            dialog.agent_kind,
            mode_area,
            dialog.current_field() == DialogField::Mode,
            label_width,
        );
        next_row += 2;
    }

    if dialog.linear_available {
        let linear_area = Rect::new(inner.x + 1, inner.y + next_row, inner.width - 2, 1);
        render_linear_field(
            frame,
            &dialog.linear_issue,
            linear_area,
            dialog.current_field() == DialogField::Linear,
            label_width,
        );
        next_row += 2;
    }

    if dialog.github_available {
        let github_area = Rect::new(inner.x + 1, inner.y + next_row, inner.width - 2, 1);
        render_github_pr_field(
            frame,
            &dialog.github_pr,
            github_area,
            dialog.current_field() == DialogField::GithubPr,
            label_width,
        );
        next_row += 2;
    }

    let is_title_focused = dialog.current_field() == DialogField::Title;
    let title_rows = render_title_field(
        frame,
        &TitleFieldParams {
            label: "Title:",
            value: &dialog.title,
            cursor: dialog.title_cursor,
            x: inner.x + 1,
            y: inner.y + next_row,
            total_width: inner.width - 2,
            focused: is_title_focused,
            label_width,
        },
    );
    next_row += title_rows + 1;

    let prompt_label = if dialog.kind == IssueKind::NonAgentic {
        "Notes:"
    } else {
        "Prompt:"
    };

    let available_for_prompt = inner.height.saturating_sub(next_row + 2) as usize;
    let visible_lines = available_for_prompt.clamp(3, PROMPT_VISIBLE_LINES);

    let is_prompt_focused = dialog.current_field() == DialogField::Prompt;

    let label_area = Rect::new(inner.x + 1, inner.y + next_row, label_width as u16, 1);
    let label_style = field_label_style(is_prompt_focused);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("{:<width$}", prompt_label, width = label_width),
            label_style,
        )),
        label_area,
    );

    let prompt_area = Rect::new(
        inner.x + 1 + label_width as u16,
        inner.y + next_row,
        inner.width.saturating_sub(2 + label_width as u16),
        visible_lines as u16,
    );
    render_prompt_field(frame, dialog, prompt_area, is_prompt_focused);

    let footer_y = inner.y + inner.height - 2;
    let on_external_field = dialog.is_on_linear_field() || dialog.is_on_github_field();
    let submit_hint = if dialog.editing_index.is_some() || dialog.kind == IssueKind::NonAgentic {
        ":save  "
    } else {
        ":start  "
    };

    let footer = if on_external_field {
        Line::from(vec![
            Span::styled("Space", styles::statusbar_key_style()),
            Span::styled(":attach  ", styles::statusbar_desc_style()),
            Span::styled("Bksp", styles::statusbar_key_style()),
            Span::styled(":detach  ", styles::statusbar_desc_style()),
            Span::styled("Shift+Enter", styles::statusbar_key_style()),
            Span::styled(submit_hint, styles::statusbar_desc_style()),
            Span::styled("Esc", styles::statusbar_key_style()),
            Span::styled(":cancel", styles::statusbar_desc_style()),
        ])
    } else {
        let next_key = if is_prompt_focused { "Tab" } else { "Enter" };
        let mut spans = vec![
            Span::styled(next_key, styles::statusbar_key_style()),
            Span::styled(":next  ", styles::statusbar_desc_style()),
        ];
        if is_prompt_focused {
            spans.push(Span::styled("Ctrl+e", styles::statusbar_key_style()));
            spans.push(Span::styled(":editor  ", styles::statusbar_desc_style()));
        }
        spans.extend([
            Span::styled("Shift+Enter", styles::statusbar_key_style()),
            Span::styled(submit_hint, styles::statusbar_desc_style()),
            Span::styled("Esc", styles::statusbar_key_style()),
            Span::styled(":cancel", styles::statusbar_desc_style()),
        ]);
        Line::from(spans)
    };
    let footer_area = Rect::new(inner.x + 1, footer_y, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn render_prompt_field(
    frame: &mut Frame,
    dialog: &crate::app::DialogState,
    area: Rect,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let content_len = dialog.prompt.lines().len();
    let cursor_row = dialog.prompt.cursor().0;

    if focused {
        frame.render_widget(&dialog.prompt, area);
    } else {
        let text = dialog.prompt_text();
        let display_lines: Vec<Line> = text
            .lines()
            .map(|l| Line::from(Span::styled(l, styles::dim_style())))
            .collect();
        frame.render_widget(
            Paragraph::new(display_lines).wrap(ratatui::widgets::Wrap { trim: false }),
            area,
        );
    }

    let scrollbar_content = content_len.max(area.height as usize);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_style(Style::default().fg(styles::ACCENT))
        .track_style(styles::dim_style());
    let mut state = ScrollbarState::new(scrollbar_content).position(cursor_row);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

const TITLE_MAX_ROWS: u16 = 3;

struct TitleFieldParams<'a> {
    label: &'a str,
    value: &'a str,
    cursor: usize,
    x: u16,
    y: u16,
    total_width: u16,
    focused: bool,
    label_width: usize,
}

/// Renders the title field with text wrapping. Returns the number of rows used.
fn render_title_field(frame: &mut Frame, params: &TitleFieldParams) -> u16 {
    let TitleFieldParams {
        label,
        value,
        cursor,
        x,
        y,
        total_width,
        focused,
        label_width,
    } = *params;

    let label_style = field_label_style(focused);
    let value_style = field_value_style(focused);

    let value_width = total_width.saturating_sub(label_width as u16) as usize;
    if value_width == 0 {
        return 1;
    }

    let chars: Vec<char> = value.chars().collect();
    let char_count = chars.len();
    let safe_cursor = cursor.min(char_count);

    // Build visual lines by wrapping at value_width
    let mut visual_lines: Vec<String> = Vec::new();
    let mut cursor_visual_row: usize = 0;
    let mut cursor_visual_col: usize = 0;

    if chars.is_empty() {
        visual_lines.push(String::new());
        cursor_visual_row = 0;
        cursor_visual_col = 0;
    } else {
        let mut col = 0;
        while col < chars.len() {
            let end = (col + value_width).min(chars.len());
            let segment: String = chars[col..end].iter().collect();

            if safe_cursor >= col && safe_cursor < end {
                cursor_visual_row = visual_lines.len();
                cursor_visual_col = safe_cursor - col;
            }

            visual_lines.push(segment);
            col = end;
        }

        // Cursor at end of text
        if safe_cursor >= chars.len() {
            if chars.len().is_multiple_of(value_width) {
                visual_lines.push(String::new());
                cursor_visual_row = visual_lines.len() - 1;
                cursor_visual_col = 0;
            } else {
                let last_segment_start = chars.len() - (chars.len() % value_width);
                cursor_visual_row = visual_lines.len() - 1;
                cursor_visual_col = safe_cursor - last_segment_start;
            }
        }
    }

    let display_rows = (visual_lines.len() as u16).clamp(1, TITLE_MAX_ROWS);

    // Scroll so cursor is visible
    let scroll_offset = if cursor_visual_row >= display_rows as usize {
        cursor_visual_row - display_rows as usize + 1
    } else {
        0
    };

    // Render label on the first row
    let label_area = Rect::new(x, y, label_width as u16, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("{:<width$}", label, width = label_width),
            label_style,
        )),
        label_area,
    );

    // Render wrapped text lines
    let value_x = x + label_width as u16;
    let visible_end = (scroll_offset + display_rows as usize).min(visual_lines.len());
    for (i, line) in visual_lines[scroll_offset..visible_end].iter().enumerate() {
        let line_area = Rect::new(value_x, y + i as u16, value_width as u16, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(line.as_str(), value_style)),
            line_area,
        );
    }

    // Render cursor only when focused
    if focused {
        let cursor_screen_row = cursor_visual_row.saturating_sub(scroll_offset);
        if (cursor_screen_row as u16) < display_rows {
            let cursor_x = value_x + cursor_visual_col as u16;
            let cursor_y = y + cursor_screen_row as u16;

            if cursor_x < value_x + value_width as u16 {
                let cursor_char = visual_lines
                    .get(cursor_visual_row)
                    .and_then(|l| l.chars().nth(cursor_visual_col))
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| " ".to_string());

                let cursor_area = Rect::new(cursor_x, cursor_y, 1, 1);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        cursor_char,
                        Style::default()
                            .fg(styles::ACCENT)
                            .add_modifier(Modifier::REVERSED),
                    )),
                    cursor_area,
                );
            }
        }
    }

    display_rows
}

fn render_kind_field(
    frame: &mut Frame,
    kind: IssueKind,
    area: Rect,
    focused: bool,
    label_width: usize,
) {
    let label_style = field_label_style(focused);
    let selected_style = Style::default()
        .fg(styles::ACCENT)
        .add_modifier(Modifier::BOLD);
    let unselected_style = styles::dim_style();
    let indicator = |selected: bool| if selected { "\u{25cf}" } else { "\u{25cb}" };

    let is_agentic = kind == IssueKind::Agentic;
    let is_todo = kind == IssueKind::NonAgentic;

    let line = Line::from(vec![
        Span::styled(
            format!("{:<width$}", "Kind:", width = label_width),
            label_style,
        ),
        Span::styled(
            format!("[{} Agentic]", indicator(is_agentic)),
            if is_agentic {
                selected_style
            } else {
                unselected_style
            },
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{} Todo]", indicator(is_todo)),
            if is_todo {
                selected_style
            } else {
                unselected_style
            },
        ),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn render_linear_field(
    frame: &mut Frame,
    linear_issue: &Option<LinearIssue>,
    area: Rect,
    focused: bool,
    label_width: usize,
) {
    let label_style = field_label_style(focused);

    let mut spans = vec![Span::styled(
        format!("{:<width$}", "Linear:", width = label_width),
        label_style,
    )];

    match linear_issue {
        Some(li) => {
            spans.push(Span::styled(
                &li.identifier,
                Style::default()
                    .fg(styles::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
            let remaining = (area.width as usize)
                .saturating_sub(label_width)
                .saturating_sub(li.identifier.len())
                .saturating_sub(3);
            if remaining > 4 {
                let title_display: String = li.title.chars().take(remaining).collect();
                spans.push(Span::styled(" \u{2022} ", styles::dim_style()));
                spans.push(Span::styled(title_display, styles::dim_style()));
            }
        }
        None => {
            spans.push(Span::styled("\u{2014}", styles::dim_style()));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_github_pr_field(
    frame: &mut Frame,
    github_pr: &Option<PrStatus>,
    area: Rect,
    focused: bool,
    label_width: usize,
) {
    let label_style = field_label_style(focused);

    let mut spans = vec![Span::styled(
        format!("{:<width$}", "GitHub:", width = label_width),
        label_style,
    )];

    match github_pr {
        Some(pr) => {
            let number_str = format!("#{}", pr.number);
            let num_len = number_str.len();
            spans.push(Span::styled(
                number_str,
                Style::default()
                    .fg(styles::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
            let remaining = (area.width as usize)
                .saturating_sub(label_width)
                .saturating_sub(num_len)
                .saturating_sub(3);
            if remaining > 4 {
                let title_display: String = pr.title.chars().take(remaining).collect();
                spans.push(Span::styled(" \u{2022} ", styles::dim_style()));
                spans.push(Span::styled(title_display, styles::dim_style()));
            }
        }
        None => {
            spans.push(Span::styled("\u{2014}", styles::dim_style()));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_mode_field(
    frame: &mut Frame,
    mode: &AgentMode,
    agent_kind: AgentKind,
    area: Rect,
    focused: bool,
    label_width: usize,
) {
    let label_style = field_label_style(focused);

    let selected_style = Style::default()
        .fg(styles::ACCENT)
        .add_modifier(Modifier::BOLD);
    let unselected_style = styles::dim_style();

    let indicator = |selected: bool| if selected { "\u{25cf}" } else { "\u{25cb}" };

    let plan_selected = *mode == AgentMode::Plan;
    let build_selected = *mode == AgentMode::Build;
    let yolo_selected = *mode == AgentMode::Yolo;

    let mut spans = vec![
        Span::styled(
            format!("{:<width$}", "Mode:", width = label_width),
            label_style,
        ),
        Span::styled(
            format!("[{} plan]", indicator(plan_selected)),
            if plan_selected {
                selected_style
            } else {
                unselected_style
            },
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{} build]", indicator(build_selected)),
            if build_selected {
                selected_style
            } else {
                unselected_style
            },
        ),
    ];

    if agent_kind == AgentKind::Claude {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("[{} yolo]", indicator(yolo_selected)),
            if yolo_selected {
                selected_style
            } else {
                unselected_style
            },
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn field_label_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        styles::dim_style()
    }
}

fn field_value_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(styles::TEXT)
    } else {
        styles::dim_style()
    }
}
