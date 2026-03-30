use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, InputMode};
use crate::ui::styles;

pub fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            " BORK ",
            Style::default()
                .fg(styles::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("- {} ", app.config.project_name),
            Style::default().fg(styles::TEXT),
        ),
        Span::styled("v0.1.0", styles::dim_style()),
    ]);

    frame.render_widget(Paragraph::new(title), area);

    if app.busy_count > 0 {
        let spinner = Line::from(Span::styled(
            format!("{} ", app.spinner_frame()),
            Style::default().fg(styles::ACCENT),
        ));
        let right = Paragraph::new(spinner).alignment(Alignment::Right);
        frame.render_widget(right, area);
    }
}

pub fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    // Confirm mode
    if app.input_mode == InputMode::Confirm {
        if let Some(ref msg) = app.confirm_message {
            let line = Line::from(Span::styled(
                format!(" {msg}"),
                Style::default()
                    .fg(ratatui::style::Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
    }

    // Search mode: show /query with cursor
    if app.input_mode == InputMode::Search {
        let line = Line::from(vec![
            Span::styled(
                format!(" /{}", app.search_query),
                Style::default()
                    .fg(styles::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("\u{2588}", Style::default().fg(styles::ACCENT)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // Dialog/picker mode: footer is handled by the overlay itself
    if app.input_mode == InputMode::Dialog || app.input_mode == InputMode::LinearPicker {
        return;
    }

    // Temporary message
    if let Some(ref msg) = app.message {
        let line = Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(ratatui::style::Color::Yellow),
        ));
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // Active search filter indicator
    if app.has_active_search() {
        let line = Line::from(vec![
            Span::styled(
                format!(" /{}", app.search_query),
                Style::default().fg(styles::ACCENT),
            ),
            Span::raw("  "),
            Span::styled("Esc", styles::statusbar_key_style()),
            Span::styled(":clear", styles::statusbar_desc_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // Normal mode: show keybinding hints
    let has_pr = app.selected_issue().and_then(|i| app.pr_for(i)).is_some();

    let mut bindings = vec![
        ("h/l", "focus"),
        ("j/k", "up/down"),
        ("Tab", "column"),
        ("Enter", "open"),
        ("a", "add"),
        ("n", "new"),
        ("e", "edit"),
        ("d", "delete"),
        ("x", "kill"),
        ("H/L", "move"),
        ("D", "done"),
        ("T", "todo"),
        ("P", "sync prs"),
        ("/", "search"),
    ];
    if app.linear_available {
        bindings.push(("I", "linear"));
    }
    bindings.push(("q", "quit"));

    if has_pr {
        bindings.push(("o", "open pr"));
    }

    let has_worktree = app
        .selected_issue()
        .and_then(|i| i.worktree.as_ref())
        .is_some();
    if has_worktree {
        bindings.push(("W", "reset wt"));
    } else {
        bindings.push(("W", "worktree"));
    }

    bindings.push(("q", "quit"));

    let mut spans = vec![Span::raw(" ")];
    for (i, (key, desc)) in bindings.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(*key, styles::statusbar_key_style()));
        spans.push(Span::styled(
            format!(":{desc}"),
            styles::statusbar_desc_style(),
        ));
    }

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}
