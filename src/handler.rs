use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::app::{App, ConfirmAction, InputMode, DIALOG_FIELD_COUNT};
use crate::config::{self, AppConfig};
use crate::external::{github, opencode, tmux};
use crate::input::Action;
use crate::types::{AgentStatus, Column, Issue};

pub type PrWakeTx = mpsc::Sender<()>;

pub struct ActionResult {
    pub message: String,
    pub session_to_open: Option<String>,
}

pub enum PostAction {
    None,
    OpenTmuxPopup { session_name: String },
    LaunchAndOpenPopup { issue_index: usize },
}

pub fn handle_action(
    app: &mut App,
    action: Action,
    action_tx: &mpsc::Sender<ActionResult>,
    pr_wake_tx: &PrWakeTx,
) -> PostAction {
    match app.input_mode {
        InputMode::Confirm => {
            handle_confirm(app, action, action_tx);
            PostAction::None
        }
        InputMode::Dialog => {
            handle_dialog(app, action);
            PostAction::None
        }
        InputMode::Normal => handle_normal(app, action, action_tx, pr_wake_tx),
    }
}

fn handle_normal(
    app: &mut App,
    action: Action,
    action_tx: &mpsc::Sender<ActionResult>,
    pr_wake_tx: &PrWakeTx,
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
            let _ = config::save_state(&app.to_state(), &app.config.project_root);
            PostAction::None
        }
        Action::MoveIssueLeft => {
            app.move_issue_left();
            let _ = config::save_state(&app.to_state(), &app.config.project_root);
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

            app.start_confirm(
                format!("Kill session '{}'? (y/n)", session_name),
                ConfirmAction::KillSession { session_name },
            );
            PostAction::None
        }

        Action::CreateIssue => {
            app.open_dialog();
            PostAction::None
        }

        Action::EditIssue => {
            let Some(idx) = app.selected_issue_index() else {
                return PostAction::None;
            };
            let issue = app.issues[idx].clone();
            app.open_edit_dialog(&issue, idx);
            PostAction::None
        }

        Action::DeleteIssue => {
            let Some(issue) = app.selected_issue() else {
                return PostAction::None;
            };
            let Some(idx) = app.selected_issue_index() else {
                return PostAction::None;
            };

            app.start_confirm(
                format!("Delete {}: {}? (y/n)", issue.id, issue.title),
                ConfirmAction::DeleteIssue { issue_index: idx },
            );
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

        Action::SyncPRs => {
            let _ = pr_wake_tx.send(());
            app.set_message("Syncing PRs...");
            PostAction::None
        }

        Action::OpenPR => {
            let Some(issue) = app.selected_issue() else {
                return PostAction::None;
            };
            let Some(pr) = app.pr_for(issue) else {
                app.set_message("No PR found for this issue");
                return PostAction::None;
            };
            let pr_number = pr.number;
            let main_worktree = app.config.project_root.join("main");
            thread::spawn(move || {
                github::open_pr_in_browser(pr_number, &main_worktree);
            });
            PostAction::None
        }

        _ => PostAction::None,
    }
}

fn handle_dialog(app: &mut App, action: Action) {
    match action {
        Action::DialogChar(c) => {
            if let Some(ref mut dialog) = app.dialog {
                dialog.push_char(c);
            }
        }
        Action::DialogBackspace => {
            if let Some(ref mut dialog) = app.dialog {
                dialog.delete_char();
            }
        }

        Action::DialogNextField => {
            let default_prompt = app
                .config
                .default_prompt
                .clone()
                .unwrap_or_else(|| config::DEFAULT_PROMPT_FALLBACK.to_string());

            if let Some(ref mut dialog) = app.dialog {
                let current = dialog.focused_field;
                let next = current + 1;

                if next >= DIALOG_FIELD_COUNT {
                    let _ = dialog;
                    submit_dialog(app);
                    return;
                }

                // Auto-fill prompt with default_prompt when creating a new issue
                if dialog.editing_index.is_none()
                    && current == 0
                    && next == 1
                    && dialog.prompt.is_empty()
                {
                    dialog.prompt = default_prompt;
                }

                dialog.focused_field = next;
            }
        }
        Action::DialogPrevField => {
            if let Some(ref mut dialog) = app.dialog {
                if dialog.focused_field > 0 {
                    dialog.focused_field -= 1;
                }
            }
        }
        Action::DialogSubmit => {
            submit_dialog(app);
        }
        Action::DialogCancel => {
            app.close_dialog();
        }
        _ => {}
    }
}

