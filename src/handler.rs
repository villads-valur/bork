use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;

use crate::app::{App, ConfirmAction, ImportSource, InputMode, LinearPickerContext};
use crate::config::{self, AppConfig};
use crate::external::{github, opencode, tmux, tuicr};
use crate::input::Action;
use crate::lock;
use crate::types::{AgentMode, Column, Issue, IssueKind};

pub type PrWakeTx = mpsc::Sender<()>;
pub type LinearWakeTx = mpsc::Sender<()>;
pub type GitWakeTx = mpsc::Sender<()>;

pub struct ActionResult {
    pub message: String,
    pub session_to_open: Option<String>,
    pub popup_title: Option<String>,
    pub session_id: Option<(String, String)>,
}

pub enum PostAction {
    None,
    OpenTmuxPopup {
        session_name: String,
        popup_title: String,
    },
    LaunchAndOpenPopup {
        issue_index: usize,
        popup_title: String,
    },
    OpenEditor {
        initial_content: String,
    },
    SwitchProject {
        index: usize,
    },
}

pub fn handle_action(
    app: &mut App,
    action: Action,
    action_tx: &mpsc::Sender<ActionResult>,
    pr_wake_tx: &PrWakeTx,
    linear_wake_tx: &LinearWakeTx,
    git_wake_tx: &GitWakeTx,
) -> PostAction {
    match app.input_mode {
        InputMode::Confirm => {
            handle_confirm(app, action, action_tx);
            PostAction::None
        }
        InputMode::Dialog => handle_dialog(app, action),
        InputMode::Search => {
            handle_search(app, action);
            PostAction::None
        }
        InputMode::LinearPicker => {
            handle_linear_picker(app, action, linear_wake_tx, pr_wake_tx);
            PostAction::None
        }
        InputMode::Help => {
            handle_help(app, action);
            PostAction::None
        }
        InputMode::DebugInspector => {
            handle_debug_inspector(app, action);
            PostAction::None
        }
        InputMode::Normal => handle_normal(app, action, action_tx, pr_wake_tx, git_wake_tx),
        InputMode::Sidebar => handle_sidebar(app, action),
    }
}

