use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::types::AgentMode;
use crate::ui::styles;

const DIALOG_HEIGHT: u16 = 21;
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

    // --- Title field (row 1) ---
    let title_area = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, 1);
    render_single_line_field(
        frame,
        "Title:",
        &dialog.title,
        title_area,
        dialog.focused_field == 0,
        label_width,
        field_width,
    );

    // --- Prompt field (rows 3-5, 3 visible lines) ---
    let prompt_area = Rect::new(
        inner.x + 1,
        inner.y + 3,
        inner.width - 2,
        PROMPT_VISIBLE_LINES as u16,
    );
    render_multiline_field(
        frame,
        "Prompt:",
        &dialog.prompt,
        prompt_area,
        dialog.focused_field == 1,
        label_width,
        field_width,
    );

    // --- Worktree field (row 7, after the 3-line prompt) ---
    let worktree_row = 3 + PROMPT_VISIBLE_LINES as u16 + 1;
    let worktree_area = Rect::new(inner.x + 1, inner.y + worktree_row, inner.width - 2, 1);
    render_single_line_field(
        frame,
        "Worktree:",
        &dialog.worktree,
        worktree_area,
        dialog.focused_field == 2,
        label_width,
        field_width,
    );

    // --- Mode field (row 9) ---
    let mode_row = worktree_row + 2;
    let mode_area = Rect::new(inner.x + 1, inner.y + mode_row, inner.width - 2, 1);
    render_mode_field(
        frame,
        &dialog.agent_mode,
        mode_area,
        dialog.focused_field == 3,
        label_width,
    );

    // --- Footer hints ---
    let footer_y = inner.y + inner.height - 2;
    let submit_hint = if dialog.editing_index.is_some() {
        ":save  "
    } else {
        ":start  "
    };
    let footer = Line::from(vec![
        Span::styled("Enter", styles::statusbar_key_style()),
        Span::styled(":next  ", styles::statusbar_desc_style()),
        Span::styled("Shift+Enter", styles::statusbar_key_style()),
        Span::styled(submit_hint, styles::statusbar_desc_style()),
        Span::styled("Esc", styles::statusbar_key_style()),
        Span::styled(":cancel", styles::statusbar_desc_style()),
    ]);
    let footer_area = Rect::new(inner.x + 1, footer_y, inner.width - 2, 1);
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn render_single_line_field(
    frame: &mut Frame,
    label: &str,
    value: &str,
    area: Rect,
    focused: bool,
    label_width: usize,
    field_width: usize,
) {
    let label_style = field_label_style(focused);
    let value_style = field_value_style(focused);

    let max_value_len = field_width.saturating_sub(label_width + 1);
    let char_count = value.chars().count();
    let display_value = if char_count > max_value_len && max_value_len > 3 {
        let skip = char_count - (max_value_len - 3);
        let tail: String = value.chars().skip(skip).collect();
        format!("...{}", tail)
    } else {
        value.to_string()
    };

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

    // Show the last N lines so the cursor is always visible
    let start = if wrapped.len() > visible_lines {
        wrapped.len() - visible_lines
    } else {
        0
    };
    let visible = &wrapped[start..];

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

        let is_last_line = i == visible.len() - 1;

        let mut spans = vec![
            Span::styled(prefix, prefix_style),
            Span::styled(line_text.clone(), value_style),
        ];

        if focused && is_last_line {
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

/// Simple word-wrap: break text into lines of at most `max_width` characters.
/// Breaks on word boundaries when possible, otherwise hard-wraps.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        let word_len = word.chars().count();

        if current_line.is_empty() {
            if word_len > max_width {
                // Hard-wrap long words
                let mut chars = word.chars();
                while chars.clone().count() > 0 {
                    let chunk: String = chars.by_ref().take(max_width).collect();
                    if chunk.is_empty() {
                        break;
                    }
                    lines.push(chunk);
                }
            } else {
                current_line = word.to_string();
            }
        } else {
            let new_len = current_line.chars().count() + 1 + word_len;
            if new_len <= max_width {
                current_line.push(' ');
                current_line.push_str(word);
            } else {
                lines.push(current_line);
                if word_len > max_width {
                    let mut chars = word.chars();
                    while chars.clone().count() > 0 {
                        let chunk: String = chars.by_ref().take(max_width).collect();
                        if chunk.is_empty() {
                            break;
                        }
                        lines.push(chunk);
                    }
                    current_line = String::new();
                } else {
                    current_line = word.to_string();
                }
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

fn render_mode_field(
    frame: &mut Frame,
    mode: &AgentMode,
    area: Rect,
    focused: bool,
    label_width: usize,
) {
    let label_style = field_label_style(focused);

    let plan_selected = *mode == AgentMode::Plan;
    let build_selected = *mode == AgentMode::Build;

    let plan_style = if plan_selected {
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        styles::dim_style()
    };

    let build_style = if build_selected {
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        styles::dim_style()
    };

    let plan_indicator = if plan_selected {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };
    let build_indicator = if build_selected {
        "\u{25cf}"
    } else {
        "\u{25cb}"
    };

    let line = Line::from(vec![
        Span::styled(
            format!("{:<width$}", "Mode:", width = label_width),
            label_style,
        ),
        Span::styled(format!("[{} plan]", plan_indicator), plan_style),
        Span::raw("  "),
        Span::styled(format!("[{} build]", build_indicator), build_style),
    ]);

    frame.render_widget(Paragraph::new(line), area);
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
