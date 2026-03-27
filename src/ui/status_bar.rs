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

    // Dialog mode: footer is handled by the dialog overlay itself
    if app.input_mode == InputMode::Dialog {
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

    // Normal mode: show keybinding hints
    let has_pr = app.selected_issue().and_then(|i| app.pr_for(i)).is_some();

    let mut bindings = vec![
        ("h/l", "focus"),
        ("j/k", "up/down"),
        ("Tab", "column"),
        ("Enter", "open"),
        ("n", "new"),
        ("e", "edit"),
        ("d", "delete"),
        ("x", "kill"),
        ("H/L", "move"),
        ("P", "sync prs"),
    ];

    if has_pr {
        bindings.push(("o", "open pr"));
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
