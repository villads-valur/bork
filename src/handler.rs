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
    /// If the launch detected an agent session ID, carries (issue_id, agent_session_id)
    /// so main.rs can persist it on the issue.
    pub session_id: Option<(String, String)>,
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
        InputMode::Search => {
            handle_search(app, action);
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

            let session_name = issue.session_name(&app.config.project_name);
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

            let session_name = issue.session_name(&app.config.project_name);

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

        Action::AssignWorktree => {
            let Some(idx) = app.selected_issue_index() else {
                return PostAction::None;
            };
            if let Some(old) = app.issues[idx].worktree.take() {
                app.set_message(format!("Cleared worktree '{old}', re-detecting..."));
            } else {
                app.set_message("No worktree assigned, re-detecting...");
            }
            if app.auto_assign_worktrees() {
                if let Some(wt) = app.issues[idx].worktree.as_ref() {
                    app.set_message(format!("Assigned worktree: {wt}"));
                }
            }
            let _ = config::save_state(&app.to_state(), &app.config.project_root);
            PostAction::None
        }

        Action::SearchStart => {
            app.start_search();
            PostAction::None
        }

        Action::ClearSearch => {
            app.clear_search();
            PostAction::None
        }

        _ => PostAction::None,
    }
}

fn handle_search(app: &mut App, action: Action) {
    match action {
        Action::SearchChar(c) => app.search_push_char(c),
        Action::SearchBackspace => app.search_delete_char(),
        Action::SearchConfirm => app.confirm_search(),
        Action::SearchCancel => app.cancel_search(),
        _ => {}
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
            if let Some(ref mut dialog) = app.dialog {
                let next = dialog.focused_field + 1;

                if next >= DIALOG_FIELD_COUNT {
                    let _ = dialog;
                    submit_dialog(app);
                    return;
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

    if let Some(idx) = dialog.editing_index {
        if idx < app.issues.len() {
            app.issues[idx].title = title;
            app.issues[idx].prompt = prompt;
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
        tmux_session: None,
        agent_kind: app.config.agent_kind,
        agent_mode: dialog.agent_mode,
        agent_status: AgentStatus::Stopped,
        prompt,
        worktree: None,
        done_at: None,
        session_id: None,
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
                                session_id: None,
                            });
                        });
                    }
                    ConfirmAction::DeleteIssue { issue_index } => {
                        if issue_index < app.issues.len() {
                            let issue = &app.issues[issue_index];
                            let session_name = issue.session_name(&app.config.project_name);
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
                                        session_id: None,
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
        Ok((session_name, agent_sid)) => ActionResult {
            message: format!("Session '{}' started", session_name),
            session_to_open: Some(session_name),
            session_id: agent_sid.map(|sid| (issue.id.clone(), sid)),
        },
        Err(e) => ActionResult {
            message: format!("Failed to launch: {e}"),
            session_to_open: None,
            session_id: None,
        },
    }
}

fn agent_status_file(project_root: &PathBuf, session_name: &str) -> PathBuf {
    config::agent_status_dir(project_root).join(format!("{}.json", session_name))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::*;
    use crate::app::{App, DialogState};
    use crate::config::DEFAULT_DONE_SESSION_TTL;
    use crate::input::Action;
    use crate::types::Column;

    fn pr_wake_tx() -> mpsc::Sender<()> {
        mpsc::channel().0
    }

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".to_string(),
            project_root: PathBuf::from("/tmp/test-bork"),
            agent_kind: crate::types::AgentKind::OpenCode,
            default_prompt: Some("Check AGENTS.md for context.".to_string()),
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
        }
    }

    fn test_app() -> App {
        let state = crate::config::AppState { issues: vec![] };
        App::new(test_config(), state)
    }

    // ================================================================
    // Dialog: prompt field stays empty on field navigation
    // ================================================================

    #[test]
    fn dialog_next_field_does_not_auto_fill_prompt() {
        let mut app = test_app();
        app.open_dialog();

        // Type a title
        handle_action(
            &mut app,
            Action::DialogChar('H'),
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar('i'),
            &mpsc::channel().0,
            &pr_wake_tx(),
        );

        // Move from title (field 0) to prompt (field 1)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.focused_field, 1);
        assert_eq!(
            dialog.prompt, "",
            "prompt should remain empty after navigating from title"
        );
    }

    #[test]
    fn dialog_next_field_preserves_user_typed_prompt() {
        let mut app = test_app();
        app.open_dialog();

        // Move to prompt field
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );

        // Type something in the prompt
        handle_action(
            &mut app,
            Action::DialogChar('g'),
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar('o'),
            &mpsc::channel().0,
            &pr_wake_tx(),
        );

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.prompt, "go");
    }

    #[test]
    fn dialog_next_field_advances_through_all_fields() {
        let mut app = test_app();
        app.open_dialog();

        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);

        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 1);

        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 2);
        // Field 2 is the last (mode). Next field from here submits the dialog.
    }

    #[test]
    fn dialog_prev_field_goes_back() {
        let mut app = test_app();
        app.open_dialog();

        // Advance to prompt
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 1);

        // Go back to title
        handle_action(
            &mut app,
            Action::DialogPrevField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn dialog_prev_field_does_not_go_below_zero() {
        let mut app = test_app();
        app.open_dialog();

        handle_action(
            &mut app,
            Action::DialogPrevField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn dialog_space_char_appended_to_prompt() {
        let mut app = test_app();
        app.open_dialog();

        // Move to prompt field
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );

        handle_action(
            &mut app,
            Action::DialogChar('a'),
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar(' '),
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar('b'),
            &mpsc::channel().0,
            &pr_wake_tx(),
        );

        assert_eq!(app.dialog.as_ref().unwrap().prompt, "a b");
    }

    #[test]
    fn dialog_cancel_closes_dialog() {
        let mut app = test_app();
        app.open_dialog();
        assert!(app.dialog.is_some());

        handle_action(
            &mut app,
            Action::DialogCancel,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );
        assert!(app.dialog.is_none());
    }

    #[test]
    fn edit_dialog_does_not_inject_default_prompt() {
        let mut app = test_app();

        // Create an issue first
        app.issues.push(crate::types::Issue {
            id: "bork-1".to_string(),
            title: "Test".to_string(),
            column: Column::Todo,
            worktree: Some("main".to_string()),
            tmux_session: None,
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            agent_status: crate::types::AgentStatus::Stopped,
            prompt: None,
            done_at: None,
            session_id: None,
        });

        // Open edit dialog
        let issue = app.issues[0].clone();
        app.open_edit_dialog(&issue, 0);

        // Move from title to prompt
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
        );

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(
            dialog.prompt, "",
            "edit dialog prompt should stay empty when issue had no prompt"
        );
    }
}