fn submit_dialog(app: &mut App) {
    let dialog = match app.dialog.take() {
        Some(d) => d,
        None => return,
    };

    app.input_mode = InputMode::Normal;

    let title = dialog.title.trim().to_string();
    if title.is_empty() {
        app.set_message("Title cannot be empty");
        return;
    }

    let prompt = if dialog.prompt.trim().is_empty() {
        None
    } else {
        Some(dialog.prompt.trim().to_string())
    };

    let worktree = if dialog.worktree.trim().is_empty() {
        None
    } else {
        Some(dialog.worktree.trim().to_string())
    };

    if let Some(idx) = dialog.editing_index {
        if idx < app.issues.len() {
            app.issues[idx].title = title;
            app.issues[idx].prompt = prompt;
            app.issues[idx].worktree = worktree;
            app.issues[idx].agent_mode = dialog.agent_mode;
            app.set_message(format!("Updated {}", app.issues[idx].id));
            let _ = config::save_state(&app.to_state(), &app.config.project_root);
        }
        return;
    }

    let id = app.next_issue_id();
    let column = Column::from_index(app.selected_column).unwrap_or(Column::Todo);

    let issue = Issue {
        id: id.clone(),
        title,
        column,
        branch: None,
        worktree,
        tmux_session: None,
        agent_kind: app.config.agent_kind,
        agent_mode: dialog.agent_mode,
        agent_status: AgentStatus::Stopped,
        prompt,
    };

    app.issues.push(issue);
    app.set_message(format!("Created {}", id));

    let count = app.issues_in_column(column).len();
    if count > 0 {
        app.selected_row[app.selected_column] = count - 1;
    }

    let _ = config::save_state(&app.to_state(), &app.config.project_root);
}

fn handle_confirm(app: &mut App, action: Action, action_tx: &mpsc::Sender<ActionResult>) {
    match action {
        Action::ConfirmYes => {
            if let Some(confirm_action) = app.take_confirm_action() {
                match confirm_action {
                    ConfirmAction::KillSession { session_name } => {
                        app.busy_count += 1;
                        let tx = action_tx.clone();
                        let status_file =
                            agent_status_file(&app.config.project_root, &session_name);

                        thread::spawn(move || {
                            let message = match tmux::kill_session(&session_name) {
                                Ok(()) => {
                                    let _ = std::fs::remove_file(&status_file);
                                    format!("Session '{}' killed", session_name)
                                }
                                Err(e) => format!("Failed to kill session: {e}"),
                            };
                            let _ = tx.send(ActionResult {
                                message,
                                session_to_open: None,
                            });
                        });
                    }
                    ConfirmAction::DeleteIssue { issue_index } => {
                        if issue_index < app.issues.len() {
                            let issue = &app.issues[issue_index];
                            let session_name = issue.session_name();
                            let id = issue.id.clone();
                            let status_file =
                                agent_status_file(&app.config.project_root, &session_name);

                            if app.is_session_alive(&session_name) {
                                let tx = action_tx.clone();
                                let sn = session_name.clone();
                                thread::spawn(move || {
                                    let _ = tmux::kill_session(&sn);
                                    let _ = std::fs::remove_file(&status_file);
                                    let _ = tx.send(ActionResult {
                                        message: format!("Deleted {} and killed session", id),
                                        session_to_open: None,
                                    });
                                });
                                app.busy_count += 1;
                            } else {
                                let _ = std::fs::remove_file(&status_file);
                                app.set_message(format!("Deleted {}", id));
                            }

                            app.issues.remove(issue_index);
                            app.clamp_all_rows();
                            let _ = config::save_state(&app.to_state(), &app.config.project_root);
                        }
                    }
                }
            }
        }
        Action::ConfirmNo | Action::Quit => {
            app.cancel_confirm();
        }
        _ => {}
    }
}

fn launch_and_report(issue: Issue, config: AppConfig) -> ActionResult {
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

fn agent_status_file(project_root: &PathBuf, session_name: &str) -> PathBuf {
    config::agent_status_dir(project_root).join(format!("{}.json", session_name))
}
