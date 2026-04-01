use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
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

    let title_area = Rect::new(inner.x + 1, inner.y + next_row, inner.width - 2, 1);
    render_single_line_field(
        frame,
        "Title:",
        &dialog.title,
        dialog.title_cursor,
        title_area,
        dialog.current_field() == DialogField::Title,
        label_width,
        field_width,
    );
    next_row += 2;

    let prompt_label = if dialog.kind == IssueKind::NonAgentic {
        "Notes:"
    } else {
        "Prompt:"
    };

    let available_for_prompt = inner.height.saturating_sub(next_row + 2) as usize;
    let visible_lines = available_for_prompt.min(PROMPT_VISIBLE_LINES).max(3);

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
    frame.render_widget(&dialog.prompt, prompt_area);

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
        Line::from(vec![
            Span::styled(next_key, styles::statusbar_key_style()),
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
            let remaining = area.width as usize - label_width - num_len - 3;
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
