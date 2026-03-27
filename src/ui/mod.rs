pub mod board;
pub mod card;
pub mod dialog;
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
    board::render_board(frame, app, vertical[1]);
    status_bar::render_footer(frame, app, vertical[2]);

    // Render dialog on top of everything if open
    dialog::render_dialog(frame, app);
}
