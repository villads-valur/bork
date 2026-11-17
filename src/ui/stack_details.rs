use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Project, StackDetailsState};
use crate::types::{GithubStackPullRequest, PrState, StackPosition};
use crate::ui::{card, styles};

pub fn render(frame: &mut Frame, app: &App) {
    let Some(details) = &app.stack_details else {
        return;
    };
    let Some(project) = app.find_project(&details.project_id) else {
        return;
    };
    let Some(issue) = project
        .issues
        .iter()
        .find(|issue| issue.id == details.issue_id)
    else {
        return;
    };
    let is_stack = issue.github_stack.is_some();
    if is_stack && (project.live.gh_missing || project.live.stacks_unsupported) {
        return;
    }
    let size = frame.area();
    let width = size.width.saturating_sub(4).min(100);
    let height = size.height.saturating_sub(4).min(24);
    if width < 10 || height < 5 {
        return;
    }
    let area = Rect::new(
        size.x + (size.width - width) / 2,
        size.y + (size.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let title = match issue.github_stack {
        Some(number) => format!(" Stack #{number} · {} ", issue.id),
        None => format!(" PRs · {} ", issue.id),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(styles::ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let refresh_error = project
        .live
        .github_error
        .as_ref()
        .map(|error| format!("Refresh failed: {error} · showing last known data"));
    if !is_stack {
        let members = project.linked_pr_members(issue);
        let summary = refresh_error.unwrap_or_else(|| {
            format!(
                "{} PRs · CI: {}",
                members.len(),
                project.member_checks(&members).label()
            )
        });
        render_members(
            frame, app, project, details, inner, &summary, &members, true,
        );
        return;
    }
    let Some(stack) = project.attached_stack(issue) else {
        if project.live.github_loading() {
            if project.live.github_fetching() {
                super::status_bar::render_loading_spinner(frame, app, inner);
            }
            frame.render_widget(
                Paragraph::new("Esc close"),
                Rect::new(
                    inner.x,
                    inner.bottom() - 1,
                    inner.width.saturating_sub(super::status_bar::SPINNER_WIDTH),
                    1,
                ),
            );
            return;
        }
        frame.render_widget(
            Paragraph::new(format!(
                "Stack {} · {} · P refresh · Esc close",
                project.live.missing_github_status(),
                project.live.github_error.as_deref().unwrap_or("")
            )),
            inner,
        );
        return;
    };
    let summary = if let Some(error) = refresh_error {
        error
    } else if project.live.stacks_available {
        format!(
            "{} PRs · base {} · CI: {}",
            stack.pull_requests.len(),
            stack.base_ref,
            project.stack_checks(stack).label()
        )
    } else {
        "Showing last known members · P refresh".to_string()
    };
    render_members(
        frame,
        app,
        project,
        details,
        inner,
        &summary,
        &project.detail_members(issue),
        false,
    );
}

/// `group_by_stack` puts a header above each run of PRs from one stack, for
/// linked PRs that can span several stacks.
#[allow(clippy::too_many_arguments)]
fn render_members(
    frame: &mut Frame,
    app: &App,
    project: &Project,
    details: &StackDetailsState,
    inner: Rect,
    summary: &str,
    members: &[GithubStackPullRequest],
    group_by_stack: bool,
) {
    let selected = details.selected.min(members.len().saturating_sub(1));
    let missing_statuses = members
        .iter()
        .filter(|pr| pr.state == PrState::Open && project.pr_by_number(pr.number).is_none())
        .count();
    let loading =
        project.live.github_fetching() && (missing_statuses > 0 || project.live.pr_refreshing);
    frame.render_widget(
        Paragraph::new(styles::truncate(summary, inner.width as usize)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let positions: Vec<_> = members
        .iter()
        .map(|member| project.pr_stack_position(member.number))
        .collect();
    let show_headers = group_by_stack && positions.iter().any(Option::is_some);
    let mut lines: Vec<Line> = Vec::with_capacity(members.len());
    let mut selected_line = 0;
    for (index, (member, position)) in members.iter().zip(&positions).enumerate() {
        let stack = position.map(|position| position.stack);
        let previous_stack = index
            .checked_sub(1)
            .map(|previous| positions[previous].map(|position| position.stack));
        if show_headers && previous_stack != Some(stack) {
            lines.push(group_header(stack));
        }
        if index == selected {
            selected_line = lines.len();
        }
        let checked = details.checked.contains(&member.number);
        lines.push(member_line(
            project,
            member,
            *position,
            index == selected,
            checked,
            inner.width,
        ));
    }

    let visible = inner.height.saturating_sub(3) as usize;
    let offset = (selected_line + 1).saturating_sub(visible);
    for (row, line) in lines.into_iter().skip(offset).take(visible).enumerate() {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + row as u16 + 2, inner.width, 1),
        );
    }
    let footer = if let Some((message, _)) = &app.message {
        message.clone()
    } else {
        let picked = match details.checked.len() {
            0 => String::new(),
            count => format!("{count} picked  "),
        };
        format!(
            "Space pick  Enter open  o open all  R review  P refresh  Esc close  {picked}{}/{}",
            selected + 1,
            members.len()
        )
    };
    let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    let mut content_area = footer_area;
    if loading {
        content_area.width = content_area
            .width
            .saturating_sub(super::status_bar::SPINNER_WIDTH);
    }
    frame.render_widget(
        Paragraph::new(footer).style(styles::dim_style()),
        content_area,
    );
    if loading {
        super::status_bar::render_loading_spinner(frame, app, footer_area);
    }
}

fn group_header(stack: Option<u32>) -> Line<'static> {
    let text = match stack {
        Some(number) => format!("Stack #{number}"),
        None => "Not in a stack".to_string(),
    };
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::BOLD),
    ))
}

fn member_line(
    project: &Project,
    member: &GithubStackPullRequest,
    position: Option<StackPosition>,
    selected: bool,
    checked: bool,
    width: u16,
) -> Line<'static> {
    let pr = project.pr_by_number(member.number);
    let mut status_spans = if member.state != PrState::Open {
        vec![Span::styled(
            format!(" {}", member.state),
            styles::dim_style(),
        )]
    } else if pr.is_none() && project.live.github_loading() {
        Vec::new()
    } else {
        pr.map(card::pr_spans).unwrap_or_else(|| {
            vec![Span::styled(
                format!(" {}", project.live.missing_github_status()),
                styles::dim_style(),
            )]
        })
    };
    if project.review_pending_for(member.number) {
        status_spans.push(Span::styled(
            " · your review",
            Style::default().fg(styles::ACCENT),
        ));
    }
    let pointer = if selected { "▸" } else { " " };
    let checkbox = if checked { "[x]" } else { "[ ]" };
    let position = position
        .map(|position| format!("{}/{} ", position.position, position.size))
        .unwrap_or_default();
    let prefix = format!("{pointer}{checkbox} {position}#{} ", member.number);
    let title = pr
        .map(|pr| pr.title.as_str())
        .unwrap_or(&member.head_branch);
    let budget = (width as usize).saturating_sub(
        prefix.chars().count() + status_spans.iter().map(Span::width).sum::<usize>() + 2,
    );
    let mut spans = vec![
        Span::styled(prefix, styles::dim_style()),
        Span::raw(styles::truncate(title, budget)),
    ];
    spans.extend(status_spans);
    Line::from(spans)
}
