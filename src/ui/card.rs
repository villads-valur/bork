use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{CardSize, Project};
use crate::types::{
    AgentStatus, Issue, IssueKind, PrImportSource, PrState, PrStatus, WorktreeStatus,
};
use crate::ui::styles;

pub const CARD_HEIGHT: u16 = 7;
pub const CARD_HEIGHT_MEDIUM: u16 = 6;

pub struct CardContext<'a> {
    pub issue: &'a Issue,
    pub selected: bool,
    pub marked: bool,
    pub session_alive: bool,
    pub agent_status: AgentStatus,
    pub activity: Option<&'a str>,
    pub git_status: Option<&'a WorktreeStatus>,
    pub pr: Option<&'a PrStatus>,
    pub project: &'a Project,
    pub ports: Option<&'a Vec<u16>>,
    pub search_query: &'a str,
}

pub fn render_card(frame: &mut Frame, ctx: &CardContext, area: Rect, card_size: CardSize) {
    if area.width < 10 || area.height < 3 {
        return;
    }

    let border_style = if ctx.issue.kind == IssueKind::Orchestrator {
        styles::orchestrator_card_border_style(ctx.selected, ctx.marked)
    } else {
        styles::card_border_style(ctx.selected, ctx.marked)
    };
    let title_style = styles::card_title_style(ctx.selected);

    let id_text = if ctx.marked {
        format!(" [x] {} ", ctx.issue.id)
    } else {
        format!(" {} ", ctx.issue.id)
    };
    let (type_label, type_style) = match ctx.issue.kind {
        IssueKind::Orchestrator => ("· orch ", styles::orchestrator_badge_style()),
        IssueKind::NonAgentic => ("· todo ", styles::dim_style()),
        IssueKind::Agentic => ("", title_style),
    };
    let id_text = styles::truncate(
        &id_text,
        (area.width as usize).saturating_sub(Span::raw(type_label).width() + 3),
    );
    let mut title = highlight_spans(&id_text, ctx.search_query, title_style);
    if !type_label.is_empty() {
        title.push(Span::styled(type_label, type_style));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(title));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let max_width = inner.width as usize;

    match card_size {
        CardSize::Full => render_full(frame, ctx, inner, max_width, title_style),
        CardSize::Medium => render_medium(frame, ctx, inner, max_width, title_style),
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
    let title_line = Line::from(highlight_spans(&title_text, ctx.search_query, title_style));
    let status_line = format_status_line(ctx);
    let pr_lines = format_pr_rows(ctx, max_width, false);
    let bottom_line = format_bottom_line(ctx.issue, ctx.ports, max_width);

    let mut lines = vec![title_line];
    if inner.height > 1 {
        lines.push(status_line);
    }
    if inner.height > 2 {
        lines.extend(
            pr_lines
                .into_iter()
                .take(inner.height.saturating_sub(3) as usize),
        );
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
    let title_line = Line::from(highlight_spans(&title_text, ctx.search_query, title_style));
    let status_line = format_status_line(ctx);
    let pr_lines = format_pr_rows(ctx, max_width, true);

    let mut lines = vec![title_line, status_line];
    if inner.height > 2 {
        lines.extend(pr_lines.into_iter().take(1));
    }

    frame.render_widget(Paragraph::new(lines), inner);
    if inner.height > 3 {
        let footer = format_bottom_line(ctx.issue, ctx.ports, max_width);
        let bottom = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        frame.render_widget(Paragraph::new(footer), bottom);
    }
}

/// Splits `text` into spans, highlighting the first case-insensitive match of
/// `query` with the search highlight style. Non-matching portions use `base_style`.
pub fn highlight_spans(text: &str, query: &str, base_style: Style) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }

    let text_lower = text.to_lowercase();
    let query_lower = query.to_lowercase();

    let Some(start) = text_lower.find(&query_lower) else {
        return vec![Span::styled(text.to_string(), base_style)];
    };

    let end = start + query_lower.len();
    let highlight_style = styles::search_highlight_style();

    let mut spans = Vec::with_capacity(3);
    if start > 0 {
        spans.push(Span::styled(text[..start].to_string(), base_style));
    }
    spans.push(Span::styled(text[start..end].to_string(), highlight_style));
    if end < text.len() {
        spans.push(Span::styled(text[end..].to_string(), base_style));
    }
    spans
}

fn link_badge(issue: &Issue) -> Option<Span<'static>> {
    if !issue.has_links() {
        return None;
    }
    Some(Span::styled(
        format!("\u{221e}{}", issue.linked_issues.len()),
        Style::default().fg(Color::Cyan),
    ))
}

