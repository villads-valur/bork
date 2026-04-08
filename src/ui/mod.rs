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

    if let Some(sb) = &app.sidebar {
        if sb.visible {
            let horizontal =
                Layout::horizontal([Constraint::Length(22), Constraint::Min(1)]).split(vertical[1]);

            sidebar::render_sidebar(frame, app, horizontal[0]);
            board::render_board(frame, app, horizontal[1]);
        } else {
            board::render_board(frame, app, vertical[1]);
        }
    } else {
        board::render_board(frame, app, vertical[1]);
    }

    status_bar::render_footer(frame, app, vertical[2]);

    // Render overlays on top of the board
    dialog::render_dialog(frame, app);
    linear_picker::render_import_picker(frame, app);
    help::render_help(frame, app);
    debug_inspector::render_debug_inspector(frame, app);
}