fn handle_normal(
    app: &mut App,
    action: Action,
    action_tx: &mpsc::Sender<ActionResult>,
    pr_wake_tx: &PrWakeTx,
    git_wake_tx: &GitWakeTx,
) -> PostAction {
    match action {
        Action::Quit => {
            app.should_quit = true;
            PostAction::None
        }

        Action::MoveUp => {
            app.active_project_mut().move_selection_up();
            PostAction::None
        }
        Action::MoveDown => {
            app.active_project_mut().move_selection_down();
            PostAction::None
        }
        Action::FocusLeft => {
            app.active_project_mut().focus_left();
            PostAction::None
        }
        Action::FocusRight => {
            app.active_project_mut().focus_right();
            PostAction::None
        }
        Action::JumpColumnLeft => {
            app.active_project_mut().jump_column_left();
            PostAction::None
        }
        Action::JumpColumnRight => {
            app.active_project_mut().jump_column_right();
            PostAction::None
        }

        Action::ScrollToTop => {
            app.active_project_mut().scroll_to_top();
            PostAction::None
        }
        Action::ScrollToBottom => {
            app.active_project_mut().scroll_to_bottom();
            PostAction::None
        }

        Action::MoveIssueRight => {
            let p = app.active_project_mut();
            p.move_issue_right();
            p.mark_dirty();
            PostAction::None
        }
        Action::MoveIssueLeft => {
            let p = app.active_project_mut();
            p.move_issue_left();
            p.mark_dirty();
            PostAction::None
        }
        Action::MoveToDone => {
            let p = app.active_project_mut();
            p.move_to_done();
            p.mark_dirty();
            PostAction::None
        }
        Action::MoveToTodo => {
            let p = app.active_project_mut();
            p.move_to_todo();
            p.mark_dirty();
            PostAction::None
        }

        Action::KillSession => {
            let Some(issue) = app.active_project().selected_issue() else {
                return PostAction::None;
            };

            let session_name = issue.session_name(&app.active_project().config.project_name);
            if !app.active_project().is_session_alive(&session_name) {
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

        Action::AddIssue => {
            let column =
                Column::from_index(app.active_project().selected_column).unwrap_or(Column::Todo);
            app.open_dialog_in_column(column);
            PostAction::None
        }

        Action::EditIssue => {
            let Some(idx) = app.active_project().selected_issue_index() else {
                return PostAction::None;
            };
            let issue = app.active_project().issues[idx].clone();
            app.open_edit_dialog(&issue, idx);
            PostAction::None
        }

        Action::DeleteIssue => {
            let Some(issue) = app.active_project().selected_issue() else {
                return PostAction::None;
            };
            let Some(idx) = app.active_project().selected_issue_index() else {
                return PostAction::None;
            };

            app.start_confirm(
                format!("Delete {}: {}? (y/n)", issue.id, issue.title),
                ConfirmAction::DeleteIssue { issue_index: idx },
            );
            PostAction::None
        }

        Action::OpenLinearPicker => {
            app.open_import_picker();
            PostAction::None
        }

        Action::ShowHelp => {
            app.open_help();
            PostAction::None
        }

        Action::ToggleSidebar => {
            if let Some(ref mut sidebar) = app.sidebar {
                sidebar.visible = true;
                sidebar.focused = true;
                sidebar.selected = app.focused_project;
                app.input_mode = InputMode::Sidebar;
            }
            PostAction::None
        }

        Action::NextSwimlane => {
            let count = app.visible_swimlane_count();
            if count > 1 {
                app.focused_swimlane = (app.focused_swimlane + 1) % count;
            }
            PostAction::None
        }
        Action::PrevSwimlane => {
            let count = app.visible_swimlane_count();
            if count > 1 {
                if app.focused_swimlane == 0 {
                    app.focused_swimlane = count - 1;
                } else {
                    app.focused_swimlane -= 1;
                }
            }
            PostAction::None
        }

        Action::OpenTerminal => {
            let session_name = format!("{}-terminal", app.active_project().config.project_name);
            let popup_title = "Terminal".to_string();

            if app.active_project().is_session_alive(&session_name) {
                return PostAction::OpenTmuxPopup {
                    session_name,
                    popup_title,
                };
            }

            app.busy_count += 1;
            app.set_message("Opening terminal...");
            let tx = action_tx.clone();
            let project_root = app.active_project().config.project_root.clone();

            thread::spawn(move || {
                let result = match tmux::create_session(&session_name, &project_root) {
                    Ok(()) => ActionResult {
                        message: format!("Terminal session '{}' ready", session_name),
                        session_to_open: Some(session_name),
                        popup_title: Some(popup_title),
                        session_id: None,
                    },
                    Err(e) => ActionResult {
                        message: format!("Failed to open terminal: {e}"),
                        session_to_open: None,
                        popup_title: None,
                        session_id: None,
                    },
                };
                let _ = tx.send(result);
            });

            PostAction::None
        }

        Action::OpenSession => {
            let Some(idx) = app.active_project().selected_issue_index() else {
                return PostAction::None;
            };
            let issue = app.active_project().issues[idx].clone();

            if issue.kind == IssueKind::NonAgentic {
                app.open_edit_dialog(&issue, idx);
                return PostAction::None;
            }

            let session_name = issue.session_name(&app.active_project().config.project_name);
            let popup_title = format!("{}: {}", issue.id, issue.title);

            if app.active_project().is_session_alive(&session_name) {
                return PostAction::OpenTmuxPopup {
                    session_name,
                    popup_title,
                };
            }

            app.busy_count += 1;
            app.set_message("Launching session...");

            let config = app.active_project().config.clone();
            let tx = action_tx.clone();

            thread::spawn(move || {
                let result = launch_and_report(issue, config);
                let _ = tx.send(result);
            });

            PostAction::LaunchAndOpenPopup {
                issue_index: idx,
                popup_title,
            }
        }

        Action::OpenReview | Action::OpenReviewPR => {
            if !app.active_project().tuicr_available {
                return PostAction::None;
            }
            let Some(issue) = app.active_project().selected_issue() else {
                return PostAction::None;
            };
            let Some(wt) = issue.worktree.clone() else {
                app.set_message("No worktree assigned");
                return PostAction::None;
            };
            let session_name = issue.session_name(&app.active_project().config.project_name);
            if !app.active_project().is_session_alive(&session_name) {
                app.set_message("No active session");
                return PostAction::None;
            }
            let pr_mode = action == Action::OpenReviewPR;
            let popup_title = format!("{}: {}", issue.id, issue.title);
            let worktree_path = app.active_project().config.project_root.join(&wt);
            let tx = action_tx.clone();
            app.busy_count += 1;
            app.set_message(if pr_mode {
                "Opening tuicr --pr..."
            } else {
                "Opening tuicr..."
            });

            thread::spawn(move || {
                let result = match tuicr::open_in_session(&session_name, &worktree_path, pr_mode) {
                    Ok(()) => ActionResult {
                        message: "tuicr ready".to_string(),
                        session_to_open: Some(session_name),
                        popup_title: Some(popup_title),
                        session_id: None,
                    },
                    Err(e) => ActionResult {
                        message: format!("Failed to open tuicr: {e}"),
                        session_to_open: None,
                        popup_title: None,
                        session_id: None,
                    },
                };
                let _ = tx.send(result);
            });

            PostAction::None
        }

        Action::SyncPRs => {
            let _ = pr_wake_tx.send(());
            app.set_message("Syncing PRs...");
            PostAction::None
        }

        Action::OpenPR => {
            let Some(issue) = app.active_project().selected_issue() else {
                return PostAction::None;
            };
            let Some(pr) = app.active_project().pr_for(issue) else {
                app.set_message("No PR found for this issue");
                return PostAction::None;
            };
            let pr_number = pr.number;
            let main_worktree = app.active_project().config.project_root.join("main");
            thread::spawn(move || {
                github::open_pr_in_browser(pr_number, &main_worktree);
            });
            PostAction::None
        }

        Action::OpenLinear => {
            let Some(issue) = app.active_project().selected_issue() else {
                return PostAction::None;
            };
            let Some(url) = issue.linear_url.clone() else {
                app.set_message("No Linear issue linked");
                return PostAction::None;
            };
            thread::spawn(move || {
                let _ = Command::new("open").arg(&url).output();
            });
            PostAction::None
        }

        Action::AssignWorktree => {
            let Some(idx) = app.active_project().selected_issue_index() else {
                return PostAction::None;
            };
            if let Some(old) = app.active_project_mut().issues[idx].worktree.take() {
                app.set_message(format!("Cleared worktree '{old}', re-detecting..."));
            } else {
                app.set_message("No worktree assigned, re-detecting...");
            }
            if app.active_project_mut().auto_assign_worktrees() {
                if let Some(wt) = app.active_project().issues[idx].worktree.as_ref() {
                    app.set_message(format!("Assigned worktree: {wt}"));
                }
            }
            let _ = git_wake_tx.send(());
            app.active_project_mut().mark_dirty();
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

        Action::DebugReset => {
            if !app.active_project().config.debug {
                return PostAction::None;
            }
            lock::release_lock(&app.active_project().config.project_root);
            let session_name = app.active_project().config.project_name.clone();
            let _ = tmux::kill_session(&session_name);
            app.should_quit = true;
            PostAction::None
        }

        Action::DebugInspect => {
            if !app.active_project().config.debug {
                return PostAction::None;
            }
            let Some(issue) = app.active_project().selected_issue().cloned() else {
                app.set_message("No issue selected");
                return PostAction::None;
            };
            let json = serde_json::to_string_pretty(&issue).unwrap_or_else(|e| format!("{e}"));
            app.open_debug_inspector(json);
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

fn handle_help(app: &mut App, action: Action) {
    match action {
        Action::CloseHelp => app.close_help(),
        Action::Quit => {
            app.should_quit = true;
        }
        _ => {}
    }
}

fn handle_debug_inspector(app: &mut App, action: Action) {
    match action {
        Action::DebugInspectorClose => app.close_debug_inspector(),
        Action::DebugInspectorScrollDown => {
            app.debug_inspector_scroll = app.debug_inspector_scroll.saturating_add(1);
        }
        Action::DebugInspectorScrollUp => {
            app.debug_inspector_scroll = app.debug_inspector_scroll.saturating_sub(1);
        }
        Action::DebugInspectorScrollTop => {
            app.debug_inspector_scroll = 0;
        }
        Action::DebugInspectorScrollBottom => {
            let lines = app.debug_inspector_line_count();
            app.debug_inspector_scroll = lines.saturating_sub(1);
        }
        Action::Quit => {
            app.should_quit = true;
        }
        _ => {}
    }
}

fn handle_dialog(app: &mut App, action: Action) -> PostAction {
    let on_linear = app.dialog.as_ref().is_some_and(|d| d.is_on_linear_field());
    let on_github = app.dialog.as_ref().is_some_and(|d| d.is_on_github_field());

    if on_linear {
        match action {
            Action::DialogChar(' ') => {
                app.picker_tab = ImportSource::Linear;
                app.open_linear_picker_with_context(LinearPickerContext::Attach);
                return PostAction::None;
            }
            Action::DialogNextField => {
                submit_dialog(app);
                return PostAction::None;
            }
            Action::DialogBackspace | Action::DialogDelete => {
                if let Some(dialog) = app.dialog.as_mut() {
                    dialog.linear_issue = None;
                    dialog.linear_detached = true;
                }
                return PostAction::None;
            }
            Action::DialogChar(_) => return PostAction::None,
            _ => {}
        }
    }

    if on_github {
        match action {
            Action::DialogChar(' ') => {
                app.picker_tab = ImportSource::GitHub;
                app.open_import_picker_with_context(LinearPickerContext::Attach);
                return PostAction::None;
            }
            Action::DialogNextField => {
                submit_dialog(app);
                return PostAction::None;
            }
            Action::DialogBackspace | Action::DialogDelete => {
                if let Some(dialog) = app.dialog.as_mut() {
                    dialog.github_pr = None;
                    dialog.github_pr_cleared = true;
                }
                return PostAction::None;
            }
            Action::DialogChar(_) => return PostAction::None,
            _ => {}
        }
    }

    match action {
        Action::DialogSubmit => {
            submit_dialog(app);
            return PostAction::None;
        }
        Action::DialogCancel => {
            app.close_dialog();
            return PostAction::None;
        }
        Action::DialogNextField => {
            if let Some(dialog) = app.dialog.as_mut() {
                dialog.next_field();
            }
            return PostAction::None;
        }
        Action::DialogOpenEditor => {
            let Some(dialog) = app.dialog.as_ref() else {
                return PostAction::None;
            };
            return PostAction::OpenEditor {
                initial_content: dialog.prompt_text(),
            };
        }
        _ => {}
    }

    let Some(dialog) = app.dialog.as_mut() else {
        return PostAction::None;
    };

    match action {
        Action::DialogChar(c) => dialog.push_char(c),
        Action::DialogBackspace => dialog.delete_char(),
        Action::DialogDelete => dialog.delete_char_forward(),
        Action::DialogMoveLeft => dialog.move_cursor_left(),
        Action::DialogMoveRight => dialog.move_cursor_right(),
        Action::DialogMoveStart => dialog.move_cursor_start(),
        Action::DialogMoveEnd => dialog.move_cursor_end(),
        Action::DialogDeleteWord => dialog.delete_word_backward(),
        Action::DialogClearToStart => dialog.clear_to_start(),
        Action::DialogPromptKey(key_event) => {
            dialog.prompt.input(key_event);
        }
        Action::DialogPrevField => dialog.prev_field(),
        _ => {}
    }

    PostAction::None
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

    let prompt_text = dialog.prompt_text();
    let prompt = if prompt_text.trim().is_empty() {
        None
    } else {
        Some(prompt_text)
    };

    if let Some(idx) = dialog.editing_index {
        let p = &mut app.projects[app.focused_project];
        if idx < p.issues.len() {
            p.issues[idx].title = title;
            p.issues[idx].prompt = prompt;
            p.issues[idx].agent_mode = dialog.agent_mode;
            p.issues[idx].kind = dialog.kind;

            apply_linear_fields(&mut p.issues[idx], &dialog);
            apply_pr_fields(&mut p.issues[idx], &dialog);

            app.set_message(format!(
                "Updated {}",
                app.projects[app.focused_project].issues[idx].id
            ));
            app.project_mut().mark_dirty();
        }
        return;
    }

    let id = app.project().next_issue_id();
    let column = dialog.target_column.unwrap_or(Column::Todo);
    let column_index = column.index();

    let mut issue = Issue {
        id: id.clone(),
        title,
        kind: dialog.kind,
        column,
        agent_kind: app.project().config.agent_kind,
        agent_mode: dialog.agent_mode,
        prompt,
        worktree: None,
        done_at: None,
        session_id: None,
        linear_id: None,
        linear_identifier: None,
        linear_url: None,
        linear_imported: false,
        pr_number: None,
        pr_imported: false,
    };

    apply_linear_fields(&mut issue, &dialog);
    apply_pr_fields(&mut issue, &dialog);

    let p = &mut app.projects[app.focused_project];
    p.issues.push(issue);
    app.set_message(format!("Created {}", id));

    let p = &mut app.projects[app.focused_project];
    p.selected_column = column_index;
    let count = p.issues_in_column(column).len();
    if count > 0 {
        p.selected_row[column_index] = count - 1;
    }

    p.mark_dirty();
}

fn apply_linear_fields(issue: &mut Issue, dialog: &crate::app::DialogState) {
    if dialog.linear_detached {
        issue.linear_id = None;
        issue.linear_identifier = None;
        issue.linear_url = None;
        issue.linear_imported = false;
    } else if let Some(ref li) = dialog.linear_issue {
        issue.linear_id = Some(li.id.clone());
        issue.linear_identifier = Some(li.identifier.clone());
        issue.linear_url = Some(li.url.clone());
        // Attached via dialog, not imported — don't sync title
        issue.linear_imported = false;
    }
}

fn apply_pr_fields(issue: &mut Issue, dialog: &crate::app::DialogState) {
    if dialog.github_pr_cleared {
        issue.pr_number = None;
        issue.pr_imported = false;
    } else if let Some(ref pr) = dialog.github_pr {
        issue.pr_number = Some(pr.number);
        // Attached via dialog, not imported — don't sync title
        issue.pr_imported = false;
    }
}

fn handle_linear_picker(
    app: &mut App,
    action: Action,
    linear_wake_tx: &LinearWakeTx,
    pr_wake_tx: &PrWakeTx,
) {
    match action {
        Action::LinearPickerClose => {
            app.close_linear_picker();
        }
        Action::PickerSwitchTab => {
            let has_linear = !app.project().live().linear_issues.is_empty();
            let has_github = app.project().has_github_prs();
            if has_linear && has_github {
                app.picker_tab = match app.picker_tab {
                    ImportSource::Linear => ImportSource::GitHub,
                    ImportSource::GitHub => ImportSource::Linear,
                };
                if let Some(ref mut picker) = app.linear_picker {
                    picker.selected = 0;
                }
            }
        }
        Action::LinearPickerDown => {
            let count = match app.picker_tab {
                ImportSource::Linear => app.filtered_linear_issues().len(),
                ImportSource::GitHub => app.filtered_github_prs().len(),
            };
            if let Some(ref mut picker) = app.linear_picker {
                if count > 0 && picker.selected < count - 1 {
                    picker.selected += 1;
                }
            }
        }
        Action::LinearPickerUp => {
            if let Some(ref mut picker) = app.linear_picker {
                if picker.selected > 0 {
                    picker.selected -= 1;
                }
            }
        }
        Action::LinearPickerChar(c) => {
            if let Some(ref mut picker) = app.linear_picker {
                picker.search.push(c);
                picker.selected = 0;
            }
        }
        Action::LinearPickerBackspace => {
            if let Some(ref mut picker) = app.linear_picker {
                picker.search.pop();
                picker.selected = 0;
            }
        }
        Action::LinearPickerSelect => match (app.linear_picker_context, app.picker_tab) {
            (LinearPickerContext::Attach, ImportSource::Linear) => attach_linear_to_dialog(app),
            (LinearPickerContext::Attach, ImportSource::GitHub) => attach_github_to_dialog(app),
            (_, ImportSource::Linear) => import_linear_issue(app),
            (_, ImportSource::GitHub) => import_github_pr(app),
        },
        Action::LinearPickerRefresh => match app.picker_tab {
            ImportSource::Linear => {
                let _ = linear_wake_tx.send(());
                app.set_message("Refreshing Linear issues...");
            }
            ImportSource::GitHub => {
                let _ = pr_wake_tx.send(());
                app.set_message("Refreshing GitHub PRs...");
            }
        },
        _ => {}
    }
}

fn attach_linear_to_dialog(app: &mut App) {
    let filtered = app.filtered_linear_issues();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let linear_issue = match filtered.get(selected_idx) {
        Some(i) => (*i).clone(),
        None => return,
    };

    app.linear_picker = None;
    app.input_mode = InputMode::Dialog;
    app.linear_picker_context = LinearPickerContext::Import;

    if let Some(ref mut dialog) = app.dialog {
        dialog.linear_issue = Some(linear_issue);
        dialog.linear_detached = false;
    }
}

fn import_linear_issue(app: &mut App) {
    let filtered = app.filtered_linear_issues();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let linear_issue = match filtered.get(selected_idx) {
        Some(i) => (*i).clone(),
        None => return,
    };

    let id = linear_issue.identifier.to_lowercase();

    if app.project().issues.iter().any(|i| i.id == id) {
        app.set_message(format!(
            "{} is already on the board",
            linear_issue.identifier
        ));
        app.close_linear_picker();
        return;
    }

    let issue = Issue {
        id,
        title: linear_issue.title.clone(),
        kind: IssueKind::Agentic,
        column: Column::Todo,
        agent_kind: app.project().config.agent_kind,
        agent_mode: crate::types::AgentMode::Plan,
        prompt: None,
        worktree: None,
        done_at: None,
        session_id: None,
        linear_id: Some(linear_issue.id.clone()),
        linear_identifier: Some(linear_issue.identifier.clone()),
        linear_url: Some(linear_issue.url.clone()),
        linear_imported: true,
        pr_number: None,
        pr_imported: false,
    };

    app.project_mut().issues.push(issue);
    app.set_message(format!("Imported {}", linear_issue.identifier));
    app.close_linear_picker();

    let p = &mut app.projects[app.focused_project];
    let count = p.issues_in_column(Column::Todo).len();
    if count > 0 {
        p.selected_column = 0;
        p.selected_row[0] = count - 1;
    }

    p.mark_dirty();
}

fn import_github_pr(app: &mut App) {
    let filtered = app.filtered_github_prs();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let pr = match filtered.get(selected_idx) {
        Some(pr) => (*pr).clone(),
        None => return,
    };

    if app
        .project()
        .issues
        .iter()
        .any(|i| i.pr_number == Some(pr.number))
    {
        app.set_message(format!("PR #{} is already on the board", pr.number));
        app.close_linear_picker();
        return;
    }

    let id = app.project().next_issue_id();
    let issue = Issue {
        id,
        title: pr.title.clone(),
        kind: IssueKind::Agentic,
        column: Column::CodeReview,
        agent_kind: app.project().config.agent_kind,
        agent_mode: AgentMode::Plan,
        prompt: None,
        worktree: None,
        done_at: None,
        session_id: None,
        linear_id: None,
        linear_identifier: None,
        linear_url: None,
        linear_imported: false,
        pr_number: Some(pr.number),
        pr_imported: true,
    };

    app.project_mut().issues.push(issue);
    app.set_message(format!("Imported PR #{}", pr.number));
    app.close_linear_picker();

    let p = &mut app.projects[app.focused_project];
    let count = p.issues_in_column(Column::CodeReview).len();
    if count > 0 {
        p.selected_column = Column::CodeReview.index();
        p.selected_row[Column::CodeReview.index()] = count - 1;
    }

    p.mark_dirty();
}

fn attach_github_to_dialog(app: &mut App) {
    let filtered = app.filtered_github_prs();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let pr = match filtered.get(selected_idx) {
        Some(pr) => (*pr).clone(),
        None => return,
    };

    app.linear_picker = None;
    app.input_mode = InputMode::Dialog;
    app.linear_picker_context = LinearPickerContext::Import;

    if let Some(ref mut dialog) = app.dialog {
        dialog.github_pr = Some(pr);
        dialog.github_pr_cleared = false;
    }
}

fn handle_sidebar(app: &mut App, action: Action) -> PostAction {
    match action {
        Action::ToggleSidebar | Action::SidebarSelect => {
            if let Some(ref mut sidebar) = app.sidebar {
                let switch =
                    action == Action::SidebarSelect && sidebar.selected != app.focused_project;

                sidebar.focused = false;
                sidebar.visible = false;
                app.input_mode = InputMode::Normal;

                if switch {
                    return PostAction::SwitchProject {
                        index: sidebar.selected,
                    };
                }
            }
            PostAction::None
        }
        Action::SidebarDown => {
            if let Some(ref mut sidebar) = app.sidebar {
                if sidebar.selected + 1 < app.projects.len() {
                    sidebar.selected += 1;
                }
            }
            PostAction::None
        }
        Action::SidebarUp => {
            if let Some(ref mut sidebar) = app.sidebar {
                if sidebar.selected > 0 {
                    sidebar.selected -= 1;
                }
            }
            PostAction::None
        }
        Action::SidebarToggleSwimlane => {
            if let Some(ref mut sidebar) = app.sidebar {
                let idx = sidebar.selected;
                if idx == app.focused_project {
                    // Can't toggle the focused project off
                    PostAction::None
                } else if let Some(pos) = sidebar.swimlane_indices.iter().position(|&i| i == idx) {
                    sidebar.swimlane_indices.remove(pos);
                    // Reset swimlane focus if it was pointing at a removed lane
                    let total = 1 + sidebar.swimlane_indices.len();
                    if app.focused_swimlane >= total {
                        app.focused_swimlane = 0;
                    }
                    PostAction::None
                } else if sidebar.swimlane_indices.len() < 2 {
                    sidebar.swimlane_indices.push(idx);
                    PostAction::None
                } else {
                    app.set_message("Maximum 3 projects visible (1 focused + 2 extra)");
                    PostAction::None
                }
            } else {
                PostAction::None
            }
        }
        Action::Quit => {
            app.should_quit = true;
            PostAction::None
        }
        _ => PostAction::None,
    }
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
                            agent_status_file(&app.project().config.project_root, &session_name);

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
                                popup_title: None,
                                session_id: None,
                            });
                        });
                    }
                    ConfirmAction::DeleteIssue { issue_index } => {
                        if issue_index < app.project().issues.len() {
                            let issue = &app.project().issues[issue_index];
                            let session_name =
                                issue.session_name(&app.project().config.project_name);
                            let id = issue.id.clone();
                            let status_file = agent_status_file(
                                &app.project().config.project_root,
                                &session_name,
                            );

                            if app.project().is_session_alive(&session_name) {
                                let tx = action_tx.clone();
                                let sn = session_name.clone();
                                thread::spawn(move || {
                                    let _ = tmux::kill_session(&sn);
                                    let _ = std::fs::remove_file(&status_file);
                                    let _ = tx.send(ActionResult {
                                        message: format!("Deleted {} and killed session", id),
                                        session_to_open: None,
                                        popup_title: None,
                                        session_id: None,
                                    });
                                });
                                app.busy_count += 1;
                            } else {
                                let _ = std::fs::remove_file(&status_file);
                                app.set_message(format!("Deleted {}", id));
                            }

                            let p = app.project_mut();
                            p.issues.remove(issue_index);
                            p.clamp_all_rows();
                            p.mark_dirty();
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
            popup_title: None,
            session_id: agent_sid.map(|sid| (issue.id.clone(), sid)),
        },
        Err(e) => ActionResult {
            message: format!("Failed to launch: {e}"),
            session_to_open: None,
            popup_title: None,
            session_id: None,
        },
    }
}

fn agent_status_file(project_root: &Path, session_name: &str) -> PathBuf {
    config::agent_status_dir(project_root).join(format!("{}.json", session_name))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::*;
    use crate::app::App;
    use crate::config::DEFAULT_DONE_SESSION_TTL;
    use crate::input::Action;
    use crate::types::Column;

    fn pr_wake_tx() -> mpsc::Sender<()> {
        mpsc::channel().0
    }

    fn linear_wake_tx() -> mpsc::Sender<()> {
        mpsc::channel().0
    }

    fn git_wake_tx() -> mpsc::Sender<()> {
        mpsc::channel().0
    }

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".to_string(),
            project_root: PathBuf::from("/tmp/test-bork"),
            agent_kind: crate::types::AgentKind::OpenCode,
            default_prompt: Some("Check AGENTS.md for context.".to_string()),
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
        }
    }

    fn test_app() -> App {
        let state = crate::config::AppState { issues: vec![] };
        App::new(test_config(), state)
    }

    fn test_issue(id: &str, column: Column) -> crate::types::Issue {
        crate::types::Issue {
            id: id.to_string(),
            title: format!("Test issue {}", id),
            kind: crate::types::IssueKind::Agentic,
            column,
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            prompt: None,
            worktree: None,
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
        }
    }

    // ================================================================
    // Dialog: prompt field stays empty on field navigation
    // ================================================================

    #[test]
    fn dialog_next_field_does_not_auto_fill_prompt() {
        let mut app = test_app();
        app.open_dialog();

        // Type a title (starts on Title field = 2 for Agentic, no linear)
        handle_action(
            &mut app,
            Action::DialogChar('H'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar('i'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        // Move from title (field 2) to prompt (field 3)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.focused_field, 3);
        assert_eq!(
            dialog.prompt_text(),
            "",
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
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        // Type something in the prompt
        handle_action(
            &mut app,
            Action::DialogChar('g'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar('o'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.prompt_text(), "go");
    }

    #[test]
    fn dialog_next_field_advances_through_all_fields() {
        let mut app = test_app();
        app.open_dialog();

        // Agentic, no linear: Kind(0), Mode(1), Title(2), Prompt(3)
        // Starts on Title (field 2)
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 2);

        // Tab: Title(2) -> Prompt(3)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 3);

        // Tab on last field (Prompt) wraps to Kind(0)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn dialog_prev_field_goes_back() {
        let mut app = test_app();
        app.open_dialog();

        // Starts on Title (field 2). Advance to Prompt (field 3)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 3);

        // Go back to Title (field 2)
        handle_action(
            &mut app,
            Action::DialogPrevField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 2);
    }

    #[test]
    fn dialog_prev_field_wraps_to_last_field() {
        let mut app = test_app();
        app.open_dialog();

        // Agentic, no linear: Kind(0), Mode(1), Title(2), Prompt(3)
        // Starts on Title (field 2). Two Shift+Tabs -> Kind(0)
        handle_action(
            &mut app,
            Action::DialogPrevField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        handle_action(
            &mut app,
            Action::DialogPrevField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);

        // One more Shift+Tab wraps to last field (Prompt = 3)
        handle_action(
            &mut app,
            Action::DialogPrevField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 3);
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
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        handle_action(
            &mut app,
            Action::DialogChar('a'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar(' '),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::DialogChar('b'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        assert_eq!(app.dialog.as_ref().unwrap().prompt_text(), "a b");
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
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert!(app.dialog.is_none());
    }

    #[test]
    fn open_session_on_non_agentic_opens_edit_dialog() {
        let mut app = test_app();
        app.project_mut().issues.push(crate::types::Issue {
            id: "bork-1".to_string(),
            title: "Manual task".to_string(),
            kind: crate::types::IssueKind::NonAgentic,
            column: Column::Todo,
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            prompt: Some("scratch".to_string()),
            worktree: None,
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
        });

        let post = handle_action(
            &mut app,
            Action::OpenSession,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        assert!(matches!(post, PostAction::None));
        assert_eq!(app.input_mode, InputMode::Dialog);
        let dialog = app.dialog.as_ref().expect("dialog should be open");
        assert_eq!(dialog.title, "Manual task");
        assert_eq!(dialog.focused_field, 1);
    }

    #[test]
    fn edit_dialog_does_not_inject_default_prompt() {
        let mut app = test_app();

        // Create an issue first
        app.project_mut().issues.push(crate::types::Issue {
            id: "bork-1".to_string(),
            title: "Test".to_string(),
            kind: crate::types::IssueKind::Agentic,
            column: Column::Todo,
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            prompt: None,
            worktree: Some("main".to_string()),
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
        });

        // Open edit dialog
        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0);

        // Move from title to prompt
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(
            dialog.prompt_text(),
            "",
            "edit dialog prompt should stay empty when issue had no prompt"
        );
    }

    // ================================================================
    // Linear picker: import and navigation
    // ================================================================

    fn test_linear_issue(
        id: &str,
        identifier: &str,
        title: &str,
    ) -> crate::external::linear::LinearIssue {
        crate::external::linear::LinearIssue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            title: title.to_string(),
            url: format!("https://linear.app/test/issue/{}", identifier),
            branch_name: format!("{}-slug", identifier.to_lowercase()),
            priority: 2,
            state_name: "In Progress".to_string(),
            team_key: "TEST".to_string(),
        }
    }

    #[test]
    fn linear_picker_import_creates_issue_in_todo() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live_mut().linear_issues =
            vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        app.open_linear_picker();
        assert_eq!(app.input_mode, crate::app::InputMode::LinearPicker);

        handle_action(
            &mut app,
            Action::LinearPickerSelect,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        assert_eq!(app.input_mode, crate::app::InputMode::Normal);
        assert_eq!(app.project().issues.len(), 1);
        assert_eq!(app.project().issues[0].id, "test-1");
        assert_eq!(app.project().issues[0].title, "First issue");
        assert_eq!(app.project().issues[0].column, Column::Todo);
        assert_eq!(
            app.project().issues[0].linear_id,
            Some("uuid-1".to_string())
        );
        assert_eq!(
            app.project().issues[0].linear_identifier,
            Some("TEST-1".to_string())
        );
    }

    #[test]
    fn linear_picker_import_rejects_duplicate() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live_mut().linear_issues =
            vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        // Import once
        app.open_linear_picker();
        handle_action(
            &mut app,
            Action::LinearPickerSelect,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.project().issues.len(), 1);

        // Try to import again (should show issue but reject the import)
        app.open_linear_picker();

        // The picker should still show the issue (visible but marked as imported)
        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 1);

        // Attempting to import should not create a duplicate
        handle_action(
            &mut app,
            Action::LinearPickerSelect,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn linear_picker_search_filters_issues() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live_mut().linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "Login page"),
            test_linear_issue("uuid-2", "TEST-2", "Dashboard bug"),
        ];

        app.open_linear_picker();

        // Type search
        handle_action(
            &mut app,
            Action::LinearPickerChar('l'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::LinearPickerChar('o'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        handle_action(
            &mut app,
            Action::LinearPickerChar('g'),
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].identifier, "TEST-1");
    }

    #[test]
    fn linear_picker_navigation() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live_mut().linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "First"),
            test_linear_issue("uuid-2", "TEST-2", "Second"),
            test_linear_issue("uuid-3", "TEST-3", "Third"),
        ];

        app.open_linear_picker();
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 0);

        handle_action(
            &mut app,
            Action::LinearPickerDown,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 1);

        handle_action(
            &mut app,
            Action::LinearPickerDown,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 2);

        // Should not go past the last item
        handle_action(
            &mut app,
            Action::LinearPickerDown,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 2);

        handle_action(
            &mut app,
            Action::LinearPickerUp,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn linear_picker_close_restores_normal() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live_mut().linear_issues =
            vec![test_linear_issue("uuid-1", "TEST-1", "First")];

        app.open_linear_picker();
        handle_action(
            &mut app,
            Action::LinearPickerClose,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );

        assert_eq!(app.input_mode, crate::app::InputMode::Normal);
        assert!(app.linear_picker.is_none());
        assert_eq!(app.project().issues.len(), 0);
    }

    // ================================================================
    // Dialog with Linear field: tab-through wraps
    // ================================================================

    #[test]
    fn edit_dialog_with_linear_tabs_through_and_wraps() {
        let mut app = test_app();
        app.project_mut().linear_available = true;

        app.project_mut().issues.push(crate::types::Issue {
            id: "bork-1".to_string(),
            title: "Test issue".to_string(),
            kind: crate::types::IssueKind::Agentic,
            column: Column::Todo,
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            prompt: None,
            worktree: None,
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
        });

        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0);

        // Agentic + linear: Kind(0), Mode(1), Linear(2), Title(3), Prompt(4)
        // Should start on Title (field 3)
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 3);
        assert_eq!(app.dialog.as_ref().unwrap().active_field_count(), 5);

        // Tab: Title(3) -> Prompt(4)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 4);

        // Tab on Prompt (last field) wraps to Kind(0)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn edit_dialog_with_linear_tab_wraps_without_issues_loaded() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        // No linear issues loaded
        app.project_mut().live_mut().linear_issues = vec![];

        app.project_mut().issues.push(crate::types::Issue {
            id: "bork-1".to_string(),
            title: "Test issue".to_string(),
            kind: crate::types::IssueKind::Agentic,
            column: Column::Todo,
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            prompt: None,
            worktree: None,
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
        });

        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0);

        // Agentic + linear: Kind(0), Mode(1), Linear(2), Title(3), Prompt(4)
        // Starts on Title(3). Tab to Prompt(4), then tab wraps.
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 4);

        // Tab on Prompt (last field) wraps to Kind(0)
        handle_action(
            &mut app,
            Action::DialogNextField,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn edit_dialog_with_linear_shift_enter_submits_from_any_field() {
        let mut app = test_app();
        app.project_mut().linear_available = true;

        app.project_mut().issues.push(crate::types::Issue {
            id: "bork-1".to_string(),
            title: "Test issue".to_string(),
            kind: crate::types::IssueKind::Agentic,
            column: Column::Todo,
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            prompt: None,
            worktree: None,
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
        });

        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0);

        // Starts on Title(3). Shift+Enter should submit from any field.
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 3);

        handle_action(
            &mut app,
            Action::DialogSubmit,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        );
        assert_eq!(app.input_mode, crate::app::InputMode::Normal);
        assert!(app.dialog.is_none());
    }

    // ================================================================
    // Normal mode: navigation
    // ================================================================

    fn app_with_issues() -> App {
        let mut app = test_app();
        app.project_mut()
            .issues
            .push(test_issue("bork-1", Column::Todo));
        app.project_mut()
            .issues
            .push(test_issue("bork-2", Column::Todo));
        app.project_mut()
            .issues
            .push(test_issue("bork-3", Column::InProgress));
        app
    }

    fn act(app: &mut App, action: Action) -> PostAction {
        handle_action(
            app,
            action,
            &mpsc::channel().0,
            &pr_wake_tx(),
            &linear_wake_tx(),
            &git_wake_tx(),
        )
    }

    #[test]
    fn move_down_increments_row() {
        let mut app = app_with_issues();
        assert_eq!(app.project().selected_row[0], 0);
        act(&mut app, Action::MoveDown);
        assert_eq!(app.project().selected_row[0], 1);
    }

    #[test]
    fn move_up_decrements_row() {
        let mut app = app_with_issues();
        app.project_mut().selected_row[0] = 1;
        act(&mut app, Action::MoveUp);
        assert_eq!(app.project().selected_row[0], 0);
    }

    #[test]
    fn focus_right_moves_down_then_next_column() {
        let mut app = app_with_issues();
        assert_eq!(app.project().selected_column, 0);
        assert_eq!(app.project().selected_row[0], 0);
        // 2 issues in Todo: first FocusRight moves to row 1
        act(&mut app, Action::FocusRight);
        assert_eq!(app.project().selected_column, 0);
        assert_eq!(app.project().selected_row[0], 1);
        // At bottom of Todo: next FocusRight jumps to InProgress
        act(&mut app, Action::FocusRight);
        assert_eq!(app.project().selected_column, 1);
    }

    #[test]
    fn focus_left_moves_up_then_prev_column() {
        let mut app = app_with_issues();
        app.project_mut().selected_column = 1;
        // InProgress has 1 issue at row 0, so FocusLeft jumps to Todo
        act(&mut app, Action::FocusLeft);
        assert_eq!(app.project().selected_column, 0);
    }

    #[test]
    fn jump_column_right() {
        let mut app = app_with_issues();
        assert_eq!(app.project().selected_column, 0);
        act(&mut app, Action::JumpColumnRight);
        // Should jump to InProgress (next column with issues)
        assert_eq!(app.project().selected_column, 1);
    }

    #[test]
    fn scroll_to_top() {
        let mut app = app_with_issues();
        app.project_mut().selected_row[0] = 1;
        act(&mut app, Action::ScrollToTop);
        assert_eq!(app.project().selected_row[0], 0);
    }

    #[test]
    fn scroll_to_bottom() {
        let mut app = app_with_issues();
        act(&mut app, Action::ScrollToBottom);
        assert_eq!(app.project().selected_row[0], 1); // 2 issues in Todo, last index is 1
    }

    // ================================================================
    // Normal mode: issue movement
    // ================================================================

    #[test]
    fn move_issue_right_changes_column() {
        let mut app = app_with_issues();
        assert_eq!(app.project().issues[0].column, Column::Todo);
        act(&mut app, Action::MoveIssueRight);
        assert_eq!(app.project().issues[0].column, Column::InProgress);
        assert!(app.project().state_dirty);
    }

    #[test]
    fn move_issue_left_changes_column() {
        let mut app = app_with_issues();
        app.project_mut().selected_column = 1;
        assert_eq!(app.project().issues[2].column, Column::InProgress);
        act(&mut app, Action::MoveIssueLeft);
        assert_eq!(app.project().issues[2].column, Column::Todo);
        assert!(app.project().state_dirty);
    }

    #[test]
    fn move_to_done() {
        let mut app = app_with_issues();
        act(&mut app, Action::MoveToDone);
        assert_eq!(app.project().issues[0].column, Column::Done);
        assert!(app.project().issues[0].done_at.is_some());
        assert!(app.project().state_dirty);
    }

    #[test]
    fn move_to_todo() {
        let mut app = app_with_issues();
        app.project_mut().selected_column = 1;
        act(&mut app, Action::MoveToTodo);
        assert_eq!(app.project().issues[2].column, Column::Todo);
        assert!(app.project().state_dirty);
    }

    // ================================================================
    // Normal mode: CRUD actions
    // ================================================================

    #[test]
    fn create_issue_opens_dialog() {
        let mut app = test_app();
        act(&mut app, Action::CreateIssue);
        assert_eq!(app.input_mode, InputMode::Dialog);
        assert!(app.dialog.is_some());
    }

    #[test]
    fn edit_issue_opens_dialog() {
        let mut app = app_with_issues();
        act(&mut app, Action::EditIssue);
        assert_eq!(app.input_mode, InputMode::Dialog);
        let dialog = app.dialog.as_ref().unwrap();
        assert!(dialog.editing_index.is_some());
        assert_eq!(dialog.title, "Test issue bork-1");
    }

    #[test]
    fn delete_issue_opens_confirm() {
        let mut app = app_with_issues();
        act(&mut app, Action::DeleteIssue);
        assert_eq!(app.input_mode, InputMode::Confirm);
        assert!(app.confirm_message.is_some());
    }

    #[test]
    fn delete_confirm_removes_issue() {
        let mut app = app_with_issues();
        act(&mut app, Action::DeleteIssue);
        assert_eq!(app.input_mode, InputMode::Confirm);
        act(&mut app, Action::ConfirmYes);
        assert_eq!(app.input_mode, InputMode::Normal);
        // bork-1 was deleted (no active session so synchronous)
        assert_eq!(app.project().issues.len(), 2);
        assert_eq!(app.project().issues[0].id, "bork-2");
        assert!(app.project().state_dirty);
    }

    #[test]
    fn delete_confirm_no_cancels() {
        let mut app = app_with_issues();
        act(&mut app, Action::DeleteIssue);
        act(&mut app, Action::ConfirmNo);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.project().issues.len(), 3);
    }

    // ================================================================
    // Normal mode: misc actions
    // ================================================================

    #[test]
    fn quit_sets_should_quit() {
        let mut app = test_app();
        act(&mut app, Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn search_start_enters_search_mode() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        assert_eq!(app.input_mode, InputMode::Search);
    }

    #[test]
    fn show_help_enters_help_mode() {
        let mut app = test_app();
        act(&mut app, Action::ShowHelp);
        assert_eq!(app.input_mode, InputMode::Help);
    }

    #[test]
    fn close_help_returns_to_normal() {
        let mut app = test_app();
        act(&mut app, Action::ShowHelp);
        assert_eq!(app.input_mode, InputMode::Help);
        act(&mut app, Action::CloseHelp);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn sync_prs_returns_none() {
        let mut app = test_app();
        let post = act(&mut app, Action::SyncPRs);
        assert!(matches!(post, PostAction::None));
    }

    #[test]
    fn add_issue_opens_dialog_for_current_column() {
        let mut app = app_with_issues();
        app.project_mut().selected_column = 1; // InProgress
        act(&mut app, Action::AddIssue);
        assert_eq!(app.input_mode, InputMode::Dialog);
        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.target_column, Some(Column::InProgress));
    }

    #[test]
    fn noop_does_nothing() {
        let mut app = test_app();
        let post = act(&mut app, Action::Noop);
        assert!(matches!(post, PostAction::None));
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    // ================================================================
    // Search mode dispatch
    // ================================================================

    #[test]
    fn search_char_appends() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        act(&mut app, Action::SearchChar('a'));
        act(&mut app, Action::SearchChar('b'));
        assert_eq!(app.project().search_query, "ab");
    }

    #[test]
    fn search_backspace_removes() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        act(&mut app, Action::SearchChar('a'));
        act(&mut app, Action::SearchChar('b'));
        act(&mut app, Action::SearchBackspace);
        assert_eq!(app.project().search_query, "a");
    }

    #[test]
    fn search_confirm_returns_to_normal() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        act(&mut app, Action::SearchChar('x'));
        act(&mut app, Action::SearchConfirm);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.project().search_query, "x"); // query preserved
    }

    #[test]
    fn search_cancel_clears_and_returns() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        act(&mut app, Action::SearchChar('x'));
        act(&mut app, Action::SearchCancel);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.project().search_query, ""); // query cleared
    }

    // ================================================================
    // Kill session (confirm flow, no actual tmux)
    // ================================================================

    #[test]
    fn kill_session_no_active_session_shows_message() {
        let mut app = app_with_issues();
        act(&mut app, Action::KillSession);
        // No active session, so it just sets a message
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.message.as_ref().unwrap().contains("No active session"));
    }

    #[test]
    fn kill_session_with_active_session_opens_confirm() {
        let mut app = app_with_issues();
        app.project_mut()
            .live_mut()
            .active_sessions
            .insert("bork-bork-1".to_string());
        act(&mut app, Action::KillSession);
        assert_eq!(app.input_mode, InputMode::Confirm);
    }
}
