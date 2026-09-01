use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, PruneDialogState};
use crate::prune::{PruneAction, PruneCandidate};
use crate::types::Column;
use crate::ui::styles;

const DIALOG_WIDTH: u16 = 88;
const MIN_HEIGHT: u16 = 10;

pub fn render_prune_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = app.prune_dialog.as_ref() else {
        return;
    };

    let area = frame.area();
    let width = DIALOG_WIDTH.min(area.width);
    // Header (1) + list rows + footer (3 for instructions + optional error)
    let list_height = (dialog.candidates.len() as u16).clamp(1, 16);
    let footer_height = if dialog.error.is_some() { 4 } else { 3 };
    let height = (list_height + footer_height + 2)
        .max(MIN_HEIGHT)
        .min(area.height);

    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(styles::ACCENT))
        .title(Span::styled(
            " Prune worktrees ",
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 4 || inner.width < 20 {
        return;
    }

    let list_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(footer_height),
    );
    render_list(frame, dialog, list_area);

    let footer_y = inner.y + inner.height.saturating_sub(footer_height);
    render_footer(
        frame,
        dialog,
        Rect::new(inner.x, footer_y, inner.width, footer_height),
    );
}

fn render_list(frame: &mut Frame, dialog: &PruneDialogState, area: Rect) {
    let mut rows: Vec<Line> = Vec::with_capacity(dialog.candidates.len());
    let visible = area.height as usize;
    let selected = dialog.selected;

    // Simple scroll: keep selected row visible within viewport
    let scroll = if dialog.candidates.len() <= visible || selected < visible / 2 {
        0
    } else if selected + visible / 2 >= dialog.candidates.len() {
        dialog.candidates.len() - visible
    } else {
        selected - visible / 2
    };

    for (i, candidate) in dialog
        .candidates
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
    {
        rows.push(format_row(candidate, i == selected, area.width));
    }
    frame.render_widget(Paragraph::new(rows), area);
}

fn format_row(candidate: &PruneCandidate, selected: bool, width: u16) -> Line<'static> {
    let (checkbox, checkbox_color) = match candidate.action {
        PruneAction::Remove => ("[x] ", Color::Red),
        PruneAction::Keep => ("[ ] ", styles::TEXT),
    };

    let issue_label = candidate.issue_id.as_deref().unwrap_or("(orphan)");
    let row_style = if selected {
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let prefix = if selected { "▸ " } else { "  " };

    // Reserve ~36 columns for the prefix, checkbox, issue label, and state.
    let wt_width = (width as usize).saturating_sub(36).max(8);
    let wt = styles::truncate(&candidate.worktree, wt_width);
    let wt_pad = format!("{:<wt_width$}", wt);

    Line::from(vec![
        Span::styled(prefix, row_style),
        Span::styled(checkbox, Style::default().fg(checkbox_color)),
        Span::styled(wt_pad, row_style),
        Span::styled(
            format!("{:<10}", styles::truncate(issue_label, 10)),
            styles::dim_style(),
        ),
        Span::styled(
            format!("{:<12}", state_label_for(candidate)),
            Style::default().fg(state_color_for(candidate)),
        ),
    ])
}

fn state_label_for(c: &PruneCandidate) -> String {
    if c.session_alive {
        return "● session".to_string();
    }
    let Some(status) = c.status.as_ref() else {
        // Git poll hasn't reached this worktree yet.
        return "? unknown".to_string();
    };
    if !status.is_clean() {
        return format!("◌ {} dirty", status.staged + status.unstaged);
    }
    match c.issue_column {
        Some(Column::Done) => "○ done".to_string(),
        Some(Column::InProgress) => "○ in-prog".to_string(),
        Some(Column::Todo) => "○ todo".to_string(),
        Some(Column::CodeReview) => "○ review".to_string(),
        None => "○ orphan".to_string(),
    }
}

fn state_color_for(c: &PruneCandidate) -> Color {
    if c.session_alive {
        return Color::Green;
    }
    if c.is_dirty() {
        return Color::Yellow;
    }
    Color::Gray
}

fn render_footer(frame: &mut Frame, dialog: &PruneDialogState, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(err) = dialog.error.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("  {}", err),
            Style::default().fg(Color::Red),
        )));
    }
    let counts = summarize(dialog);
    lines.push(Line::from(Span::styled(
        format!("  {}", counts),
        styles::dim_style(),
    )));
    lines.push(Line::from(vec![
        Span::styled("  Space", styles::statusbar_key_style()),
        Span::styled(" toggle  ", styles::statusbar_desc_style()),
        Span::styled("a", styles::statusbar_key_style()),
        Span::styled(" all  ", styles::statusbar_desc_style()),
        Span::styled("n", styles::statusbar_key_style()),
        Span::styled(" none  ", styles::statusbar_desc_style()),
        Span::styled("j/k", styles::statusbar_key_style()),
        Span::styled(" move", styles::statusbar_desc_style()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Enter", styles::statusbar_key_style()),
        Span::styled(" prune  ", styles::statusbar_desc_style()),
        Span::styled("Esc / q", styles::statusbar_key_style()),
        Span::styled(" cancel", styles::statusbar_desc_style()),
    ]));
    frame.render_widget(Paragraph::new(lines), area);
}

fn summarize(dialog: &PruneDialogState) -> String {
    let total = dialog.candidates.len();
    let remove = dialog.remove_count();
    let keep = total - remove;
    let dirty_remove = dialog.dirty_remove_count();
    if dirty_remove > 0 {
        format!("{total} worktrees | remove: {remove} ({dirty_remove} dirty!) | keep: {keep}")
    } else {
        format!("{total} worktrees | remove: {remove} | keep: {keep}")
    }
}
