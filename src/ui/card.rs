use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::CardSize;
use crate::types::{AgentStatus, Issue, IssueKind, PrState, PrStatus, WorktreeStatus};
use crate::ui::styles;

pub const CARD_HEIGHT: u16 = 7;
pub const CARD_HEIGHT_MEDIUM: u16 = 5;
pub const CARD_HEIGHT_COMPACT: u16 = 4;

pub struct CardContext<'a> {
    pub issue: &'a Issue,
    pub selected: bool,
    pub session_alive: bool,
    pub agent_status: AgentStatus,
    pub activity: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub git_status: Option<&'a WorktreeStatus>,
    pub pr: Option<&'a PrStatus>,
    pub ports: Option<&'a Vec<u16>>,
}

pub fn render_card(frame: &mut Frame, ctx: &CardContext, area: Rect, card_size: CardSize) {
    if area.width < 10 || area.height < 3 {
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

    let max_width = inner.width as usize;

    match card_size {
        CardSize::Full => render_full(frame, ctx, inner, max_width, title_style),
        CardSize::Medium => render_medium(frame, ctx, inner, max_width, title_style),
        CardSize::Compact => render_compact(frame, ctx, inner, max_width, title_style),
    }
}

fn render_full(
    frame: &mut Frame,
    ctx: &CardContext,
    inner: Rect,
    max_width: usize,
    title_style: Style,
) {
    let title_text = styles::truncate(&ctx.issue.title, max_width);
    let title_line = Line::from(Span::styled(title_text, title_style));
    let status_line = format_status_line(ctx);
    let pr_line = format_pr_line(ctx.pr);
    let bottom_line = format_bottom_line(ctx.issue, ctx.branch, ctx.ports, max_width);

    let mut lines = vec![title_line];
    if inner.height > 1 {
        lines.push(status_line);
    }
    if inner.height > 2 {
        lines.push(pr_line);
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);

    if inner.height > 3 {
        let bottom_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(Paragraph::new(bottom_line), bottom_area);
    }
}

fn render_medium(
    frame: &mut Frame,
    ctx: &CardContext,
    inner: Rect,
    max_width: usize,
    title_style: Style,
) {
    let title_text = styles::truncate(&ctx.issue.title, max_width);
    let title_line = Line::from(Span::styled(title_text, title_style));
    let status_line = format_status_line(ctx);
    let pr_line = format_pr_compact(ctx.pr);

    let mut lines = vec![title_line, status_line];
    if inner.height > 2 {
        lines.push(pr_line);
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_compact(
    frame: &mut Frame,
    ctx: &CardContext,
    inner: Rect,
    max_width: usize,
    title_style: Style,
) {
    let title_text = styles::truncate(&ctx.issue.title, max_width);
    let title_line = Line::from(Span::styled(title_text, title_style));

    let mut status_spans = Vec::new();
    if ctx.issue.kind != IssueKind::NonAgentic {
        let status_color = styles::agent_status_color(&ctx.agent_status);
        if ctx.session_alive {
            status_spans.push(Span::styled("▶", styles::session_alive_style()));
            status_spans.push(Span::raw(" "));
        }
        status_spans.push(Span::styled(
            ctx.agent_status.symbol(),
            Style::default().fg(status_color),
        ));

        let git_spans = format_git_status(ctx.git_status);
        if !git_spans.is_empty() {
            status_spans.push(Span::raw(" "));
            status_spans.extend(git_spans);
        }
    }

    if let Some(branch) = ctx.branch {
        if !status_spans.is_empty() {
            status_spans.push(Span::raw("  "));
        }
        status_spans.push(Span::styled("\u{1f33f}", Style::default().fg(Color::Green)));
        let max_branch = max_width.saturating_sub(12);
        status_spans.push(Span::styled(
            styles::truncate(branch, max_branch),
            styles::dim_style(),
        ));
    }

    let status_line = Line::from(status_spans);
    let mut lines = vec![title_line];
    if inner.height > 1 {
        lines.push(status_line);
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn format_status_line(ctx: &CardContext) -> Line<'static> {
    if ctx.issue.kind == IssueKind::NonAgentic {
        return Line::from(vec![
            Span::raw("  "),
            Span::styled("Todo", styles::dim_style()),
        ]);
    }

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

    let mut spans = vec![
        Span::styled(session_indicator, session_style),
        Span::raw(" "),
        Span::styled(ctx.agent_status.symbol(), Style::default().fg(status_color)),
        Span::styled(format!(" {}", status_label), styles::dim_style()),
    ];

    let git_spans = format_git_status(ctx.git_status);
    if !git_spans.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(git_spans);
    }

    Line::from(spans)
}

fn format_bottom_line(
    issue: &Issue,
    branch: Option<&str>,
    ports: Option<&Vec<u16>>,
    max_width: usize,
) -> Line<'static> {
    let has_linear = issue.linear_identifier.is_some();
    let has_branch = branch.is_some();
    let has_ports = ports.is_some_and(|p| !p.is_empty());

    if !has_linear && !has_branch && !has_ports {
        return Line::from("");
    }

    let mut left_spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    let mut left_width: usize = 2;

    if let Some(ref identifier) = issue.linear_identifier {
        let text = format!("\u{25c8} {}", identifier);
        left_width += text.len();
        left_spans.push(Span::styled(text, Style::default().fg(Color::Blue)));
    }

    let mut right_spans: Vec<Span<'static>> = Vec::new();
    let mut right_width: usize = 0;

    if has_ports {
        right_spans.push(Span::styled("\u{1f50c}", Style::default()));
        right_width += 2;
    }

    if has_branch {
        if !right_spans.is_empty() {
            right_spans.insert(0, Span::raw(" "));
            right_width += 1;
        }
        right_spans.insert(
            0,
            Span::styled("\u{1f33f}", Style::default().fg(Color::Green)),
        );
        right_width += 2;
    }

    if !right_spans.is_empty() {
        let total = left_width + right_width + 1;
        let gap = if total < max_width {
            max_width - total
        } else {
            1
        };
        left_spans.push(Span::raw(" ".repeat(gap)));
        left_spans.extend(right_spans);
        left_spans.push(Span::raw(" "));
    }

    Line::from(left_spans)
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

fn format_pr_line(pr: Option<&PrStatus>) -> Line<'static> {
    let Some(pr) = pr else {
        return Line::from("");
    };

    let pr_number = Span::styled(format!("#{}", pr.number), styles::dim_style());

    match &pr.state {
        PrState::Merged | PrState::Closed => {
            let (label, color) = styles::pr_state_style(&pr.state);
            Line::from(vec![
                Span::raw("  "),
                pr_number,
                Span::raw(" "),
                Span::styled(label, Style::default().fg(color)),
            ])
        }
        PrState::Open => {
            let (checks_sym, checks_color) = styles::checks_icon(pr.checks);
            let (review_sym, review_color) = styles::review_icon(pr.review);

            let mut spans = vec![
                Span::raw("  "),
                pr_number,
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
    }
}

fn format_pr_compact(pr: Option<&PrStatus>) -> Line<'static> {
    let Some(pr) = pr else {
        return Line::from("");
    };

    let pr_number = Span::styled(format!("  #{}", pr.number), styles::dim_style());

    match &pr.state {
        PrState::Merged | PrState::Closed => {
            let (label, color) = styles::pr_state_style(&pr.state);
            Line::from(vec![
                pr_number,
                Span::raw(" "),
                Span::styled(label, Style::default().fg(color)),
            ])
        }
        PrState::Open => {
            let (checks_sym, checks_color) = styles::checks_icon(pr.checks);
            let (review_sym, review_color) = styles::review_icon(pr.review);
            Line::from(vec![
                pr_number,
                Span::raw(" "),
                Span::styled(checks_sym, Style::default().fg(checks_color)),
                Span::raw(" "),
                Span::styled(review_sym, Style::default().fg(review_color)),
            ])
        }
    }
}