fn format_status_line(ctx: &CardContext) -> Line<'static> {
    if ctx.issue.kind == IssueKind::NonAgentic {
        return Line::default();
    }

    let status_color = styles::agent_status_color(&ctx.agent_status);
    let session_indicator = if ctx.session_alive { "▶" } else { " " };
    let session_style = if ctx.session_alive {
        styles::session_alive_style()
    } else {
        styles::session_dead_style()
    };

    let is_review = ctx.issue.primary_pr_import_source() == Some(PrImportSource::ReviewRequested);

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

    if is_review {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("review", Style::default().fg(Color::Yellow)));
    }

    let git_spans = format_git_status(ctx.git_status);
    if !git_spans.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(git_spans);
    }

    Line::from(spans)
}

fn format_bottom_line(issue: &Issue, ports: Option<&Vec<u16>>, max_width: usize) -> Line<'static> {
    let mut right = Vec::new();
    if let Some(text) = pruned_indicator_text(issue) {
        right.push(text);
    }
    if ports.is_some_and(|ports| !ports.is_empty()) {
        right.push("🔌".to_string());
    }
    // Drop secondary indicators first when the footer gets narrow.
    while right.len() > 1 && Span::raw(right.join(" ")).width() + 10 > max_width {
        right.remove(0);
    }
    let right = Span::styled(right.join(" "), styles::dim_style());
    let left_budget = max_width.saturating_sub(right.width() + 2);
    let mut left = vec![Span::raw("  ")];
    if let Some(badge) = link_badge(issue) {
        left.push(badge);
        left.push(Span::raw(" "));
    }
    let used = left.iter().map(Span::width).sum::<usize>();
    if issue.has_linear() && left_budget > used + 2 {
        let identifiers = issue.linear_identifiers().join(", ");
        left.push(Span::styled(
            styles::truncate(&format!("◈ {identifiers}"), left_budget - used),
            Style::default().fg(Color::Blue),
        ));
    }
    let used = left.iter().map(Span::width).sum::<usize>();
    if right.width() > 0 && used + right.width() < max_width {
        left.push(Span::raw(" ".repeat(max_width - used - right.width() - 1)));
        left.push(right);
        left.push(Span::raw(" "));
    }
    Line::from(left)
}

/// "pruned 3d ago" indicator. Only shown when the issue has been pruned and
/// no new worktree has been attached since.
fn pruned_indicator_text(issue: &Issue) -> Option<String> {
    if issue.worktree.is_some() {
        return None;
    }
    let pruned_at = issue.pruned_at?;
    let now = crate::app::unix_now();
    Some(format!(
        "pruned {}",
        humanize_age(now.saturating_sub(pruned_at))
    ))
}

