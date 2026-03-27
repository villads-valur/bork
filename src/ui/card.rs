use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::types::Issue;
use crate::ui::styles;

pub const CARD_HEIGHT: u16 = 4;

pub fn render_card(
    frame: &mut Frame,
    issue: &Issue,
    area: Rect,
    selected: bool,
    session_alive: bool,
) {
    if area.height < CARD_HEIGHT || area.width < 10 {
        return;
    }

    let border_style = styles::card_border_style(selected);
    let title_style = styles::card_title_style(selected);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(format!(" {} ", issue.id), title_style));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let max_title_len = inner.width as usize;
    let title_text = truncate(&issue.title, max_title_len);
    let title_line = Line::from(Span::styled(title_text, title_style));

    let status_color = styles::agent_status_color(&issue.agent_status);
    let session_indicator = if session_alive { "▶" } else { " " };
    let session_style = if session_alive {
        styles::session_alive_style()
    } else {
        styles::session_dead_style()
    };

    let mut status_spans = vec![
        Span::styled(session_indicator, session_style),
        Span::raw(" "),
        Span::styled(
            issue.agent_status.symbol(),
            Style::default().fg(status_color),
        ),
        Span::styled(format!(" {}", issue.agent_status), styles::dim_style()),
    ];

    if let Some(ref branch) = issue.branch {
        let remaining = inner.width.saturating_sub(
            session_indicator.len() as u16
                + 1
                + issue.agent_status.symbol().len() as u16
                + 1
                + issue.agent_status.to_string().len() as u16
                + 2,
        );
        if remaining > 3 {
            let branch_text = truncate(branch, remaining as usize);
            status_spans.push(Span::raw("  "));
            status_spans.push(Span::styled(branch_text, styles::dim_style()));
        }
    }

    let status_line = Line::from(status_spans);

    let mut lines = vec![title_line];
    if inner.height > 1 {
        lines.push(status_line);
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 3 {
        format!("{}...", &s[..max - 3])
    } else {
        s[..max].to_string()
    }
}
