use std::sync::mpsc;
use std::thread;

use crate::app::{App, InputMode};
use crate::config::AppConfig;
use crate::external::{opencode, tmux};
use crate::input::Action;

pub struct ActionResult {
    pub message: String,
    pub session_to_open: Option<String>,
}

/// Command that the main loop must execute after handle_action returns.
/// These are separated because some operations (like tmux popup) need to
/// happen outside the handler, after state is saved.
pub enum PostAction {
    None,
    OpenTmuxPopup { session_name: String },
    LaunchAndOpenPopup { issue_index: usize },
}

pub fn handle_action(
    app: &mut App,
    action: Action,
    action_tx: &mpsc::Sender<ActionResult>,
) -> PostAction {
    match app.input_mode {
        InputMode::Confirm => {
            handle_confirm(app, action, action_tx);
            PostAction::None
        }
        InputMode::Normal => handle_normal(app, action, action_tx),
    }
}

fn handle_normal(
    app: &mut App,
    action: Action,
    action_tx: &mpsc::Sender<ActionResult>,
) -> PostAction {
    match action {
        Action::Quit => {
            app.should_quit = true;
            PostAction::None
        }

        Action::MoveUp => {
            app.move_selection_up();
            PostAction::None
        }
        Action::MoveDown => {
            app.move_selection_down();
            PostAction::None
        }
        Action::FocusLeft => {
            app.focus_left();
            PostAction::None
        }
        Action::FocusRight => {
            app.focus_right();
            PostAction::None
        }
        Action::JumpColumnLeft => {
            app.jump_column_left();
            PostAction::None
        }
        Action::JumpColumnRight => {
            app.jump_column_right();
            PostAction::None
        }

        Action::ScrollToTop => {
            app.scroll_to_top();
            PostAction::None
        }
        Action::ScrollToBottom => {
            app.scroll_to_bottom();
            PostAction::None
        }

        Action::MoveIssueRight => {
            app.move_issue_right();
            PostAction::None
        }
        Action::MoveIssueLeft => {
            app.move_issue_left();
            PostAction::None
        }

        Action::KillSession => {
            let Some(issue) = app.selected_issue() else {
                return PostAction::None;
            };

            let session_name = issue.session_name();
            if !app.is_session_alive(&session_name) {
                app.set_message("No active session to kill");
                return PostAction::None;
            }

            app.confirm_message = Some(format!("Kill session '{}'? (y/n)", session_name));
            app.confirm_session = Some(session_name);
            app.input_mode = InputMode::Confirm;
            PostAction::None
        }

        Action::OpenSession => {
            let Some(issue) = app.selected_issue() else {
                return PostAction::None;
            };

            let session_name = issue.session_name();

            if app.is_session_alive(&session_name) {
                return PostAction::OpenTmuxPopup { session_name };
            }

            if let Some(idx) = app.selected_issue_index() {
                app.busy_count += 1;
                app.set_message("Launching session...");

                let issue = app.issues[idx].clone();
                let config = app.config.clone();
                let tx = action_tx.clone();

                thread::spawn(move || {
                    let result = launch_and_report(issue, config);
                    let _ = tx.send(result);
                });

                return PostAction::LaunchAndOpenPopup { issue_index: idx };
            }

            PostAction::None
        }

        Action::ConfirmYes | Action::ConfirmNo | Action::Noop => PostAction::None,
    }
}

fn handle_confirm(app: &mut App, action: Action, action_tx: &mpsc::Sender<ActionResult>) {
    match action {
        Action::ConfirmYes => {
            if let Some(session_name) = app.confirm_session.take() {
                app.busy_count += 1;
                let tx = action_tx.clone();

                thread::spawn(move || {
                    let message = match tmux::kill_session(&session_name) {
                        Ok(()) => format!("Session '{}' killed", session_name),
                        Err(e) => format!("Failed to kill session: {e}"),
                    };
                    let _ = tx.send(ActionResult {
                        message,
                        session_to_open: None,
                    });
                });
            }
            app.confirm_message = None;
            app.input_mode = InputMode::Normal;
        }
        Action::ConfirmNo | Action::Quit => {
            app.confirm_message = None;
            app.confirm_session = None;
            app.input_mode = InputMode::Normal;
        }
        _ => {}
    }
}

fn launch_and_report(issue: crate::types::Issue, config: AppConfig) -> ActionResult {
    match opencode::launch_session(&issue, &config) {
        Ok(session_name) => ActionResult {
            message: format!("Session '{}' started", session_name),
            session_to_open: Some(session_name),
        },
        Err(e) => ActionResult {
            message: format!("Failed to launch: {e}"),
            session_to_open: None,
        },
    }
}
