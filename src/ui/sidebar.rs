use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;

use crate::app::App;
use crate::ui::styles;

pub const SIDEBAR_WIDTH: u16 = 22;
const SIDEBAR_NAME_PADDING: u16 = 5;

pub fn render_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let sidebar = match &app.sidebar {
        Some(s) => s,
        None => return,
    };

    let border_color = if sidebar.focused {
        styles::ACCENT
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .title(" Projects ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let items: Vec<ListItem> = app
        .projects
        .iter()
        .enumerate()
        .map(|(i, project)| {
            let is_focused = i == app.focused_project;
            let is_swimlane = sidebar.swimlane_indices.contains(&i);
            let has_activity = sidebar.activity.get(&i).copied().unwrap_or(false);

            let marker = if is_focused {
                "◆"
            } else if is_swimlane {
                "▪"
            } else if has_activity {
                "●"
            } else {
                " "
            };

            let marker_style = if is_focused || is_swimlane {
                Style::default().fg(styles::ACCENT)
            } else if has_activity {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let name = &project.config.project_name;
            let max_name = area.width.saturating_sub(SIDEBAR_NAME_PADDING) as usize;
            let display_name = styles::truncate(name, max_name);

            let name_style = if is_focused {
                Style::default()
                    .fg(styles::ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else if is_swimlane {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let mut style = Style::default();
            if sidebar.focused && i == sidebar.selected {
                style = style.add_modifier(Modifier::REVERSED);
            }

            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", marker), marker_style),
                Span::styled(display_name, name_style),
            ]))
            .style(style)
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}
