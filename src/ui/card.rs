use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::types::{AgentStatus, Issue, PrStatus, WorktreeStatus};
use crate::ui::styles;

pub const CARD_HEIGHT: u16 = 5;

pub struct CardContext<'a> {
    pub issue: &'a Issue,
    pub selected: bool,
    pub session_alive: bool,
    pub agent_status: AgentStatus,
    pub activity: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub git_status: Option<&'a WorktreeStatus>,
    pub pr: Option<&'a PrStatus>,
}

pub fn render_card(frame: &mut Frame, ctx: &CardContext, area: Rect) {
    if area.height < CARD_HEIGHT || area.width < 10 {
        return;
    }

    let border_style = styles::card_border_style(ctx.selected);
    let title_style = styles::card_title_style(ctx.selected);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(format!(" {} ", ctx.issue.id), title_style));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let max_title_len = inner.width as usize;
    let title_text = truncate(&ctx.issue.title, max_title_len);
    let title_line = Line::from(Span::styled(title_text, title_style));

    let status_color = styles::agent_status_color(&ctx.agent_status);
    let session_indicator = if ctx.session_alive { "▶" } else { " " };
    let session_style = if ctx.session_alive {
        styles::session_alive_style()
    } else {
        styles::session_dead_style()
    };

    let status_label = match ctx.activity {
        Some(activity) if !activity.is_empty() => activity.to_string(),
        _ => ctx.agent_status.to_string(),
    };

    let mut status_spans = vec![
        Span::styled(session_indicator, session_style),
        Span::raw(" "),
        Span::styled(ctx.agent_status.symbol(), Style::default().fg(status_color)),
        Span::styled(format!(" {}", status_label), styles::dim_style()),
    ];

    let git_spans = format_git_status(ctx.git_status);
    let git_width: usize = git_spans.iter().map(|s| s.width()).sum();
    let base_len =
        session_indicator.len() + 1 + ctx.agent_status.symbol().len() + 1 + status_label.len();

    if let Some(branch_name) = ctx.branch {
        let reserved = base_len + 2 + git_width + if git_width > 0 { 1 } else { 0 };
        let available = (inner.width as usize).saturating_sub(reserved);
        if available > 3 {
            status_spans.push(Span::raw("  "));
            status_spans.push(Span::styled(
                truncate(branch_name, available),
                styles::dim_style(),
            ));
        }
    }

    if !git_spans.is_empty() {
        status_spans.push(Span::raw(" "));
        status_spans.extend(git_spans);
    }

    let status_line = Line::from(status_spans);

    let mut lines = vec![title_line];
    if inner.height > 1 {
        lines.push(status_line);
    }
    if inner.height > 2 {
        lines.push(format_pr_line(ctx.pr, inner.width as usize));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn format_git_status(status: Option<&WorktreeStatus>) -> Vec<Span<'static>> {
    let Some(status) = status else {
        return Vec::new();
    };

    if status.is_clean() {
        return Vec::new();
    }

    let mut spans = Vec::new();

    if status.staged > 0 {
        spans.push(Span::styled(
            format!("+{}", status.staged),
            Style::default().fg(Color::Green),
        ));
    }

    if status.staged > 0 && status.unstaged > 0 {
        spans.push(Span::styled("/", styles::dim_style()));
    }

    if status.unstaged > 0 {
        spans.push(Span::styled(
            format!("-{}", status.unstaged),
            Style::default().fg(Color::Yellow),
        ));
    }

    spans
}

fn format_pr_line(pr: Option<&PrStatus>, _max_width: usize) -> Line<'static> {
    let Some(pr) = pr else {
        return Line::from("");
    };

    let (checks_sym, checks_color) = styles::checks_icon(pr.checks);
    let (review_sym, review_color) = styles::review_icon(pr.review);

    let mut spans = vec![
        Span::styled(format!("#{}", pr.number), styles::dim_style()),
        Span::raw(" "),
        Span::styled(checks_sym, Style::default().fg(checks_color)),
        Span::raw(" "),
        Span::styled(review_sym, Style::default().fg(review_color)),
    ];

    if pr.additions > 0 || pr.deletions > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("+{}", pr.additions),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::styled("/", styles::dim_style()));
        spans.push(Span::styled(
            format!("-{}", pr.deletions),
            Style::default().fg(Color::Red),
        ));
    }

    if pr.is_draft {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("draft", styles::dim_style()));
    }

    Line::from(spans)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max > 3 {
        let end: String = s.chars().take(max - 3).collect();
        format!("{}...", end)
    } else {
        s.chars().take(max).collect()
    }
}
