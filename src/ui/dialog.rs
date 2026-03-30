use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::external::linear::LinearIssue;
use crate::types::{AgentKind, AgentMode, IssueKind};
use crate::ui::styles;

const DIALOG_HEIGHT: u16 = 22;
const DIALOG_MIN_WIDTH: u16 = 44;
const DIALOG_MAX_WIDTH: u16 = 80;
const PROMPT_VISIBLE_LINES: usize = 3;

pub fn render_dialog(frame: &mut Frame, app: &App) {
    let dialog = match &app.dialog {
        Some(d) => d,
        None => return,
    };

    let area = frame.area();
    let width = (area.width * 60 / 100)
        .max(DIALOG_MIN_WIDTH)
        .min(DIALOG_MAX_WIDTH);
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

    let field_width = inner.width.saturating_sub(2) as usize;
    let label_width = 10;

    // --- Kind field (row 1) ---
    let kind_area = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, 1);
    render_kind_field(
        frame,
        dialog.kind,
        kind_area,
        dialog.focused_field == 0,
        label_width,
    );

    // --- Title field (row 3) ---
    let title_area = Rect::new(inner.x + 1, inner.y + 3, inner.width - 2, 1);
    render_single_line_field(
        frame,
        "Title:",
        &dialog.title,
        dialog.title_cursor,
        title_area,
        dialog.focused_field == 1,
        label_width,
        field_width,
    );

    // --- Prompt/notes field (rows 5-7, 3 visible lines) ---
    let prompt_label = if dialog.kind == IssueKind::NonAgentic {
        "Notes:"
    } else {
        "Prompt:"
    };
    let prompt_area = Rect::new(
        inner.x + 1,
        inner.y + 5,
        inner.width - 2,
        PROMPT_VISIBLE_LINES as u16,
    );
    render_multiline_field(
        frame,
        prompt_label,
        &dialog.prompt,
        dialog.prompt_cursor,
        prompt_area,
        dialog.focused_field == 2,
        label_width,
        field_width,
    );

    let mut next_row = 5 + PROMPT_VISIBLE_LINES as u16 + 1;

    if dialog.kind == IssueKind::Agentic {
        // --- Mode field (after the 3-line prompt) ---
        let mode_area = Rect::new(inner.x + 1, inner.y + next_row, inner.width - 2, 1);
        render_mode_field(
            frame,
            &dialog.agent_mode,
            dialog.agent_kind,
            mode_area,
            dialog.focused_field == 3,
            label_width,
        );
        next_row += 2;
    }

    // --- Linear field (after mode for Agentic, after prompt for NonAgentic) ---
    if dialog.linear_available {
        let linear_field_idx = dialog.linear_field_index().unwrap_or(99);
        let linear_area = Rect::new(inner.x + 1, inner.y + next_row, inner.width - 2, 1);
        render_linear_field(
            frame,
            &dialog.linear_issue,
            linear_area,
            dialog.focused_field == linear_field_idx,
            label_width,
        );
    }

    // --- Footer hints ---
    let footer_y = inner.y + inner.height - 2;
    let on_linear = dialog.is_on_linear_field();
    let submit_hint = if dialog.editing_index.is_some() || dialog.kind == IssueKind::NonAgentic {
        ":save  "
    } else {
        ":start  "
    };

    let footer = if on_linear {
        Line::from(vec![
            Span::styled("Enter", styles::statusbar_key_style()),
            Span::styled(":attach  ", styles::statusbar_desc_style()),
            Span::styled("Bksp", styles::statusbar_key_style()),
            Span::styled(":detach  ", styles::statusbar_desc_style()),
            Span::styled("Shift+Enter", styles::statusbar_key_style()),
            Span::styled(submit_hint, styles::statusbar_desc_style()),
            Span::styled("Esc", styles::statusbar_key_style()),
            Span::styled(":cancel", styles::statusbar_desc_style()),
        ])
    } else {
        Line::from(vec![
            Span::styled("Enter", styles::statusbar_key_style()),
            Span::styled(":next  ", styles::statusbar_desc_style()),
            Span::styled("Shift+Enter", styles::statusbar_key_style()),
            Span::styled(submit_hint, styles::statusbar_desc_style()),
            Span::styled("Esc", styles::statusbar_key_style()),
            Span::styled(":cancel", styles::statusbar_desc_style()),
        ])
    };
    let footer_area = Rect::new(inner.x + 1, footer_y, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn render_single_line_field(
    frame: &mut Frame,
    label: &str,
    value: &str,
    cursor: usize,
    area: Rect,
    focused: bool,
    label_width: usize,
    field_width: usize,
) {
    let label_style = field_label_style(focused);
    let value_style = field_value_style(focused);

    let max_value_len = field_width.saturating_sub(label_width + 1);
    let char_count = value.chars().count();
    let safe_cursor = cursor.min(char_count);
    let start = safe_cursor.saturating_sub(max_value_len);
    let display_value: String = value.chars().skip(start).take(max_value_len).collect();

    let mut spans = vec![
        Span::styled(
            format!("{:<width$}", label, width = label_width),
            label_style,
        ),
        Span::styled(display_value, value_style),
    ];

    if focused {
        spans.push(Span::styled(
            "\u{2588}",
            Style::default().fg(styles::ACCENT),
        ));
    }

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_multiline_field(
    frame: &mut Frame,
    label: &str,
    value: &str,
    cursor: usize,
    area: Rect,
    focused: bool,
    label_width: usize,
    field_width: usize,
) {
    let label_style = field_label_style(focused);
    let value_style = field_value_style(focused);

    let chars_per_line = field_width.saturating_sub(label_width + 1);
    if chars_per_line == 0 {
        return;
    }

    // Word-wrap the value into visual lines
    let wrapped = wrap_text(value, chars_per_line);
    let visible_lines = area.height as usize;

    let line_count = wrapped.len().max(1);
    let cursor_line = cursor_line_index(&wrapped, cursor.min(value.chars().count()));
    let mut start = 0usize;
    if line_count > visible_lines && cursor_line + 1 > visible_lines {
        start = cursor_line + 1 - visible_lines;
    }
    let end = (start + visible_lines).min(line_count);
    let visible = if wrapped.is_empty() {
        Vec::new()
    } else {
        wrapped[start..end].to_vec()
    };

    for (i, line_text) in visible.iter().enumerate() {
        let y = area.y + i as u16;
        if y >= area.y + area.height {
            break;
        }

        let prefix = if i == 0 {
            format!("{:<width$}", label, width = label_width)
        } else {
            " ".repeat(label_width)
        };

        let prefix_style = if i == 0 { label_style } else { label_style };

        let is_cursor_line = start + i == cursor_line;

        let mut spans = vec![
            Span::styled(prefix, prefix_style),
            Span::styled(line_text.clone(), value_style),
        ];

        if focused && is_cursor_line {
            spans.push(Span::styled(
                "\u{2588}",
                Style::default().fg(styles::ACCENT),
            ));
        }

        let line = Line::from(spans);
        let line_area = Rect::new(area.x, y, area.width, 1);
        frame.render_widget(Paragraph::new(line), line_area);
    }

    // If value is empty and we have space, show cursor on first line
    if wrapped.is_empty() {
        let mut spans = vec![Span::styled(
            format!("{:<width$}", label, width = label_width),
            label_style,
        )];
        if focused {
            spans.push(Span::styled(
                "\u{2588}",
                Style::default().fg(styles::ACCENT),
            ));
        }
        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line), area);
    }
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

fn cursor_line_index(lines: &[String], cursor: usize) -> usize {
    if lines.is_empty() {
        return 0;
    }

    let mut seen = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let line_len = line.chars().count();
        if cursor <= seen + line_len {
            return i;
        }
        seen += line_len;
    }

    lines.len() - 1
}

