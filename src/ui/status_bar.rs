use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, InputMode, MessageKind};
use crate::ui::styles;

pub fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let has_swimlanes = app.visible_swimlane_count() > 1;
    let mut title_spans = vec![Span::styled(
        " BORK ",
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD),
    )];
    if !has_swimlanes {
        title_spans.push(Span::styled(
            format!("- {} ", app.active_project().config.project_name),
            Style::default().fg(styles::TEXT),
        ));
    }
    if app.active_project().config.debug {
        title_spans.push(Span::styled(
            " [DEBUG]",
            Style::default()
                .fg(ratatui::style::Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let title = Line::from(title_spans);
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

    // Sidebar mode
    if app.input_mode == InputMode::Sidebar {
        let line = Line::from(vec![
            Span::styled(" j", styles::statusbar_key_style()),
            Span::styled("/", styles::statusbar_desc_style()),
            Span::styled("k", styles::statusbar_key_style()),
            Span::styled(":nav  ", styles::statusbar_desc_style()),
            Span::styled("Enter", styles::statusbar_key_style()),
            Span::styled(":switch  ", styles::statusbar_desc_style()),
            Span::styled("Space", styles::statusbar_key_style()),
            Span::styled(":swimlane  ", styles::statusbar_desc_style()),
            Span::styled("Esc", styles::statusbar_key_style()),
            Span::styled(":close", styles::statusbar_desc_style()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // Overlay modes: footer is handled by the overlay itself
    if matches!(
        app.input_mode,
        InputMode::Dialog | InputMode::LinearPicker | InputMode::Help | InputMode::DebugInspector
    ) {
        return;
    }

    // Temporary message
    if let Some((ref msg, kind)) = app.message {
        let color = match kind {
            MessageKind::Info => styles::DIM,
            MessageKind::Warning => ratatui::style::Color::Yellow,
            MessageKind::Error => ratatui::style::Color::Red,
        };
        let left = Line::from(Span::styled(format!(" {msg}"), Style::default().fg(color)));
        frame.render_widget(Paragraph::new(left), area);
        render_update_banner(frame, app, area);
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
        render_update_banner(frame, app, area);
        return;
    }

    let swimlane_count = app.visible_swimlane_count();

    let mut bindings: Vec<(&str, &str)> = vec![
        ("h/l", "focus"),
        ("j/k", "nav"),
        ("Enter", "open"),
        ("t", "term"),
        ("a", "add"),
        ("n", "new"),
        ("?", "help"),
        ("q", "quit"),
    ];

    if swimlane_count > 1 {
        bindings.insert(0, ("Tab", "lane"));
    }

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

    // Right side: swimlane indicator and/or update banner
    let mut right_spans: Vec<Span> = Vec::new();

    if swimlane_count > 1 {
        let active_name = &app.active_project().config.project_name;
        right_spans.push(Span::styled(
            format!(
                "{} ({}/{}) ",
                active_name,
                app.focused_swimlane + 1,
                swimlane_count
            ),
            Style::default().fg(styles::ACCENT),
        ));
    }

    if app.update_available {
        if !right_spans.is_empty() {
            right_spans.push(Span::raw(" "));
        }
        right_spans.push(update_span());
    }

    if !right_spans.is_empty() {
        let left_width: usize = spans.iter().map(|s| s.width()).sum();
        let right_width: usize = right_spans.iter().map(|s| s.width()).sum();
        let gap = (area.width as usize).saturating_sub(left_width + right_width);
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.extend(right_spans);
        }
    }

    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

fn update_span() -> Span<'static> {
    Span::styled(
        "new update available (bork update) ",
        Style::default()
            .fg(ratatui::style::Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

fn render_update_banner(frame: &mut Frame, app: &App, area: Rect) {
    if !app.update_available {
        return;
    }
    let line = Line::from(update_span());
    let right = Paragraph::new(line).alignment(Alignment::Right);
    frame.render_widget(right, area);
}
