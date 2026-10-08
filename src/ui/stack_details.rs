use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::types::PrState;
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
    if project.live.gh_missing || project.live.stacks_unsupported {
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
    let title = format!(
        " Stack #{} · {} ",
        issue.github_stack.unwrap_or_default(),
        issue.id
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(styles::ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);

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
    let missing_statuses = stack
        .pull_requests
        .iter()
        .filter(|pr| pr.state == PrState::Open && project.pr_by_number(pr.number).is_none())
        .count();
    let loading =
        project.live.github_fetching() && (missing_statuses > 0 || project.live.pr_refreshing);
    let summary = if let Some(error) = &project.live.github_error {
        format!("Refresh failed: {error} · showing last known data")
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
    frame.render_widget(
        Paragraph::new(styles::truncate(&summary, inner.width as usize)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let visible = inner.height.saturating_sub(3) as usize;
    let offset = details
        .selected
        .min(stack.pull_requests.len().saturating_sub(1))
        .saturating_sub(visible.saturating_sub(1));
    for (row, member) in stack
        .pull_requests
        .iter()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
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
        if project.review_requested_for(member.number) {
            status_spans.push(Span::styled(
                " · your review",
                Style::default().fg(styles::ACCENT),
            ));
        }
        let pointer = if offset + row == details.selected {
            "▸"
        } else {
            " "
        };
        let prefix = format!("{pointer}{:>2} #{} ", offset + row + 1, member.number);
        let title = pr
            .map(|pr| pr.title.as_str())
            .unwrap_or(&member.head_branch);
        let budget = (inner.width as usize).saturating_sub(
            prefix.chars().count() + status_spans.iter().map(Span::width).sum::<usize>() + 2,
        );
        let mut spans = vec![
            Span::styled(prefix, styles::dim_style()),
            Span::raw(styles::truncate(title, budget)),
        ];
        spans.extend(status_spans);
        let line = Line::from(spans);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + row as u16 + 2, inner.width, 1),
        );
    }
    let footer = if let Some((message, _)) = &app.message {
        message.clone()
    } else {
        format!(
            "j/k move  o open all  R review all  P refresh  Esc close  {}/{}",
            (offset + visible).min(stack.pull_requests.len()),
            stack.pull_requests.len()
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
