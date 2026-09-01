use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::CardSize;
use crate::types::{
    AgentStatus, GithubStack, Issue, IssueKind, PrImportSource, PrState, PrStatus, WorktreeStatus,
};
use crate::ui::styles;

pub const CARD_HEIGHT: u16 = 7;
pub const CARD_HEIGHT_MEDIUM: u16 = 5;

pub struct CardContext<'a> {
    pub issue: &'a Issue,
    pub selected: bool,
    pub marked: bool,
    pub session_alive: bool,
    pub agent_status: AgentStatus,
    pub activity: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub git_status: Option<&'a WorktreeStatus>,
    pub pr: Option<&'a PrStatus>,
    pub stack: Option<&'a GithubStack>,
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(highlight_spans(
            &id_text,
            ctx.search_query,
            title_style,
        )));

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
    let pr_line = format_pr_line(ctx.pr, ctx.issue, ctx.stack);
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
    let title_line = Line::from(highlight_spans(&title_text, ctx.search_query, title_style));
    let status_line = format_status_line(ctx);
    let pr_line = format_pr_compact(ctx.pr, ctx.issue, ctx.stack);

    let mut lines = vec![title_line, status_line];
    if inner.height > 2 {
        lines.push(pr_line);
    }

    frame.render_widget(Paragraph::new(lines), inner);
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
        let mut spans = vec![Span::raw("  "), Span::styled("Todo", styles::dim_style())];
        if let Some(badge) = link_badge(ctx.issue) {
            spans.push(Span::raw(" "));
            spans.push(badge);
        }
        return Line::from(spans);
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

    if ctx.issue.kind == IssueKind::Orchestrator {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "\u{25c6} orch",
            styles::orchestrator_badge_style(),
        ));
    }

    if let Some(badge) = link_badge(ctx.issue) {
        spans.push(Span::raw(" "));
        spans.push(badge);
    }

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
    let has_linear = issue.has_linear();
    let has_missing_branch = branch.is_none();
    let has_ports = ports.is_some_and(|p| !p.is_empty());
    let pruned_indicator = pruned_indicator_text(issue);

    if !has_linear && !has_missing_branch && !has_ports && pruned_indicator.is_none() {
        return Line::from("");
    }

    let mut left_spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    let mut left_width: usize = 2;

    if has_linear {
        let identifiers: Vec<&str> = issue.linear_identifiers();
        let prefix = "\u{25c8} ";
        left_spans.push(Span::styled(
            prefix.to_string(),
            Style::default().fg(Color::Blue),
        ));
        left_width += prefix.len();

        let budget = max_width.saturating_sub(left_width + 6);
        let mut used = 0;
        for (i, ident) in identifiers.iter().enumerate() {
            let sep = if i > 0 { ", " } else { "" };
            let text = format!("{}{}", sep, ident);
            if used + text.len() > budget && i > 0 {
                let remaining = identifiers.len() - i;
                let overflow = format!("+{}", remaining);
                left_spans.push(Span::styled(
                    overflow.clone(),
                    Style::default().fg(Color::Blue),
                ));
                left_width += overflow.len();
                break;
            }
            if i > 0 {
                left_spans.push(Span::styled(", ", Style::default().fg(Color::Blue)));
                left_width += 2;
            }
            left_spans.push(Span::styled(
                ident.to_string(),
                Style::default().fg(Color::Blue),
            ));
            left_width += ident.len();
            used += text.len();
        }
    }

    let mut right_spans: Vec<Span<'static>> = Vec::new();
    let mut right_width: usize = 0;

    if has_ports {
        right_spans.push(Span::styled("\u{1f50c}", Style::default()));
        right_width += 2;
    }

    if has_missing_branch {
        if !right_spans.is_empty() {
            right_spans.insert(0, Span::raw(" "));
            right_width += 1;
        }
        right_spans.insert(
            0,
            Span::styled("\u{00f8}", Style::default().fg(styles::DIM)),
        );
        right_width += 1;
    }

    if let Some(text) = pruned_indicator.as_deref() {
        if !right_spans.is_empty() {
            right_spans.insert(0, Span::raw(" "));
            right_width += 1;
        }
        right_spans.insert(0, Span::styled(text.to_string(), styles::dim_style()));
        right_width += text.len();
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

fn format_pr_line(
    pr: Option<&PrStatus>,
    issue: &Issue,
    stack: Option<&GithubStack>,
) -> Line<'static> {
    let Some(pr) = pr else {
        if issue.github_pr_links.len() > 1 {
            let mut spans = vec![Span::raw("  ")];
            for (i, link) in issue.github_pr_links.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(", ", styles::dim_style()));
                }
                spans.push(Span::styled(
                    format!("#{}", link.number),
                    styles::dim_style(),
                ));
            }
            append_stack_badge(&mut spans, stack, issue.github_pr_links[0].number);
            return Line::from(spans);
        }
        if let Some(num) = issue.primary_pr_number() {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("#{}", num), styles::dim_style()),
            ];
            append_stack_badge(&mut spans, stack, num);
            return Line::from(spans);
        }
        return Line::from("");
    };

    let pr_number = Span::styled(format!("#{}", pr.number), styles::dim_style());

    let extra_pr_spans: Vec<Span<'static>> = issue
        .github_pr_links
        .iter()
        .filter(|l| l.number != pr.number)
        .flat_map(|l| {
            vec![
                Span::styled(", ", styles::dim_style()),
                Span::styled(format!("#{}", l.number), styles::dim_style()),
            ]
        })
        .collect();

    match &pr.state {
        PrState::Merged | PrState::Closed => {
            let (label, color) = styles::pr_state_style(&pr.state);
            let mut spans = vec![Span::raw("  "), pr_number];
            spans.extend(extra_pr_spans);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, Style::default().fg(color)));
            Line::from(spans)
        }
        PrState::Open => {
            let (checks_sym, checks_color) = styles::checks_icon(pr.checks);
            let (review_sym, review_color) = styles::review_icon(pr.review);

            let mut spans = vec![Span::raw("  "), pr_number];
            spans.extend(extra_pr_spans);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(checks_sym, Style::default().fg(checks_color)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(review_sym, Style::default().fg(review_color)));

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

            append_stack_badge(&mut spans, stack, pr.number);

            Line::from(spans)
        }
    }
}