/// Simple word-wrap: break text into lines of at most `max_width` characters.
/// Breaks on word boundaries when possible, otherwise hard-wraps.
/// Preserves trailing spaces so the cursor position stays accurate.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut line_len: usize = 0;

    // Split into segments of (whitespace, word) pairs to preserve spaces
    let mut chars = text.char_indices().peekable();
    while chars.peek().is_some() {
        // Collect a word (non-space characters)
        let mut word = String::new();
        while let Some(&(_, c)) = chars.peek() {
            if c == ' ' {
                break;
            }
            word.push(c);
            chars.next();
        }

        if !word.is_empty() {
            let word_len = word.chars().count();

            if line_len == 0 {
                if word_len > max_width {
                    let mut wchars = word.chars();
                    while wchars.clone().count() > 0 {
                        let chunk: String = wchars.by_ref().take(max_width).collect();
                        if chunk.is_empty() {
                            break;
                        }
                        let chunk_len = chunk.chars().count();
                        if wchars.clone().count() > 0 {
                            lines.push(chunk);
                        } else {
                            current_line = chunk;
                            line_len = chunk_len;
                        }
                    }
                } else {
                    current_line.push_str(&word);
                    line_len += word_len;
                }
            } else if line_len + word_len <= max_width {
                current_line.push_str(&word);
                line_len += word_len;
            } else {
                lines.push(current_line);
                if word_len > max_width {
                    let mut wchars = word.chars();
                    current_line = String::new();
                    line_len = 0;
                    while wchars.clone().count() > 0 {
                        let chunk: String = wchars.by_ref().take(max_width).collect();
                        if chunk.is_empty() {
                            break;
                        }
                        let chunk_len = chunk.chars().count();
                        if wchars.clone().count() > 0 {
                            lines.push(chunk);
                        } else {
                            current_line = chunk;
                            line_len = chunk_len;
                        }
                    }
                } else {
                    current_line = word;
                    line_len = word_len;
                }
            }
        }

        // Collect spaces after the word
        while let Some(&(_, c)) = chars.peek() {
            if c != ' ' {
                break;
            }
            if line_len >= max_width {
                lines.push(current_line);
                current_line = String::new();
                line_len = 0;
            }
            current_line.push(' ');
            line_len += 1;
            chars.next();
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
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
            let remaining = area.width as usize - label_width - li.identifier.len() - 3;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_empty_string() {
        assert_eq!(wrap_text("", 20), Vec::<String>::new());
    }

    #[test]
    fn wrap_text_single_word_fits() {
        assert_eq!(wrap_text("hello", 20), vec!["hello"]);
    }

    #[test]
    fn wrap_text_multiple_words_fit_one_line() {
        assert_eq!(wrap_text("hello world", 20), vec!["hello world"]);
    }

    #[test]
    fn wrap_text_wraps_at_word_boundary() {
        assert_eq!(
            wrap_text("hello world foo", 11),
            vec!["hello world", " foo"]
        );
    }

    #[test]
    fn wrap_text_hard_wraps_long_word() {
        assert_eq!(wrap_text("abcdefghij", 5), vec!["abcde", "fghij"]);
    }

    #[test]
    fn wrap_text_preserves_trailing_space() {
        let result = wrap_text("hello ", 20);
        assert_eq!(result, vec!["hello "]);
    }

    #[test]
    fn wrap_text_preserves_space_between_words() {
        let result = wrap_text("a b", 20);
        assert_eq!(result, vec!["a b"]);
    }

    #[test]
    fn wrap_text_trailing_space_after_wrap() {
        let result = wrap_text("hello world ", 20);
        assert_eq!(result, vec!["hello world "]);
    }

    #[test]
    fn wrap_text_multiple_trailing_spaces() {
        let result = wrap_text("hi   ", 20);
        assert_eq!(result, vec!["hi   "]);
    }

    #[test]
    fn wrap_text_space_causes_line_wrap() {
        // Line is exactly at max_width, then space pushes to next line
        let result = wrap_text("12345 ", 5);
        assert_eq!(result, vec!["12345", " "]);
    }

    #[test]
    fn wrap_text_multiple_lines_with_trailing_space() {
        let result = wrap_text("aaa bbb ccc ", 7);
        assert_eq!(result, vec!["aaa bbb", " ccc "]);
    }

    #[test]
    fn wrap_text_just_spaces() {
        let result = wrap_text("   ", 10);
        assert_eq!(result, vec!["   "]);
    }

    #[test]
    fn wrap_text_word_exactly_max_width() {
        assert_eq!(wrap_text("hello", 5), vec!["hello"]);
    }

    #[test]
    fn wrap_text_two_words_exactly_max_width() {
        assert_eq!(wrap_text("ab cd", 5), vec!["ab cd"]);
    }

    #[test]
    fn wrap_text_long_sentence_wraps_correctly() {
        let result = wrap_text("the quick brown fox", 10);
        assert_eq!(result, vec!["the quick ", "brown fox"]);
    }
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
