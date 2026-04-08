pub mod board;
pub mod card;
pub mod debug_inspector;
pub mod dialog;
pub mod help;
pub mod linear_picker;
pub mod sidebar;
pub mod status_bar;
pub mod styles;

use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let vertical = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    status_bar::render_header(frame, app, vertical[0]);

    let sidebar_visible = app.sidebar.as_ref().is_some_and(|s| s.visible);
    let board_area = if sidebar_visible {
        let horizontal =
            Layout::horizontal([Constraint::Length(22), Constraint::Min(1)]).split(vertical[1]);
        sidebar::render_sidebar(frame, app, horizontal[0]);
        horizontal[1]
    } else {
        vertical[1]
    };

    let swimlanes = app.visible_swimlanes();
    let card_size = app.card_size();

    if swimlanes.len() <= 1 {
        board::render_board(
            frame,
            &app.projects[swimlanes[0]],
            app,
            board_area,
            card_size,
            true,
        );
    } else {
        let constraints: Vec<Constraint> = swimlanes
            .iter()
            .map(|_| Constraint::Ratio(1, swimlanes.len() as u32))
            .collect();
        let lane_areas = Layout::vertical(constraints).split(board_area);

        for (lane_idx, (&proj_idx, &lane_area)) in
            swimlanes.iter().zip(lane_areas.iter()).enumerate()
        {
            let is_focused_lane = lane_idx == app.focused_swimlane;
            board::render_board(
                frame,
                &app.projects[proj_idx],
                app,
                lane_area,
                card_size,
                is_focused_lane,
            );
        }
    }

    status_bar::render_footer(frame, app, vertical[2]);

    dialog::render_dialog(frame, app);
    linear_picker::render_import_picker(frame, app);
    help::render_help(frame, app);
    debug_inspector::render_debug_inspector(frame, app);
}