fn format_pr_compact(
    pr: Option<&PrStatus>,
    issue: &Issue,
    stack: Option<&GithubStack>,
) -> Line<'static> {
    let Some(pr) = pr else {
        if let Some(num) = issue.primary_pr_number() {
            let mut spans = vec![Span::styled(format!("  #{}", num), styles::dim_style())];
            for link in issue.github_pr_links.iter().skip(1) {
                spans.push(Span::styled(
                    format!(", #{}", link.number),
                    styles::dim_style(),
                ));
            }
            append_stack_badge(&mut spans, stack, num);
            return Line::from(spans);
        }
        return Line::from("");
    };

    let pr_number = Span::styled(format!("  #{}", pr.number), styles::dim_style());

    let extra_pr_spans: Vec<Span<'static>> = issue
        .github_pr_links
        .iter()
        .filter(|l| l.number != pr.number)
        .flat_map(|l| {
            vec![
                Span::styled(", ", styles::dim_style()),
                Span::styled(format!("#{}", l.number), styles::dim_style()),
            ]
        })
        .collect();

    match &pr.state {
        PrState::Merged | PrState::Closed => {
            let (label, color) = styles::pr_state_style(&pr.state);
            let mut spans = vec![pr_number];
            spans.extend(extra_pr_spans);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, Style::default().fg(color)));
            Line::from(spans)
        }
        PrState::Open => {
            let (checks_sym, checks_color) = styles::checks_icon(pr.checks);
            let (review_sym, review_color) = styles::review_icon(pr.review);
            let mut spans = vec![pr_number];
            spans.extend(extra_pr_spans);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(checks_sym, Style::default().fg(checks_color)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(review_sym, Style::default().fg(review_color)));
            append_stack_badge(&mut spans, stack, pr.number);
            Line::from(spans)
        }
    }
}

fn append_stack_badge(spans: &mut Vec<Span<'static>>, stack: Option<&GithubStack>, pr_number: u32) {
    let Some(stack) = stack else {
        return;
    };
    let Some(position) = stack
        .pull_requests
        .iter()
        .position(|pr| pr.number == pr_number)
    else {
        return;
    };
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!(
            "S{} {}/{}",
            stack.number,
            position + 1,
            stack.pull_requests.len()
        ),
        Style::default().fg(Color::Cyan),
    ));
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
