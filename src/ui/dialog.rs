use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::types::AgentMode;
use crate::ui::styles;

const DIALOG_HEIGHT: u16 = 15;
const DIALOG_MIN_WIDTH: u16 = 44;
const DIALOG_MAX_WIDTH: u16 = 80;

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

    // Clear the area behind the dialog
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

    if inner.height < 10 || inner.width < 10 {
        return;
    }

    let field_width = inner.width.saturating_sub(2) as usize;
    let label_width = 10;
    let field_rect = |row: u16| Rect::new(inner.x + 1, inner.y + row, inner.width - 2, 1);

    let fields: &[(&str, &str, usize)] = &[
        ("Title:", &dialog.title, 0),
        ("Prompt:", &dialog.prompt, 1),
        ("Worktree:", &dialog.worktree, 2),
    ];
    for (i, &(label, value, field_idx)) in fields.iter().enumerate() {
        let row = 1 + (i as u16 * 2);
        render_field(
            frame,
            label,
            value,
            field_rect(row),
            dialog.focused_field == field_idx,
            label_width,
            field_width,
        );
    }

    render_mode_field(
        frame,
        &dialog.agent_mode,
        field_rect(7),
        dialog.focused_field == 3,
        label_width,
    );

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

fn render_field(
    frame: &mut Frame,
    label: &str,
    value: &str,
    area: Rect,
    focused: bool,
    label_width: usize,
    field_width: usize,
) {
    let label_style = if focused {
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        styles::dim_style()
    };

    let value_style = if focused {
        Style::default().fg(styles::TEXT)
    } else {
        styles::dim_style()
    };

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

fn render_mode_field(
    frame: &mut Frame,
    mode: &AgentMode,
    area: Rect,
    focused: bool,
    label_width: usize,
) {
    let label_style = if focused {
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        styles::dim_style()
    };

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