pub(crate) fn humanize_age(secs: u64) -> String {
    if secs < 60 {
        return "just now".to_string();
    }
    if secs < 3600 {
        return format!("{}m ago", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h ago", secs / 3600);
    }
    if secs < 30 * 86_400 {
        return format!("{}d ago", secs / 86_400);
    }
    format!("{}mo ago", secs / (30 * 86_400))
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

fn format_pr_rows(ctx: &CardContext, width: usize, compact: bool) -> Vec<Line<'static>> {
    if ctx.project.live.gh_missing {
        return Vec::new();
    }
    if let Some(number) = ctx
        .issue
        .github_stack
        .filter(|_| !ctx.project.live.stacks_unsupported)
    {
        let Some(stack) = ctx.project.attached_stack(ctx.issue) else {
            if ctx.project.live.github_loading() {
                return vec![Line::styled(
                    format!("  Stack #{number}"),
                    styles::dim_style(),
                )];
            }
            return vec![Line::styled(
                styles::truncate(
                    &format!(
                        "  Stack #{number} · {}",
                        ctx.project.live.missing_github_status()
                    ),
                    width,
                ),
                styles::dim_style(),
            )];
        };
        let count = stack.pull_requests.len();
        if !compact && count <= 2 && count > 0 && ctx.project.live.stacks_available {
            return stack
                .pull_requests
                .iter()
                .enumerate()
                .map(|(index, member)| {
                    let connector = if count == 1 {
                        "─"
                    } else if index == 0 {
                        "┌"
                    } else {
                        "└"
                    };
                    let mut spans = vec![
                        Span::styled(format!("  {connector} "), Style::default().fg(Color::Cyan)),
                        Span::styled(format!("#{}", member.number), styles::dim_style()),
                    ];
                    if member.state != PrState::Open {
                        let (label, color) = styles::pr_state_style(&member.state);
                        spans.push(Span::styled(
                            format!(" {label}"),
                            Style::default().fg(color),
                        ));
                    } else if let Some(pr) = ctx.project.pr_by_number(member.number) {
                        spans.extend(pr_spans(pr));
                    } else if !ctx.project.live.github_loading() {
                        spans.push(Span::styled(
                            format!(" {}", ctx.project.live.missing_github_status()),
                            styles::dim_style(),
                        ));
                    }
                    Line::from(spans)
                })
                .collect();
        }
        let mut header = vec![Span::styled(
            format!("  {count} PRs"),
            Style::default().fg(Color::Cyan),
        )];
        let mut status = Vec::new();
        if !ctx.project.live.stacks_available {
            if ctx.project.live.github_loading() {
                return vec![Line::from(header)];
            }
            status.push(Span::styled(
                ctx.project.live.missing_github_status(),
                styles::dim_style(),
            ));
        } else {
            let checks = ctx.project.stack_checks(stack);
            let states = [
                (checks.failed, "✗", "failed", Color::Red),
                (checks.pending, "◌", "pending", Color::Yellow),
                (checks.unknown, "?", "unknown", styles::DIM),
                (checks.passed, "✓", "passed", Color::Green),
            ];
            let single_state = states.iter().filter(|(count, ..)| *count > 0).count() == 1;
            for (count, symbol, label, color) in states {
                if count > 0 {
                    let text = if single_state {
                        format!("{symbol} {count} {label}")
                    } else {
                        format!("{symbol} {count}")
                    };
                    status.push(Span::styled(text, Style::default().fg(color)));
                }
            }
            if status.is_empty() {
                let merged = stack
                    .pull_requests
                    .iter()
                    .filter(|pr| pr.state == PrState::Merged)
                    .count();
                status.push(Span::styled(
                    format!("{merged} merged · {} closed", count - merged),
                    styles::dim_style(),
                ));
            }
        }
        for (index, span) in status.iter().enumerate() {
            let used = header.iter().map(Span::width).sum::<usize>();
            let remaining = if index + 1 < status.len() { 2 } else { 0 };
            if used + 3 + span.width() + remaining > width {
                if used + 2 <= width {
                    header.push(Span::styled(" …", styles::dim_style()));
                }
                break;
            }
            header.push(Span::styled(" · ", styles::dim_style()));
            header.push(span.clone());
        }
        return vec![Line::from(header)];
    }

    let mut numbers = ctx.issue.pr_numbers();
    if numbers.is_empty() {
        if let Some(pr) = ctx.pr {
            numbers.push(pr.number);
        }
    }
    let limit = if compact { 1 } else { 2 };
    numbers
        .iter()
        .take(limit)
        .enumerate()
        .map(|(index, number)| {
            let pr = ctx
                .project
                .pr_by_number(*number)
                .or_else(|| ctx.pr.filter(|pr| pr.number == *number));
            let suffix = if index + 1 == limit && numbers.len() > limit {
                format!("  +{} PRs", numbers.len() - limit)
            } else {
                String::new()
            };
            let mut spans = vec![Span::styled(format!("  #{number}"), styles::dim_style())];
            let mut status = pr.map(pr_spans).unwrap_or_else(|| {
                if ctx.project.live.github_loading() {
                    return Vec::new();
                }
                vec![Span::styled(
                    format!(" {}", ctx.project.live.missing_github_status()),
                    styles::dim_style(),
                )]
            });
            if let Some(pr) = pr.filter(|pr| !pr.is_draft && pr.state != PrState::Merged) {
                status.push(Span::styled(
                    format!(" +{}", pr.additions),
                    Style::default().fg(Color::Green),
                ));
                status.push(Span::styled(
                    format!("/-{}", pr.deletions),
                    Style::default().fg(Color::Red),
                ));
            }
            while !status.is_empty()
                && spans.iter().map(Span::width).sum::<usize>()
                    + status.iter().map(Span::width).sum::<usize>()
                    + suffix.len()
                    > width
            {
                status.pop();
            }
            spans.extend(status);
            spans.push(Span::styled(suffix, Style::default().fg(Color::Cyan)));
            Line::from(spans)
        })
        .collect()
}

pub(crate) fn pr_spans(pr: &PrStatus) -> Vec<Span<'static>> {
    if pr.state != PrState::Open {
        let (label, color) = styles::pr_state_style(&pr.state);
        return vec![Span::styled(
            format!(" {label}"),
            Style::default().fg(color),
        )];
    }
    let mut spans = Vec::new();
    let (checks, checks_color) = styles::checks_icon(pr.checks);
    let (review, review_color) = styles::review_icon(pr.review);
    spans.push(Span::styled(
        format!(" {checks}"),
        Style::default().fg(checks_color),
    ));
    spans.push(Span::styled(
        format!(" {review}"),
        Style::default().fg(review_color),
    ));
    if pr.is_draft {
        spans.push(Span::styled(" draft", styles::dim_style()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn humanize_age_seconds() {
        assert_eq!(humanize_age(0), "just now");
        assert_eq!(humanize_age(30), "just now");
    }

    #[test]
    fn humanize_age_minutes() {
        assert_eq!(humanize_age(60), "1m ago");
        assert_eq!(humanize_age(3599), "59m ago");
    }

    #[test]
    fn humanize_age_hours() {
        assert_eq!(humanize_age(3600), "1h ago");
        assert_eq!(humanize_age(86_399), "23h ago");
    }

    #[test]
    fn humanize_age_days() {
        assert_eq!(humanize_age(86_400), "1d ago");
        assert_eq!(humanize_age(7 * 86_400), "7d ago");
    }

    #[test]
    fn humanize_age_months() {
        assert_eq!(humanize_age(30 * 86_400), "1mo ago");
        assert_eq!(humanize_age(90 * 86_400), "3mo ago");
    }

    fn issue_for_prune_indicator(worktree: Option<&str>, pruned_at: Option<u64>) -> Issue {
        Issue {
            worktree: worktree.map(String::from),
            pruned_at,
            ..Issue::new(
                "bork-1",
                "t",
                crate::types::Column::Done,
                crate::types::AgentKind::OpenCode,
            )
        }
    }

    #[test]
    fn pruned_indicator_none_when_worktree_still_attached() {
        let issue = issue_for_prune_indicator(Some("wt"), Some(1_700_000_000));
        assert!(pruned_indicator_text(&issue).is_none());
    }

    #[test]
    fn pruned_indicator_none_when_never_pruned() {
        let issue = issue_for_prune_indicator(None, None);
        assert!(pruned_indicator_text(&issue).is_none());
    }

    #[test]
    fn pruned_indicator_set_when_pruned_and_detached() {
        let issue = issue_for_prune_indicator(None, Some(0));
        let text = pruned_indicator_text(&issue).expect("expected pruned indicator");
        assert!(text.starts_with("pruned "));
    }

    #[test]
    fn highlight_spans_no_query_returns_single_span() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("Fix login bug", "", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Fix login bug");
    }

    #[test]
    fn highlight_spans_no_match_returns_single_span() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("Fix login bug", "zzz", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Fix login bug");
    }

    #[test]
    fn highlight_spans_match_at_start() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("Fix login bug", "fix", base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Fix");
        assert_eq!(spans[0].style, styles::search_highlight_style());
        assert_eq!(spans[1].content, " login bug");
        assert_eq!(spans[1].style, base);
    }

    #[test]
    fn highlight_spans_match_at_end() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("Fix login bug", "bug", base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Fix login ");
        assert_eq!(spans[0].style, base);
        assert_eq!(spans[1].content, "bug");
        assert_eq!(spans[1].style, styles::search_highlight_style());
    }

    #[test]
    fn highlight_spans_match_in_middle() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("Fix login bug", "log", base);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "Fix ");
        assert_eq!(spans[1].content, "log");
        assert_eq!(spans[1].style, styles::search_highlight_style());
        assert_eq!(spans[2].content, "in bug");
    }

    #[test]
    fn highlight_spans_case_insensitive() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("FIX Login", "fix", base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "FIX");
        assert_eq!(spans[0].style, styles::search_highlight_style());
    }

    #[test]
    fn highlight_spans_full_match() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("Fix", "fix", base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Fix");
        assert_eq!(spans[0].style, styles::search_highlight_style());
    }

    #[test]
    fn highlight_spans_first_occurrence_only() {
        let base = Style::default().fg(Color::White);
        let spans = highlight_spans("Fix the fix", "fix", base);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Fix");
        assert_eq!(spans[0].style, styles::search_highlight_style());
        assert_eq!(spans[1].content, " the fix");
        assert_eq!(spans[1].style, base);
    }
}
