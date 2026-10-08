use std::path::Path;
use std::sync::mpsc;
use std::thread;

use crate::app::{
    ActionContext, App, ConfirmAction, ImportSource, InputMode, LinearPickerContext, MessageKind,
    Project, ProjectId,
};
use crate::config::AppConfig;
use crate::external::{agent, browser, github, tmux, tuicr};
use crate::global_config::ReloadResult;
use crate::input::Action;
use crate::types::{
    AgentKind, Column, Issue, IssueDraft, IssueKind, LinkedGithubPr, LinkedLinear, PrImportSource,
};

pub struct ActionChannels<'a> {
    pub action_tx: &'a mpsc::Sender<ActionResult>,
    pub pr_wake_tx: &'a mpsc::Sender<crate::github_poll::Wake>,
    pub linear_wake_tx: &'a mpsc::Sender<()>,
    pub git_wake_tx: &'a mpsc::Sender<()>,
    pub reload_tx: &'a mpsc::Sender<ReloadResult>,
}

/// A detected agent session id for the issue named by `launched_issue_id`.
/// Agent and kind are captured at launch time: the id is stored under the
/// agent that minted it, and a kind change while detection was still polling
/// means the session was invalidated and must not be recorded.
pub struct LaunchedSession {
    pub agent: AgentKind,
    pub kind: IssueKind,
    pub session_id: String,
}

#[derive(Default)]
pub struct ActionResult {
    pub message: String,
    pub message_kind: MessageKind,
    pub session_to_open: Option<String>,
    pub popup_title: Option<String>,
    pub launched_session: Option<LaunchedSession>,
    /// True when this launch's command included the one-time worktree setup
    /// prefix, so the issue's `setup_ran` flag must be persisted even if
    /// session-id detection failed.
    pub launched_setup_ran: bool,
    /// Set (on success *and* failure) when this result completes a session
    /// launch, so the main loop can clear the in-flight guard for the issue.
    pub launched_issue_id: Option<String>,
    /// If set, apply this prune outcome to the issues of `project_id`.
    pub prune_outcome: Option<(crate::app::ProjectId, crate::prune::PruneOutcome)>,
    /// Remove this issue only after its asynchronous session teardown succeeds.
    pub issue_to_delete: Option<(ProjectId, String)>,
    /// Move this issue to Done and drop its worktree only after its
    /// asynchronous teardown (session kill + teardown script + worktree
    /// removal) succeeds.
    pub issue_to_archive: Option<(ProjectId, String)>,
}

pub enum PostAction {
    None,
    OpenTmuxPopup {
        session_name: String,
        popup_title: String,
    },
    /// Background session launches, one `(issue_id, popup_title)` per issue.
    /// `open_popup` is only set for a single-issue Open.
    Launch {
        launches: Vec<(String, String)>,
        open_popup: bool,
    },
    OpenEditor {
        initial_content: String,
    },
    SwitchProject {
        id: ProjectId,
    },
}

pub fn handle_action(
    app: &mut App,
    action: Action,
    ctx: &ActionContext,
    ch: &ActionChannels<'_>,
) -> PostAction {
    match app.input_mode {
        InputMode::Confirm => {
            handle_confirm(app, action, ctx, ch.action_tx);
            PostAction::None
        }
        InputMode::Dialog => handle_dialog(app, action, ctx),
        InputMode::Search => {
            handle_search(app, action, ctx);
            PostAction::None
        }
        InputMode::LinearPicker => {
            handle_linear_picker(app, action, ctx, ch.linear_wake_tx, ch.pr_wake_tx);
            PostAction::None
        }
        InputMode::StackDetails => handle_stack_details(app, action, ch),
        InputMode::LinkPicker => {
            handle_link_picker(app, action, ctx);
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
        InputMode::Normal => handle_normal(app, action, ctx, ch),
        InputMode::Sidebar => handle_sidebar(app, action),
        InputMode::PruneDialog => {
            handle_prune_dialog(app, action, ctx, ch.action_tx);
            PostAction::None
        }
    }
}

/// Summarize a batch of browser-open attempts for the status bar. `noun` names
/// what was opened ("PR", "Linear issue"); `failures` holds one "label: error"
/// line per failed link. Partial failures report how many opened plus the
/// first failure, so the user knows not to retry the ones that worked.
fn summarize_open_links(
    noun: &str,
    total: usize,
    first_label: &str,
    failures: Vec<String>,
) -> ActionResult {
    let (message, message_kind) = if failures.is_empty() {
        let message = if total == 1 {
            format!("Opened {noun} {first_label}")
        } else {
            format!("Opened {total} {noun}s")
        };
        (message, MessageKind::Info)
    } else {
        let mut detail = failures[0].clone();
        if failures.len() > 1 {
            detail.push_str(&format!(" (+{} more failed)", failures.len() - 1));
        }
        let opened = total - failures.len();
        let message = if opened == 0 {
            format!("Failed to open {noun} {detail}")
        } else {
            format!("Opened {opened} of {total} {noun}s; {detail}")
        };
        (message, MessageKind::Error)
    };
    ActionResult {
        message,
        message_kind,
        ..Default::default()
    }
}

/// Report the result of a mark action: the new marked count, or a warning when
/// there was no issue under the cursor.
fn report_mark(app: &mut App, count: Option<usize>) {
    match count {
        Some(count) => app.set_message(format!("{} issues marked", count)),
        None => app.set_warning("No issue selected"),
    }
}

/// Run a column-move on the active project and, when issues were marked, report
/// how many moved. `suffix` is appended to the bulk message (e.g. " to done").
fn bulk_move(
    app: &mut App,
    ctx: &ActionContext,
    query: &str,
    suffix: &str,
    move_fn: fn(&mut Project, &str) -> usize,
) {
    let p = app.context_project_mut(ctx);
    let was_bulk = !p.marked_issues.is_empty();
    let moved = move_fn(p, query);
    p.mark_dirty();
    if was_bulk {
        app.set_message(format!("Moved {} marked issues{}", moved, suffix));
    }
}

/// Start a session for every marked agentic issue in the context project,
/// skipping issues that aren't agentic, already run, or are already launching.
/// Clears the marks afterward.
fn start_marked_sessions(
    app: &mut App,
    ctx: &ActionContext,
    ch: &ActionChannels<'_>,
) -> PostAction {
    let project = app.context_project(ctx);
    let marked = project.marked_issue_indices();
    let startable: Vec<Issue> = marked
        .iter()
        .map(|&idx| &project.issues[idx])
        .filter(|issue| {
            issue.kind.is_agentic()
                && !project.is_session_alive(&issue.session_name(&project.config.project_name))
        })
        .cloned()
        .collect();
    let config = project.config.clone();

    let launches = spawn_launches(app, ch, startable, config);
    let skipped = marked.len() - launches.len();
    app.context_project_mut(ctx).clear_marks();

    if launches.is_empty() {
        app.set_warning("No marked issues could be started");
        return PostAction::None;
    }
    let mut message = format!("Starting {} marked sessions", launches.len());
    if skipped > 0 {
        message.push_str(&format!(" ({} skipped)", skipped));
    }
    app.set_message(message);
    PostAction::Launch {
        launches,
        open_popup: false,
    }
}

/// Launch sessions for `issues` on one background thread, one after another,
/// sending a result per issue. Running them in sequence matters: agents that
/// detect their session id by diffing a global session list would otherwise
/// pick up a sibling launch's id. Issues already launching are skipped.
/// Returns `(issue_id, popup_title)` for each launch that was started.
fn spawn_launches(
    app: &mut App,
    ch: &ActionChannels<'_>,
    issues: Vec<Issue>,
    config: AppConfig,
) -> Vec<(String, String)> {
    // Guard against double-launch: tmux::session_exists inside the
    // launch thread is check-then-act, so a second keypress before
    // the first launch completes would race it.
    let issues: Vec<Issue> = issues
        .into_iter()
        .filter(|issue| app.launches_in_flight.insert(issue.id.clone()))
        .collect();
    if issues.is_empty() {
        return Vec::new();
    }

    let launches = issues
        .iter()
        .map(|issue| {
            app.begin_busy();
            (issue.id.clone(), issue.popup_title())
        })
        .collect();

    let tx = ch.action_tx.clone();
    thread::spawn(move || {
        for issue in issues {
            let panic_issue_id = issue.id.clone();
            // A panic in the launch path must still deliver a result,
            // otherwise the in-flight guard for this issue leaks and
            // blocks every future launch attempt until restart.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                launch_and_report(issue, config.clone())
            }))
            .unwrap_or_else(|_| ActionResult {
                message: "Launch failed unexpectedly (internal panic)".to_string(),
                message_kind: MessageKind::Error,
                launched_issue_id: Some(panic_issue_id),
                ..Default::default()
            });
            let _ = tx.send(result);
        }
    });

    launches
}

fn handle_normal(
    app: &mut App,
    action: Action,
    ctx: &ActionContext,
    ch: &ActionChannels<'_>,
) -> PostAction {
    let q = app.search_query.clone();
    if matches!(action, Action::OpenPR | Action::OpenReviewPR) {
        if let Some(issue) = app.context_project(ctx).selected_issue(&q).cloned() {
            if issue.github_stack.is_some() {
                return open_stack(app, action, ctx, &issue, ch);
            }
        }
    }
    match action {
        Action::ExpandStack => {
            let live = &app.context_project(ctx).live;
            if live.gh_missing || live.stacks_unsupported {
                return PostAction::None;
            }
            if let Some(issue) = app.context_project(ctx).selected_issue(&q) {
                if issue.github_stack.is_some() {
                    app.stack_details = Some(crate::app::StackDetailsState {
                        project_id: ctx.project_id.clone(),
                        issue_id: issue.id.clone(),
                        selected: 0,
                    });
                    app.message = None;
                    app.input_mode = InputMode::StackDetails;
                } else if issue.has_pr() {
                    app.set_message("Individual PRs attached; edit GitHub links and use Ctrl+s to attach a stack");
                } else {
                    app.set_message("Attach a stack with Ctrl+s in the GitHub picker");
                }
            }
            PostAction::None
        }
        Action::Quit => {
            app.should_quit = true;
            PostAction::None
        }

        Action::MoveUp => {
            app.context_project_mut(ctx).move_selection_up();
            PostAction::None
        }
        Action::MoveDown => {
            app.context_project_mut(ctx).move_selection_down(&q);
            PostAction::None
        }
        Action::FocusLeft => {
            app.context_project_mut(ctx).focus_left(&q);
            PostAction::None
        }
        Action::FocusRight => {
            app.context_project_mut(ctx).focus_right(&q);
            PostAction::None
        }
        Action::JumpColumnLeft => {
            app.context_project_mut(ctx).jump_column_left(&q);
            PostAction::None
        }
        Action::JumpColumnRight => {
            app.context_project_mut(ctx).jump_column_right(&q);
            PostAction::None
        }

        Action::ScrollToTop => {
            app.context_project_mut(ctx).scroll_to_top();
            PostAction::None
        }
        Action::ScrollToBottom => {
            app.context_project_mut(ctx).scroll_to_bottom(&q);
            PostAction::None
        }

        Action::MoveIssueRight => {
            bulk_move(app, ctx, &q, "", Project::move_issue_right);
            PostAction::None
        }
        Action::MoveIssueLeft => {
            bulk_move(app, ctx, &q, "", Project::move_issue_left);
            PostAction::None
        }
        Action::MoveIssueUp => {
            let p = app.context_project_mut(ctx);
            p.move_issue_up(&q);
            p.mark_dirty();
            PostAction::None
        }
        Action::MoveIssueDown => {
            let p = app.context_project_mut(ctx);
            p.move_issue_down(&q);
            p.mark_dirty();
            PostAction::None
        }
        Action::MoveToDone => {
            bulk_move(app, ctx, &q, " to done", Project::move_to_done);
            PostAction::None
        }
        Action::MoveToTodo => {
            bulk_move(app, ctx, &q, " to todo", Project::move_to_todo);
            PostAction::None
        }

        Action::ToggleMark => {
            let count = app.context_project_mut(ctx).toggle_mark(&q);
            report_mark(app, count);
            PostAction::None
        }
        Action::MarkLinkedComponent => {
            let count = app.context_project_mut(ctx).mark_linked_component(&q);
            report_mark(app, count);
            PostAction::None
        }

        Action::KillSession => {
            let Some(issue) = app.context_project(ctx).selected_issue(&q) else {
                return PostAction::None;
            };

            let session_name = issue.session_name(&app.context_project(ctx).config.project_name);
            if !app.context_project(ctx).is_session_alive(&session_name) {
                app.set_warning("No active session to kill");
                return PostAction::None;
            }

            app.start_confirm(
                format!("Kill session '{}'? (y/n)", session_name),
                ConfirmAction::KillSession {
                    session_name,
                    issue_id: issue.id.clone(),
                    project_id: ctx.project_id.clone(),
                },
            );
            PostAction::None
        }

        Action::CreateIssue => {
            app.open_dialog(ctx);
            PostAction::None
        }

        Action::AddIssue => {
            let column = Column::from_index(app.context_project(ctx).selected_column)
                .unwrap_or(Column::Todo);
            app.open_dialog_in_column(column, ctx);
            PostAction::None
        }

        Action::EditIssue => {
            let Some(idx) = app.context_project(ctx).selected_issue_index(&q) else {
                return PostAction::None;
            };
            let issue = app.context_project(ctx).issues[idx].clone();
            app.open_edit_dialog(&issue, idx, ctx);
            PostAction::None
        }

        Action::DeleteIssue => {
            let Some(issue) = app.context_project(ctx).selected_issue(&q) else {
                return PostAction::None;
            };

            app.start_confirm(
                format!("Delete {}: {}? (y/n)", issue.id, issue.title),
                ConfirmAction::DeleteIssue {
                    issue_id: issue.id.clone(),
                    project_id: ctx.project_id.clone(),
                },
            );
            PostAction::None
        }

        Action::ArchiveIssue => {
            let Some(issue) = app.context_project(ctx).selected_issue(&q) else {
                return PostAction::None;
            };

            let has_worktree = issue.worktree.is_some();
            let detail = if has_worktree {
                "kill session, run teardown, remove worktree"
            } else {
                "kill session, move to Done"
            };
            app.start_confirm(
                format!("Archive {}: {}? ({}) (y/n)", issue.id, issue.title, detail),
                ConfirmAction::ArchiveIssue {
                    issue_id: issue.id.clone(),
                    project_id: ctx.project_id.clone(),
                },
            );
            PostAction::None
        }

        Action::OpenLinearPicker => {
            app.open_import_picker(ctx);
            PostAction::None
        }

        Action::OpenPruneDialog => {
            app.open_prune_dialog(ctx);
            PostAction::None
        }
        Action::ToggleLinkFilter => {
            app.toggle_link_filter(ctx);
            PostAction::None
        }
        Action::OpenLinkPicker => {
            app.open_link_picker(ctx);
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
                sidebar.selected = app
                    .projects
                    .iter()
                    .position(|p| p.id() == app.focused_project)
                    .unwrap_or(0);
                app.input_mode = InputMode::Sidebar;
            }
            let known = app.known_project_roots();
            let tx = ch.reload_tx.clone();
            thread::spawn(move || {
                let result = crate::global_config::discover_new_projects(known);
                let _ = tx.send(result);
            });
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
            let session_name = format!("{}-terminal", app.context_project(ctx).config.project_name);
            let popup_title = "Terminal".to_string();

            if app.context_project(ctx).is_session_alive(&session_name) {
                return PostAction::OpenTmuxPopup {
                    session_name,
                    popup_title,
                };
            }

            app.begin_busy();
            app.set_message("Opening terminal...");
            let tx = ch.action_tx.clone();
            let project_root = app.context_project(ctx).config.project_root.clone();

            thread::spawn(move || {
                let result = match tmux::create_session(&session_name, &project_root) {
                    Ok(()) => ActionResult {
                        message: format!("Terminal session '{}' ready", session_name),
                        session_to_open: Some(session_name),
                        popup_title: Some(popup_title),
                        ..Default::default()
                    },
                    Err(e) => ActionResult {
                        message: format!("Failed to open terminal: {e}"),
                        message_kind: MessageKind::Error,
                        ..Default::default()
                    },
                };
                let _ = tx.send(result);
            });

            PostAction::None
        }

        Action::StartSession if !app.context_project(ctx).marked_issues.is_empty() => {
            start_marked_sessions(app, ctx, ch)
        }

        Action::OpenSession | Action::StartSession => {
            let open_popup = action == Action::OpenSession;

            let Some(idx) = app.context_project(ctx).selected_issue_index(&q) else {
                return PostAction::None;
            };
            let issue = app.context_project(ctx).issues[idx].clone();

            if !issue.kind.is_agentic() {
                app.open_edit_dialog(&issue, idx, ctx);
                return PostAction::None;
            }

            let session_name = issue.session_name(&app.context_project(ctx).config.project_name);
            let popup_title = issue.popup_title();

            if app.context_project(ctx).is_session_alive(&session_name) {
                if open_popup {
                    return PostAction::OpenTmuxPopup {
                        session_name,
                        popup_title,
                    };
                }
                app.set_message("Session already running");
                return PostAction::None;
            }

            let config = app.context_project(ctx).config.clone();
            let launches = spawn_launches(app, ch, vec![issue], config);
            if launches.is_empty() {
                app.set_message("Session launch already in progress");
                return PostAction::None;
            }

            app.set_message(if open_popup {
                "Launching session..."
            } else {
                "Starting session..."
            });

            PostAction::Launch {
                launches,
                open_popup,
            }
        }

        Action::OpenReview | Action::OpenReviewPR => {
            if !app.context_project(ctx).tuicr_available {
                return PostAction::None;
            }
            let Some(issue) = app.context_project(ctx).selected_issue(&q) else {
                return PostAction::None;
            };
            let Some(wt) = issue.worktree.clone() else {
                app.set_warning("No worktree assigned");
                return PostAction::None;
            };
            let session_name = issue.session_name(&app.context_project(ctx).config.project_name);
            let session_alive = app.context_project(ctx).is_session_alive(&session_name);
            let pr_mode = action == Action::OpenReviewPR;
            let popup_title = issue.popup_title();
            let worktree_path = app.context_project(ctx).config.project_root.join(&wt);
            let tx = ch.action_tx.clone();
            app.begin_busy();
            app.set_message(if session_alive {
                if pr_mode {
                    "Opening tuicr --pr..."
                } else {
                    "Opening tuicr..."
                }
            } else {
                "Starting tuicr session..."
            });

            thread::spawn(move || {
                let outcome = if session_alive {
                    tuicr::open_in_session(&session_name, &worktree_path, pr_mode)
                } else {
                    tuicr::launch_review_session(&session_name, &worktree_path, pr_mode)
                };
                let result = match outcome {
                    Ok(()) => ActionResult {
                        message: "tuicr ready".to_string(),
                        session_to_open: Some(session_name),
                        popup_title: Some(popup_title),
                        ..Default::default()
                    },
                    Err(e) => ActionResult {
                        message: format!("Failed to open tuicr: {e}"),
                        message_kind: MessageKind::Error,
                        ..Default::default()
                    },
                };
                let _ = tx.send(result);
            });

            PostAction::None
        }

        Action::SyncPRs => {
            let _ = ch.pr_wake_tx.send(crate::github_poll::Wake::Refresh);
            app.set_message("Syncing PRs...");
            PostAction::None
        }

        Action::OpenPR => {
            let Some(issue) = app.context_project(ctx).selected_issue(&q) else {
                return PostAction::None;
            };
            let pr_numbers: Vec<u32> = if issue.github_pr_links.is_empty() {
                let Some(pr) = app.context_project(ctx).pr_for(issue) else {
                    app.set_warning("No PR found for this issue");
                    return PostAction::None;
                };
                vec![pr.number]
            } else {
                issue.pr_numbers()
            };
            let main_worktree = app.context_project(ctx).config.project_root.join("main");

            app.begin_busy();
            app.set_message("Opening PR...");
            let tx = ch.action_tx.clone();

            thread::spawn(move || {
                // pr_url resolves the repo identity via gh (cached after the
                // first call), so it must run off the main thread.
                let mut failures = Vec::new();
                for num in &pr_numbers {
                    let outcome = github::pr_url(&main_worktree, *num)
                        .ok_or_else(|| {
                            "could not determine GitHub repo (is gh installed and authenticated?)"
                                .to_string()
                        })
                        .and_then(|url| browser::open_url(&url));
                    if let Err(e) = outcome {
                        failures.push(format!("#{num}: {e}"));
                    }
                }
                let first_label = format!("#{}", pr_numbers[0]);
                let _ = tx.send(summarize_open_links(
                    "PR",
                    pr_numbers.len(),
                    &first_label,
                    failures,
                ));
            });
            PostAction::None
        }

        Action::OpenLinear => {
            let Some(issue) = app.context_project(ctx).selected_issue(&q) else {
                return PostAction::None;
            };
            if issue.linear_links.is_empty() {
                app.set_warning("No Linear issue linked");
                return PostAction::None;
            }
            let links: Vec<(String, String)> = issue
                .linear_links
                .iter()
                .map(|link| (link.identifier.clone(), link.url.clone()))
                .collect();

            app.begin_busy();
            app.set_message("Opening Linear...");
            let tx = ch.action_tx.clone();

            thread::spawn(move || {
                let mut failures = Vec::new();
                for (identifier, url) in &links {
                    if let Err(e) = browser::open_url(url) {
                        failures.push(format!("{identifier}: {e}"));
                    }
                }
                let _ = tx.send(summarize_open_links(
                    "Linear issue",
                    links.len(),
                    &links[0].0,
                    failures,
                ));
            });
            PostAction::None
        }

        Action::AssignWorktree => {
            let Some(idx) = app.context_project(ctx).selected_issue_index(&q) else {
                return PostAction::None;
            };
            if let Some(old) = app.context_project_mut(ctx).issues[idx].worktree.take() {
                app.set_message(format!("Cleared worktree '{old}', re-detecting..."));
            } else {
                app.set_message("No worktree assigned, re-detecting...");
            }
            if app.context_project_mut(ctx).auto_assign_worktrees() {
                if let Some(wt) = app.context_project(ctx).issues[idx].worktree.as_ref() {
                    app.set_message(format!("Assigned worktree: {wt}"));
                }
            }
            let _ = ch.git_wake_tx.send(());
            app.context_project_mut(ctx).mark_dirty();
            PostAction::None
        }

        Action::SearchStart => {
            app.start_search();
            PostAction::None
        }

        Action::ClearSearch => {
            app.clear_search(ctx);
            PostAction::None
        }

        Action::DebugReset => {
            if !app.context_project(ctx).config.debug {
                return PostAction::None;
            }
            app.should_quit = true;
            PostAction::None
        }

        Action::DebugInspect => {
            if !app.context_project(ctx).config.debug {
                return PostAction::None;
            }
            let Some(issue) = app.context_project(ctx).selected_issue(&q).cloned() else {
                app.set_warning("No issue selected");
                return PostAction::None;
            };
            let json = serde_json::to_string_pretty(&issue).unwrap_or_else(|e| format!("{e}"));
            app.open_debug_inspector(json);
            PostAction::None
        }

        _ => PostAction::None,
    }
}

fn handle_search(app: &mut App, action: Action, ctx: &ActionContext) {
    match action {
        Action::SearchChar(c) => app.search_push_char(c, ctx),
        Action::SearchBackspace => app.search_delete_char(ctx),
        Action::SearchConfirm => app.confirm_search(),
        Action::SearchCancel => app.cancel_search(ctx),
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

fn handle_dialog(app: &mut App, action: Action, ctx: &ActionContext) -> PostAction {
    let on_linear = app.dialog.as_ref().is_some_and(|d| d.is_on_linear_field());
    let on_github = app.dialog.as_ref().is_some_and(|d| d.is_on_github_field());

    if on_linear {
        match action {
            Action::DialogChar(' ') => {
                app.picker_tab = ImportSource::Linear;
                app.open_linear_picker_with_context(LinearPickerContext::Attach, ctx);
                return PostAction::None;
            }
            Action::DialogBackspace | Action::DialogDelete => {
                if let Some(dialog) = app.dialog.as_mut() {
                    if dialog.linear_issues.is_empty() {
                        dialog.linear_detached = true;
                    } else {
                        dialog.linear_issues.pop();
                        if dialog.linear_issues.is_empty() {
                            dialog.linear_detached = true;
                        }
                    }
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
                app.open_import_picker_with_context(LinearPickerContext::Attach, ctx);
                return PostAction::None;
            }
            Action::DialogBackspace | Action::DialogDelete => {
                if let Some(dialog) = app.dialog.as_mut() {
                    if dialog.github_stack.take().is_some() {
                        return PostAction::None;
                    }
                    if dialog.github_prs.is_empty() {
                        dialog.github_pr_cleared = true;
                    } else {
                        dialog.github_prs.pop();
                        if dialog.github_prs.is_empty() {
                            dialog.github_pr_cleared = true;
                        }
                    }
                }
                return PostAction::None;
            }
            Action::DialogChar(_) => return PostAction::None,
            _ => {}
        }
    }

    match action {
        Action::DialogSubmit => {
            submit_dialog(app, ctx);
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

fn submit_dialog(app: &mut App, ctx: &ActionContext) {
    let dialog = match app.dialog.take() {
        Some(d) => d,
        None => return,
    };

    app.input_mode = InputMode::Normal;

    let title = dialog.title.trim().to_string();
    if title.is_empty() {
        app.set_warning("Title cannot be empty");
        return;
    }

    let prompt_text = dialog.prompt_text();
    let prompt = if prompt_text.trim().is_empty() {
        None
    } else {
        Some(prompt_text)
    };

    let proj_id = ctx.project_id.clone();

    if dialog.editing_index.is_some() {
        // Only apply the dialog's agent when the user actually moved the
        // picker. An untouched picker's value can be wrong two ways: a
        // normalized fallback for an unavailable stored agent, or stale
        // against a concurrent CLI agent change — silently writing it back
        // would flip the issue while the live session keeps running the real
        // agent's process.
        let picker_moved = dialog.agent_kind != dialog.initial_agent_kind;

        let Some(p) = app.find_project(&proj_id) else {
            app.set_warning("Project no longer available");
            return;
        };
        // Resolve by ID, not the index captured at dialog-open: background
        // merges can reorder or remove issues while the dialog is up, and a
        // stale index would edit (and kill the session of) the wrong issue.
        let idx = dialog.editing_issue_id.as_deref().and_then(|id| {
            let lower = id.to_lowercase();
            p.issues.iter().position(|i| i.id.to_lowercase() == lower)
        });

        // An in-flight launch is still detecting its session id; killing the
        // session out from under it wastes the launch and leaves its result
        // describing a dead pane. Block the destructive edits until the
        // launch settles.
        if let Some(idx) = idx {
            let issue = &p.issues[idx];
            let switches_agent = picker_moved && dialog.agent_kind != issue.agent_kind;
            let crosses_boundary =
                (issue.kind == IssueKind::Orchestrator) != (dialog.kind == IssueKind::Orchestrator);
            if (switches_agent || crosses_boundary) && app.launches_in_flight.contains(&issue.id) {
                app.set_warning("Launch in progress; wait for it before switching agent or kind");
                return;
            }
        }

        let Some(p) = app.find_project_mut(&proj_id) else {
            app.set_warning("Project no longer available");
            return;
        };
        if let Some(idx) = idx {
            let session_name = p.issues[idx].session_name(&p.config.project_name);
            let detached_worktree = p.issues[idx].worktree.clone();
            let crossed_orchestrator_boundary =
                p.issues[idx].kind_change_resets_session(dialog.kind);
            let agent_changed = picker_moved && dialog.agent_kind != p.issues[idx].agent_kind;

            if crossed_orchestrator_boundary || agent_changed {
                // A live session would otherwise be re-attached with the old
                // kind's prompt and cwd, or the old agent's process. Kill it
                // before committing the edit so a failed cleanup can't leave
                // an untracked old agent running, and drop the cached
                // liveness so relaunch works before the next 2s tmux poll.
                if let Err(e) = agent::terminate_session(&p.config.project_root, &session_name) {
                    app.set_error(format!("Failed to reset session: {e}"));
                    return;
                }
                p.live.active_sessions.remove(&session_name);
            }

            p.issues[idx].title = title;
            p.issues[idx].prompt = prompt;
            if picker_moved {
                let _ = p.issues[idx].set_agent_kind(dialog.agent_kind);
            }
            p.issues[idx].agent_mode = dialog.agent_mode;
            let _ = p.issues[idx].set_kind(dialog.kind);

            apply_linear_fields(&mut p.issues[idx], &dialog);
            apply_pr_fields(&mut p.issues[idx], &dialog);

            let updated_id = p.issues[idx].id.clone();
            p.mark_dirty();

            if crossed_orchestrator_boundary {
                match detached_worktree.filter(|_| dialog.kind == IssueKind::Orchestrator) {
                    Some(wt) => app.set_message(format!(
                        "Updated {} (session reset; worktree {} detached, remove it manually)",
                        updated_id, wt
                    )),
                    None => app.set_message(format!("Updated {} (session reset)", updated_id)),
                }
            } else if agent_changed {
                app.set_message(format!(
                    "Updated {} (switched to {}, other sessions kept)",
                    updated_id, dialog.agent_kind
                ));
            } else {
                app.set_message(format!("Updated {}", updated_id));
            }
        } else {
            app.set_warning("Issue no longer exists");
        }
        return;
    }

    let Some(p) = app.find_project(&proj_id) else {
        app.set_warning("Project no longer available");
        return;
    };
    let id = p.next_issue_id();
    let column = dialog.target_column.unwrap_or(Column::Todo);
    let column_index = column.index();
    // Share the CLI's create invariant so a Done-column creation stamps
    // `done_at` here too, instead of relying on the `Project::new` backfill.
    let mut issue = Issue::build(
        IssueDraft {
            id: id.clone(),
            title,
            column,
            agent_kind: dialog.agent_kind,
            kind: dialog.kind,
            agent_mode: dialog.agent_mode,
            prompt,
        },
        crate::app::unix_now(),
    );

    apply_linear_fields(&mut issue, &dialog);
    apply_pr_fields(&mut issue, &dialog);

    let Some(p) = app.find_project_mut(&proj_id) else {
        app.set_warning("Project no longer available");
        return;
    };
    p.issues.push(issue);
    p.selected_column = column_index;
    let count = p.issues_in_column(column, "").len();
    if count > 0 {
        p.selected_row[column_index] = count - 1;
    }
    p.mark_dirty();

    app.set_message(format!("Created {}", id));
}

fn apply_linear_fields(issue: &mut Issue, dialog: &crate::app::DialogState) {
    if dialog.linear_detached {
        issue.linear_links.clear();
    } else if !dialog.linear_issues.is_empty() {
        issue.linear_links = dialog
            .linear_issues
            .iter()
            .map(|li| LinkedLinear {
                id: li.id.clone(),
                identifier: li.identifier.clone(),
                url: li.url.clone(),
                imported: false,
            })
            .collect();
    }
}

fn apply_pr_fields(issue: &mut Issue, dialog: &crate::app::DialogState) {
    issue.github_stack = if dialog.kind == IssueKind::Orchestrator {
        None
    } else {
        dialog.github_stack
    };
    if issue
        .stack_review_import
        .as_ref()
        .is_some_and(|import| issue.github_stack != Some(import.stack_number))
    {
        issue.stack_review_import = None;
    }
    // Orchestrators have no PR field; drop any links left from a kind change.
    if dialog.kind == IssueKind::Orchestrator || dialog.github_pr_cleared {
        issue.github_pr_links.clear();
    } else if !dialog.github_prs.is_empty() {
        issue.github_pr_links = dialog.github_prs.clone();
    }
}

fn handle_linear_picker(
    app: &mut App,
    action: Action,
    ctx: &ActionContext,
    linear_wake_tx: &mpsc::Sender<()>,
    pr_wake_tx: &mpsc::Sender<crate::github_poll::Wake>,
) {
    match action {
        Action::LinearPickerClose => {
            app.close_linear_picker();
        }
        Action::PickerSwitchTab => {
            let has_linear = !app.context_project(ctx).live.linear_issues.is_empty();
            let has_github = app.context_project(ctx).can_browse_github();
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
        Action::AttachStack => attach_selected_stack(app, ctx),
        Action::LinearPickerSelect => match (app.linear_picker_context, app.picker_tab) {
            (LinearPickerContext::Attach, ImportSource::Linear) => {
                attach_linear_to_dialog(app, ctx)
            }
            (LinearPickerContext::Attach, ImportSource::GitHub) => {
                attach_github_to_dialog(app, ctx)
            }
            (_, ImportSource::Linear) => import_linear_issue(app, ctx),
            (_, ImportSource::GitHub) => import_github_pr(app, ctx),
        },
        Action::LinearPickerRefresh => match app.picker_tab {
            ImportSource::Linear => {
                let _ = linear_wake_tx.send(());
                app.set_message("Refreshing Linear issues...");
            }
            ImportSource::GitHub => {
                let _ = pr_wake_tx.send(crate::github_poll::Wake::Refresh);
                app.set_message("Refreshing GitHub PRs...");
            }
        },
        _ => {}
    }
}

fn handle_link_picker(app: &mut App, action: Action, ctx: &ActionContext) {
    match action {
        Action::LinkPickerClose => app.close_link_picker(),
        Action::LinkPickerDown => app.link_picker_move_down(),
        Action::LinkPickerUp => app.link_picker_move_up(),
        Action::LinkPickerChar(c) => app.link_picker_push_char(c),
        Action::LinkPickerBackspace => app.link_picker_delete_char(),
        Action::LinkPickerSelect => toggle_selected_link(app, ctx),
        _ => {}
    }
}

/// Toggle a symmetric link between the picker anchor and the highlighted
/// candidate, mutating both issues in memory. The dirty flag flushes to disk.
fn toggle_selected_link(app: &mut App, ctx: &ActionContext) {
    let Some(picker) = &app.link_picker else {
        return;
    };
    let candidates = app.link_picker_candidates();
    let Some((candidate_id, _, _)) = candidates.get(picker.selected) else {
        return;
    };
    let anchor_id = picker.anchor_id.clone();
    let candidate_id = candidate_id.clone();

    let project = app.context_project_mut(ctx);
    let already_linked = project
        .issues
        .iter()
        .find(|i| i.id.eq_ignore_ascii_case(&anchor_id))
        .is_some_and(|a| a.is_linked_to(&candidate_id));

    for issue in &mut project.issues {
        let other = if issue.id.eq_ignore_ascii_case(&anchor_id) {
            &candidate_id
        } else if issue.id.eq_ignore_ascii_case(&candidate_id) {
            &anchor_id
        } else {
            continue;
        };
        issue
            .linked_issues
            .retain(|l| !l.eq_ignore_ascii_case(other));
        if !already_linked {
            issue.linked_issues.push(other.clone());
        }
    }
    project.mark_dirty();
}

fn attach_linear_to_dialog(app: &mut App, _ctx: &ActionContext) {
    let filtered = app.filtered_linear_issues();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let linear_issue = match filtered.get(selected_idx) {
        Some(i) => (*i).clone(),
        None => return,
    };

    if let Some(ref mut dialog) = app.dialog {
        if let Some(pos) = dialog
            .linear_issues
            .iter()
            .position(|l| l.id == linear_issue.id)
        {
            dialog.linear_issues.remove(pos);
        } else {
            dialog.linear_issues.push(linear_issue);
            dialog.linear_detached = false;
        }
    }
}

fn import_linear_issue(app: &mut App, ctx: &ActionContext) {
    let filtered = app.filtered_linear_issues();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let linear_issue = match filtered.get(selected_idx) {
        Some(i) => (*i).clone(),
        None => return,
    };

    let id = linear_issue.identifier.to_lowercase();

    let proj_id = ctx.project_id.clone();
    let Some(project) = app.find_project(&proj_id) else {
        app.set_warning("Project no longer available");
        app.close_linear_picker();
        return;
    };

    if project.issues.iter().any(|i| i.id == id) {
        app.set_message(format!(
            "{} is already on the board",
            linear_issue.identifier
        ));
        app.close_linear_picker();
        return;
    }

    let issue = Issue {
        linear_links: vec![LinkedLinear {
            id: linear_issue.id.clone(),
            identifier: linear_issue.identifier.clone(),
            url: linear_issue.url.clone(),
            imported: true,
        }],
        ..Issue::new(
            id,
            linear_issue.title.clone(),
            Column::Todo,
            project.config.agent_kind,
        )
    };

    let Some(p) = app.find_project_mut(&proj_id) else {
        return;
    };
    p.issues.push(issue);
    let count = p.issues_in_column(Column::Todo, "").len();
    if count > 0 {
        p.selected_column = 0;
        p.selected_row[0] = count - 1;
    }
    p.mark_dirty();

    app.set_message(format!("Imported {}", linear_issue.identifier));
    app.close_linear_picker();
}

fn import_github_pr(app: &mut App, ctx: &ActionContext) {
    let filtered = app.filtered_github_prs();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let pr = match filtered.get(selected_idx) {
        Some(entry) => match entry.status {
            Some(pr) => pr.clone(),
            None => {
                app.set_warning("PR status unavailable; Ctrl+r to refresh");
                return;
            }
        },
        None => return,
    };

    let proj_id = ctx.project_id.clone();
    let Some(project) = app.find_project(&proj_id) else {
        app.set_warning("Project no longer available");
        app.close_linear_picker();
        return;
    };

    if project.issues.iter().any(|i| i.has_pr_number(pr.number)) {
        app.set_warning(format!("PR #{} is already on the board", pr.number));
        app.close_linear_picker();
        return;
    }

    let issue = Issue {
        github_pr_links: vec![LinkedGithubPr {
            number: pr.number,
            imported: true,
            import_source: Some(PrImportSource::Authored),
        }],
        ..Issue::new(
            project.next_issue_id(),
            pr.title.clone(),
            Column::CodeReview,
            project.config.agent_kind,
        )
    };

    let Some(p) = app.find_project_mut(&proj_id) else {
        return;
    };
    p.issues.push(issue);
    let count = p.issues_in_column(Column::CodeReview, "").len();
    if count > 0 {
        p.selected_column = Column::CodeReview.index();
        p.selected_row[Column::CodeReview.index()] = count - 1;
    }
    p.mark_dirty();

    app.set_message(format!("Imported PR #{}", pr.number));
    app.close_linear_picker();
}

fn handle_stack_details(app: &mut App, action: Action, ch: &ActionChannels<'_>) -> PostAction {
    let Some(details) = &app.stack_details else {
        return PostAction::None;
    };
    let ctx = ActionContext {
        project_id: details.project_id.clone(),
    };
    let issue = app
        .find_project(&ctx.project_id)
        .and_then(|project| {
            project
                .issues
                .iter()
                .find(|issue| issue.id == details.issue_id)
        })
        .cloned();
    if action == Action::CloseStack
        || issue
            .as_ref()
            .is_none_or(|issue| issue.github_stack.is_none())
    {
        app.stack_details = None;
        app.input_mode = InputMode::Normal;
        return PostAction::None;
    }
    let Some(issue) = issue else {
        return PostAction::None;
    };
    match action {
        Action::StackDown | Action::StackUp => {
            let count = app
                .context_project(&ctx)
                .attached_stack(&issue)
                .map(|stack| stack.pull_requests.len())
                .unwrap_or(0);
            if let Some(details) = &mut app.stack_details {
                if action == Action::StackDown {
                    details.selected = (details.selected + 1).min(count.saturating_sub(1));
                } else {
                    details.selected = details.selected.saturating_sub(1);
                }
            }
        }
        Action::SyncPRs => {
            let _ = ch.pr_wake_tx.send(crate::github_poll::Wake::Refresh);
        }
        Action::OpenPR | Action::OpenReviewPR => return open_stack(app, action, &ctx, &issue, ch),
        _ => {}
    }
    PostAction::None
}

fn open_stack(
    app: &mut App,
    action: Action,
    ctx: &ActionContext,
    issue: &Issue,
    ch: &ActionChannels<'_>,
) -> PostAction {
    let project = app.context_project(ctx);
    let numbers = match project.stack_open_numbers(issue) {
        Ok(numbers) => numbers,
        Err(message) => {
            app.set_warning(message);
            return PostAction::None;
        }
    };
    let review = action == Action::OpenReviewPR;
    if review && !project.tuicr_available {
        app.set_warning("tuicr is not installed");
        return PostAction::None;
    }
    let session = issue.session_name(&project.config.project_name);
    let alive = project.is_session_alive(&session);
    let cwd = project.config.project_root.join("main");
    let popup_title = issue.popup_title();
    let tx = ch.action_tx.clone();
    app.begin_busy();
    app.set_message(if review {
        "Opening stack review..."
    } else {
        "Opening stack PRs..."
    });
    thread::spawn(move || {
        let result = if review {
            match tuicr::open_stack(&session, &cwd, &numbers, alive) {
                Ok(()) => ActionResult {
                    message: format!("Reviewing {} PRs in stack order", numbers.len()),
                    session_to_open: Some(session),
                    popup_title: Some(popup_title),
                    ..Default::default()
                },
                Err(error) => ActionResult {
                    message: format!("Failed to open stack review: {error}"),
                    message_kind: MessageKind::Error,
                    ..Default::default()
                },
            }
        } else {
            let mut failures = Vec::new();
            for number in &numbers {
                let result = github::pr_url(&cwd, *number)
                    .ok_or_else(|| "Could not resolve GitHub repository".to_string())
                    .and_then(|url| browser::open_url(&url));
                if let Err(error) = result {
                    failures.push(format!("#{number}: {error}"));
                }
            }
            summarize_open_links("PR", numbers.len(), &format!("#{}", numbers[0]), failures)
        };
        let _ = tx.send(result);
    });
    PostAction::None
}

fn attach_selected_stack(app: &mut App, ctx: &ActionContext) {
    if app.picker_tab != ImportSource::GitHub {
        return;
    }
    let selected = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);
    let prs = app.filtered_github_prs();
    let Some(pr) = prs.get(selected) else { return };
    let project = app.context_project(ctx);
    if project.live.gh_missing || project.live.stacks_unsupported {
        return;
    }
    if !project.live.stacks_available {
        let message = format!(
            "Stack data {}; Ctrl+r to refresh",
            project.live.missing_github_status()
        );
        app.set_warning(&message);
        return;
    }
    let Some(stack) = project.stack_for_pr(pr.number) else {
        app.set_warning("This PR does not belong to a known stack");
        return;
    };
    let number = stack.number;
    let selected_number = pr.number;
    if app.linear_picker_context == LinearPickerContext::Attach {
        if let Some(dialog) = &mut app.dialog {
            if dialog.github_stack == Some(number) {
                dialog.github_stack = None;
            } else {
                dialog.github_stack = Some(number);
            }
        }
        app.focus_github_picker_pr(selected_number);
        return;
    }
    if project
        .issues
        .iter()
        .any(|issue| issue.github_stack == Some(number))
    {
        app.set_warning(format!("Stack #{number} is already on the board"));
        return;
    }
    let issue = Issue {
        github_stack: Some(number),
        ..Issue::new(
            project.next_issue_id(),
            pr.title().to_string(),
            Column::CodeReview,
            project.config.agent_kind,
        )
    };
    let project = app.context_project_mut(ctx);
    project.issues.push(issue);
    project.selected_column = Column::CodeReview.index();
    project.selected_row[Column::CodeReview.index()] = project
        .issues_in_column(Column::CodeReview, "")
        .len()
        .saturating_sub(1);
    project.mark_dirty();
    app.close_linear_picker();
    app.set_message(format!("Imported stack #{number}"));
}

fn attach_github_to_dialog(app: &mut App, _ctx: &ActionContext) {
    let filtered = app.filtered_github_prs();
    let selected_idx = app.linear_picker.as_ref().map(|p| p.selected).unwrap_or(0);

    let number = match filtered.get(selected_idx) {
        Some(pr) => pr.number,
        None => return,
    };

    if let Some(ref mut dialog) = app.dialog {
        if let Some(pos) = dialog.github_prs.iter().position(|p| p.number == number) {
            dialog.github_prs.remove(pos);
        } else {
            dialog.github_prs.push(LinkedGithubPr {
                number,
                imported: false,
                import_source: None,
            });
            dialog.github_pr_cleared = false;
        }
    }
    app.focus_github_picker_pr(number);
}

fn handle_prune_dialog(
    app: &mut App,
    action: Action,
    _ctx: &ActionContext,
    action_tx: &mpsc::Sender<ActionResult>,
) {
    match action {
        Action::PruneCancel => app.close_prune_dialog(),
        Action::PruneConfirm => submit_prune_dialog(app, action_tx),
        _ => {
            let Some(dialog) = app.prune_dialog.as_mut() else {
                return;
            };
            match action {
                Action::PruneMoveUp => dialog.move_up(),
                Action::PruneMoveDown => dialog.move_down(),
                Action::PruneToggle => dialog.toggle_current(),
                Action::PruneSelectAllRemove => dialog.select_all_remove(),
                Action::PruneSelectAllKeep => dialog.select_all_keep(),
                _ => {}
            }
        }
    }
}

fn submit_prune_dialog(app: &mut App, action_tx: &mpsc::Sender<ActionResult>) {
    let Some(dialog) = app.prune_dialog.as_mut() else {
        return;
    };

    let (to_remove, dirty) = crate::prune::partition_selection(&dialog.candidates);
    if !dirty.is_empty() {
        dialog.error = Some(format!(
            "Refusing: dirty worktrees can't be pruned ({}). Commit/stash first.",
            dirty.join(", ")
        ));
        return;
    }

    if to_remove.is_empty() {
        app.set_message("Nothing to prune");
        app.close_prune_dialog();
        return;
    }

    let project_id = dialog.project_id.clone();
    let Some(project) = app.find_project(&project_id) else {
        app.close_prune_dialog();
        return;
    };
    let project_root = project.config.project_root.clone();

    app.close_prune_dialog();
    app.begin_busy();
    app.set_message(format!("Pruning {} worktree(s)...", to_remove.len()));

    let tx = action_tx.clone();
    thread::spawn(move || {
        let outcome = crate::prune::execute_removals(&project_root, &to_remove);
        let (message, message_kind) = format_prune_result(&outcome);
        let _ = tx.send(ActionResult {
            message,
            message_kind,
            prune_outcome: Some((project_id, outcome)),
            ..Default::default()
        });
    });
}

fn format_prune_result(outcome: &crate::prune::PruneOutcome) -> (String, MessageKind) {
    let removed = outcome.removed_count();
    let failed = outcome.failed_count();
    if failed == 0 {
        return (format!("Pruned {} worktree(s)", removed), MessageKind::Info);
    }
    let failure_summary: Vec<String> = outcome
        .results
        .iter()
        .filter_map(|r| match &r.outcome {
            crate::prune::RemoveOutcome::Failed(msg) => Some(format!("{}: {}", r.worktree, msg)),
            _ => None,
        })
        .collect();
    (
        format!(
            "Pruned {} worktree(s), {} failed ({})",
            removed,
            failed,
            failure_summary.join("; ")
        ),
        MessageKind::Warning,
    )
}

fn handle_sidebar(app: &mut App, action: Action) -> PostAction {
    match action {
        Action::ToggleSidebar => {
            if let Some(ref mut sidebar) = app.sidebar {
                sidebar.focused = false;
                sidebar.visible = false;
                app.input_mode = InputMode::Normal;
            }
            PostAction::None
        }
        Action::SidebarSelect => {
            if let Some(ref mut sidebar) = app.sidebar {
                let proj_id = app.projects[sidebar.selected].id();
                sidebar.swimlanes = vec![proj_id.clone()];
                sidebar.focused = false;
                sidebar.visible = false;
                app.input_mode = InputMode::Normal;
                app.focused_swimlane = 0;

                if proj_id != app.focused_project {
                    return PostAction::SwitchProject { id: proj_id };
                }
            }
            PostAction::None
        }
        Action::SidebarDown => {
            if let Some(ref mut sidebar) = app.sidebar {
                if !app.projects.is_empty() {
                    sidebar.selected = (sidebar.selected + 1) % app.projects.len();
                }
            }
            PostAction::None
        }
        Action::SidebarUp => {
            if let Some(ref mut sidebar) = app.sidebar {
                if !app.projects.is_empty() {
                    if sidebar.selected == 0 {
                        sidebar.selected = app.projects.len() - 1;
                    } else {
                        sidebar.selected -= 1;
                    }
                }
            }
            PostAction::None
        }
        Action::SidebarToggleSwimlane => {
            let lane_count = app.visible_swimlane_count();
            if let Some(ref mut sidebar) = app.sidebar {
                let idx = sidebar.selected;
                let proj_id = app.projects[idx].id();
                if let Some(pos) = sidebar.swimlanes.iter().position(|id| *id == proj_id) {
                    if sidebar.swimlanes.len() > 1 {
                        sidebar.swimlanes.remove(pos);
                        if proj_id == app.focused_project {
                            app.focused_project = sidebar.swimlanes[0].clone();
                        }
                        if app.focused_swimlane == pos {
                            app.focused_swimlane = pos.min(sidebar.swimlanes.len() - 1);
                        } else if app.focused_swimlane > pos {
                            app.focused_swimlane -= 1;
                        }
                    }
                } else if lane_count < 3 {
                    sidebar.swimlanes.push(proj_id);
                } else {
                    app.set_warning("Maximum 3 projects visible");
                }
            }
            PostAction::None
        }
        Action::ShowHelp => {
            app.open_help();
            PostAction::None
        }
        Action::Quit => {
            app.should_quit = true;
            PostAction::None
        }
        _ => PostAction::None,
    }
}

fn handle_confirm(
    app: &mut App,
    action: Action,
    _ctx: &ActionContext,
    action_tx: &mpsc::Sender<ActionResult>,
) {
    match action {
        Action::ConfirmYes => {
            if let Some(confirm_action) = app.take_confirm_action() {
                match confirm_action {
                    ConfirmAction::KillSession {
                        session_name,
                        issue_id,
                        project_id,
                    } => {
                        let Some(project) = app.find_project(&project_id) else {
                            app.set_warning("Project no longer available");
                            return;
                        };
                        let project_root = project.config.project_root.clone();
                        app.invalidate_inflight_launch(&issue_id);
                        app.begin_busy();
                        let tx = action_tx.clone();

                        thread::spawn(move || {
                            let _ = tx.send(terminate_to_result(
                                &project_root,
                                &session_name,
                                format!("Session '{}' killed", session_name),
                                format!("Session '{}' was already stopped", session_name),
                            ));
                        });
                    }
                    ConfirmAction::DeleteIssue {
                        issue_id,
                        project_id,
                    } => {
                        let Some(p) = app.find_project(&project_id) else {
                            app.set_warning("Project no longer available");
                            return;
                        };
                        // Resolve by ID at confirm time: indices can shift while
                        // the prompt is open (PR sync, external state merges).
                        let Some(issue_index) = p.issues.iter().position(|i| i.id == issue_id)
                        else {
                            app.set_warning(format!("{} is no longer on the board", issue_id));
                            return;
                        };
                        let issue = &p.issues[issue_index];
                        let session_name = issue.session_name(&p.config.project_name);
                        let id = issue.id.clone();
                        let project_root = p.config.project_root.clone();

                        app.invalidate_inflight_launch(&issue_id);
                        app.begin_busy();
                        let tx = action_tx.clone();
                        thread::spawn(move || {
                            let mut result = terminate_to_result(
                                &project_root,
                                &session_name,
                                format!("Deleted {} and killed session", id),
                                format!("Deleted {}", id),
                            );
                            if result.message_kind != MessageKind::Error {
                                result.issue_to_delete = Some((project_id, id));
                            }
                            let _ = tx.send(result);
                        });
                    }
                    ConfirmAction::ArchiveIssue {
                        issue_id,
                        project_id,
                    } => {
                        let Some(p) = app.find_project(&project_id) else {
                            app.set_warning("Project no longer available");
                            return;
                        };
                        // Resolve by ID at confirm time: indices can shift while
                        // the prompt is open (PR sync, external state merges).
                        let Some(idx) = p.issues.iter().position(|i| i.id == issue_id) else {
                            app.set_warning(format!("{} is no longer on the board", issue_id));
                            return;
                        };
                        let issue = &p.issues[idx];
                        let id = issue.id.clone();
                        let session_name = issue.session_name(&p.config.project_name);
                        let worktree = issue.worktree.clone();
                        let config = p.config.clone();

                        app.invalidate_inflight_launch(&issue_id);
                        app.begin_busy();
                        let tx = action_tx.clone();
                        thread::spawn(move || {
                            let _ = tx.send(archive_to_result(
                                &config,
                                project_id,
                                id,
                                &session_name,
                                worktree.as_deref(),
                            ));
                        });
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

/// Run the off-thread half of a TUI archive: kill the agent session, run the
/// teardown script, and remove the worktree (never forced, matching the CLI's
/// default — a failing teardown or dirty worktree aborts and reports an
/// error). On success it sets `issue_to_archive` so the main thread applies
/// the move-to-Done and worktree drop against the live in-memory board,
/// instead of writing state.json directly and racing concurrent merges.
fn archive_to_result(
    config: &AppConfig,
    project_id: ProjectId,
    issue_id: String,
    session_name: &str,
    worktree: Option<&str>,
) -> ActionResult {
    let session_killed = match agent::terminate_session(&config.project_root, session_name) {
        Ok(killed) => killed,
        Err(e) => {
            return ActionResult {
                message: format!("Failed to archive {issue_id}: {e}"),
                message_kind: MessageKind::Error,
                ..Default::default()
            };
        }
    };

    if let Some(dir) = worktree {
        if let Err(e) = crate::worktree::remove_worktree_in(config, dir, false) {
            return ActionResult {
                message: format!("Failed to archive {issue_id}: {e}"),
                message_kind: MessageKind::Error,
                ..Default::default()
            };
        }
    }

    let message = match (worktree.is_some(), session_killed) {
        (true, true) => format!("Archived {issue_id} (session killed, worktree removed)"),
        (true, false) => format!("Archived {issue_id} (worktree removed)"),
        (false, true) => format!("Archived {issue_id} (session killed)"),
        (false, false) => format!("Archived {issue_id}"),
    };
    ActionResult {
        message,
        issue_to_archive: Some((project_id, issue_id)),
        ..Default::default()
    }
}

/// Apply a completed archive to the in-memory board: drop the worktree and
/// move the issue to Done (stamping `done_at`). Mirrors `delete_issue_from_app`
/// so state mutation happens on the main thread after the async teardown.
pub fn archive_issue_in_app(app: &mut App, project_id: &ProjectId, issue_id: &str) -> bool {
    let query = app.search_query.clone();
    let Some(project) = app.find_project_mut(project_id) else {
        return false;
    };
    let Some(issue) = project
        .issues
        .iter_mut()
        .find(|i| i.id.eq_ignore_ascii_case(issue_id))
    else {
        return false;
    };
    issue.worktree = None;
    issue.move_to_column(Column::Done, crate::app::unix_now());
    project.clamp_all_rows(&query);
    project.mark_dirty();
    true
}

pub fn delete_issue_from_app(app: &mut App, project_id: &ProjectId, issue_id: &str) -> bool {
    let query = app.search_query.clone();
    let Some(project) = app.find_project_mut(project_id) else {
        return false;
    };
    let Some(index) = project
        .issues
        .iter()
        .position(|issue| issue.id.eq_ignore_ascii_case(issue_id))
    else {
        return false;
    };

    let removed = project.issues.remove(index);
    crate::ops::remove_link_references(&mut project.issues, &removed.id);
    if project
        .link_filter
        .as_deref()
        .is_some_and(|anchor| anchor.eq_ignore_ascii_case(&removed.id))
    {
        project.link_filter = None;
    }
    project.marked_issues.remove(&removed.id.to_lowercase());
    project.clamp_all_rows(&query);
    project.mark_dirty();
    true
}

fn launch_and_report(issue: Issue, config: AppConfig) -> ActionResult {
    match agent::launch_session(&issue, &config) {
        Ok((session_name, agent_sid, setup_ran)) => ActionResult {
            launched_setup_ran: setup_ran,
            message: format!("Session '{}' started", session_name),
            session_to_open: Some(session_name),
            launched_session: agent_sid.map(|sid| LaunchedSession {
                agent: issue.agent_kind,
                kind: issue.kind,
                session_id: sid,
            }),
            launched_issue_id: Some(issue.id),
            ..Default::default()
        },
        Err(e) => ActionResult {
            message: format!("Failed to launch: {e}"),
            message_kind: MessageKind::Error,
            launched_issue_id: Some(issue.id),
            ..Default::default()
        },
    }
}

/// Terminate a session and map the outcome to a user-facing `ActionResult`.
/// `killed_msg` is shown when a live session was killed, `absent_msg` when
/// there was none; the failure message is uniform across call sites.
pub fn terminate_to_result(
    project_root: &Path,
    session_name: &str,
    killed_msg: String,
    absent_msg: String,
) -> ActionResult {
    let (message, message_kind) = match agent::terminate_session(project_root, session_name) {
        Ok(true) => (killed_msg, MessageKind::Info),
        Ok(false) => (absent_msg, MessageKind::Info),
        Err(e) => (
            format!("Failed to kill session '{session_name}': {e}"),
            MessageKind::Error,
        ),
    };
    ActionResult {
        message,
        message_kind,
        ..Default::default()
    }
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

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".to_string(),
            project_root: PathBuf::from("/tmp/test-bork"),
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            default_prompt: Some("Check AGENTS.md for context.".to_string()),
            review_prompt: None,
            stack_review_prompt: None,
            orchestrator_prompt: None,
            setup_script: None,
            teardown_script: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
            auto_import_reviews: true,
            auto_import_authored_prs: true,
            agents_allowlist: None,
            prune_threshold: crate::config::DEFAULT_PRUNE_THRESHOLD,
            auto_prune_check_interval: crate::config::DEFAULT_AUTO_PRUNE_CHECK_INTERVAL,
            agent_launch: std::collections::HashMap::new(),
        }
    }

    fn test_app() -> App {
        let state = crate::config::AppState::default();
        let mut app = App::new(test_config(), state);
        app.project_mut().available_agents = AgentKind::ALL.to_vec();
        app
    }

    fn test_issue(id: &str, column: Column) -> crate::types::Issue {
        crate::types::Issue::new(
            id,
            format!("Test issue {}", id),
            column,
            crate::types::AgentKind::OpenCode,
        )
    }

    fn test_issue_titled(id: &str, title: &str, column: Column) -> crate::types::Issue {
        crate::types::Issue::new(id, title, column, crate::types::AgentKind::OpenCode)
    }

    fn stack_test_app(count: u32) -> App {
        use crate::types::{ChecksStatus, GithubStack, GithubStackPullRequest, PrState, PrStatus};
        let mut app = test_app();
        let project = app.project_mut();
        let mut members = Vec::new();
        for number in 1..=count {
            let pr = PrStatus {
                number,
                title: format!("Change {number}"),
                url: String::new(),
                author: "author".into(),
                state: PrState::Open,
                is_draft: false,
                checks: Some(ChecksStatus::Success),
                review: None,
                additions: 100,
                deletions: 20,
                head_branch: format!("branch-{number}"),
                is_cross_repository: false,
            };
            members.push(GithubStackPullRequest {
                number,
                state: pr.state,
                is_draft: false,
                head_branch: pr.head_branch.clone(),
            });
            project
                .live
                .pr_statuses
                .insert(pr.head_branch.clone(), pr.clone());
            project.live.pr_statuses_by_number.insert(number, pr);
        }
        project.live.github_stacks.push(GithubStack {
            number: 42,
            url: String::new(),
            base_ref: "main".into(),
            open: true,
            pull_requests: members,
        });
        project.live.stacks_available = true;
        project.live.pr_poll_done = true;
        app.picker_tab = ImportSource::GitHub;
        app
    }

    #[test]
    fn github_picker_pins_attached_prs_and_keeps_focus_when_toggled() {
        let mut app = stack_test_app(4);
        app.project_mut()
            .live
            .pr_statuses_by_number
            .get_mut(&4)
            .unwrap()
            .state = crate::types::PrState::Merged;
        let ctx = app.action_context();
        app.open_import_picker(&ctx);
        assert_eq!(
            app.filtered_github_prs()
                .iter()
                .map(|pr| pr.number)
                .collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
        app.close_linear_picker();
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.github_pr_links.push(LinkedGithubPr {
            number: 1,
            imported: false,
            import_source: None,
        });
        app.project_mut().issues.push(issue.clone());
        app.open_edit_dialog(&issue, 0, &ctx);
        app.open_import_picker_with_context(LinearPickerContext::Attach, &ctx);
        assert_eq!(
            app.filtered_github_prs()
                .iter()
                .map(|pr| pr.number)
                .collect::<Vec<_>>(),
            vec![1, 4, 3, 2]
        );
        app.linear_picker.as_mut().unwrap().selected = 3;
        act(&mut app, Action::LinearPickerSelect);
        assert_eq!(
            app.filtered_github_prs()
                .iter()
                .map(|pr| pr.number)
                .collect::<Vec<_>>(),
            vec![2, 1, 4, 3]
        );
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 0);
        act(&mut app, Action::LinearPickerSelect);
        assert_eq!(
            app.filtered_github_prs()
                .iter()
                .map(|pr| pr.number)
                .collect::<Vec<_>>(),
            vec![1, 4, 3, 2]
        );
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 3);
    }

    #[test]
    fn saved_prs_remain_editable_without_live_github_data() {
        let mut app = test_app();
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.github_pr_links = vec![
            LinkedGithubPr {
                number: 52617,
                imported: true,
                import_source: Some(PrImportSource::Authored),
            },
            LinkedGithubPr {
                number: 52618,
                imported: false,
                import_source: None,
            },
        ];
        app.project_mut().issues.push(issue.clone());
        let ctx = app.action_context();
        app.open_edit_dialog(&issue, 0, &ctx);
        assert_eq!(
            app.dialog.as_ref().unwrap().github_prs,
            issue.github_pr_links
        );
        assert!(app.dialog.as_ref().unwrap().github_available);
        app.picker_tab = ImportSource::GitHub;
        app.open_import_picker_with_context(LinearPickerContext::Attach, &ctx);
        assert_eq!(app.input_mode, InputMode::LinearPicker);
        let entries = app.filtered_github_prs();
        assert_eq!(
            entries.iter().map(|entry| entry.number).collect::<Vec<_>>(),
            vec![52618, 52617]
        );
        assert!(entries.iter().all(|entry| entry.status.is_none()));
        app.close_linear_picker();
        act(&mut app, Action::DialogSubmit);
        assert_eq!(
            app.project().issues[0].github_pr_links,
            issue.github_pr_links
        );
        app.open_edit_dialog(&issue, 0, &ctx);
        app.open_import_picker_with_context(LinearPickerContext::Attach, &ctx);
        act(&mut app, Action::LinearPickerSelect);
        app.close_linear_picker();
        act(&mut app, Action::DialogSubmit);
        assert_eq!(
            app.project().issues[0].github_pr_links,
            issue.github_pr_links[..1]
        );
        assert_eq!(app.project().linked_pr_numbers(), vec![52617]);
    }

    #[test]
    fn editing_with_partial_pr_data_preserves_all_links_and_metadata() {
        let mut app = stack_test_app(1);
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.github_pr_links = vec![
            LinkedGithubPr {
                number: 1,
                imported: true,
                import_source: Some(PrImportSource::ReviewRequested),
            },
            LinkedGithubPr {
                number: 2,
                imported: false,
                import_source: None,
            },
        ];
        app.project_mut().issues.push(issue.clone());
        let ctx = app.action_context();
        app.open_edit_dialog(&issue, 0, &ctx);
        act(&mut app, Action::DialogSubmit);
        assert_eq!(
            app.project().issues[0].github_pr_links,
            issue.github_pr_links
        );
        app.picker_tab = ImportSource::GitHub;
        app.open_import_picker(&ctx);
        assert_eq!(app.filtered_github_prs().len(), 2);
    }

    #[test]
    fn pr_icons_preserve_checks_review_and_draft_without_labels() {
        use crate::types::{ChecksStatus, ReviewDecision};
        let mut app = stack_test_app(1);
        let pr = app
            .project_mut()
            .live
            .pr_statuses_by_number
            .get_mut(&1)
            .unwrap();
        pr.checks = Some(ChecksStatus::Success);
        pr.review = Some(ReviewDecision::ChangesRequested);
        pr.is_draft = true;
        let spans = crate::ui::card::pr_spans(pr);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            " ✓ ● draft"
        );
        assert_eq!(spans[0].style.fg, Some(ratatui::style::Color::Green));
        assert_eq!(spans[1].style.fg, Some(ratatui::style::Color::Red));
    }

    #[test]
    fn stack_picker_enter_preserves_single_pr_and_ctrl_s_imports_stack_once() {
        let mut app = stack_test_app(3);
        let ctx = app.action_context();
        app.open_import_picker(&ctx);
        act(&mut app, Action::LinearPickerSelect);
        assert_eq!(app.project().issues[0].pr_numbers(), vec![3]);
        assert_eq!(app.project().issues[0].github_stack, None);
        app.open_import_picker(&ctx);
        act(&mut app, Action::AttachStack);
        assert_eq!(app.project().issues[1].github_stack, Some(42));
        assert!(app.project().issues[1].github_pr_links.is_empty());
        app.open_import_picker(&ctx);
        act(&mut app, Action::AttachStack);
        assert_eq!(app.project().issues.len(), 2);
    }

    #[test]
    fn stack_dialog_attachment_is_cancelable_and_preserves_individual_links() {
        let mut app = stack_test_app(3);
        let ctx = app.action_context();
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.github_pr_links.push(LinkedGithubPr {
            number: 3,
            imported: false,
            import_source: None,
        });
        app.project_mut().issues.push(issue.clone());
        app.open_edit_dialog(&issue, 0, &ctx);
        app.open_import_picker_with_context(LinearPickerContext::Attach, &ctx);
        act(&mut app, Action::AttachStack);
        assert_eq!(app.dialog.as_ref().unwrap().github_stack, Some(42));
        assert_eq!(app.project().issues[0].github_stack, None);
        app.close_linear_picker();
        app.close_dialog();
        assert_eq!(app.project().issues[0], issue);
        app.open_edit_dialog(&issue, 0, &ctx);
        app.open_import_picker_with_context(LinearPickerContext::Attach, &ctx);
        act(&mut app, Action::AttachStack);
        app.close_linear_picker();
        act(&mut app, Action::DialogSubmit);
        assert_eq!(app.project().issues[0].github_stack, Some(42));
        assert_eq!(app.project().issues[0].pr_numbers(), vec![3]);
    }

    #[test]
    fn stack_membership_updates_and_only_open_members_are_actionable() {
        use crate::types::PrState;
        let mut app = stack_test_app(4);
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::CodeReview)
        };
        let project = app.project_mut();
        project.live.github_stacks[0].pull_requests[0].state = PrState::Merged;
        project.live.github_stacks[0].pull_requests[1].state = PrState::Closed;
        assert_eq!(project.stack_open_numbers(&issue).unwrap(), vec![3, 4]);
        project.live.github_stacks[0].pull_requests.remove(2);
        assert_eq!(project.stack_open_numbers(&issue).unwrap(), vec![4]);
        assert_eq!(project.issue_pr_numbers(&issue), vec![1, 2, 4]);
        project.live.stacks_available = false;
        assert!(project.stack_open_numbers(&issue).is_err());
        project.live.stacks_available = true;
        project.live.github_stacks.clear();
        assert!(project.stack_open_numbers(&issue).is_err());
    }

    #[test]
    fn stack_checks_include_unknown_and_ignore_merged_members() {
        use crate::types::{ChecksStatus, PrState};
        let mut app = stack_test_app(5);
        let project = app.project_mut();
        project
            .live
            .pr_statuses_by_number
            .get_mut(&1)
            .unwrap()
            .checks = Some(ChecksStatus::Error);
        project
            .live
            .pr_statuses_by_number
            .get_mut(&2)
            .unwrap()
            .checks = Some(ChecksStatus::Pending);
        project
            .live
            .pr_statuses_by_number
            .get_mut(&3)
            .unwrap()
            .checks = None;
        project.live.github_stacks[0].pull_requests[4].state = PrState::Merged;
        assert_eq!(
            project.stack_checks(&project.live.github_stacks[0]).label(),
            "1 failed · 1 pending · 1 unknown · 1 passed"
        );
    }

    #[test]
    fn stack_attachment_suppresses_auto_import_of_members_and_survives_sync() {
        let mut app = stack_test_app(3);
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::CodeReview)
        };
        let project = app.project_mut();
        project.live.user_prs = project
            .live
            .pr_statuses_by_number
            .values()
            .cloned()
            .collect();
        project.issues.push(issue);
        project.sync_prs_as_issues();
        assert_eq!(project.issues.len(), 1);
        project.live.user_prs.clear();
        project.sync_prs_as_issues();
        assert_eq!(project.issues.len(), 1);
    }

    #[test]
    fn stack_details_loading_only_tracks_its_own_missing_statuses() {
        let mut app = stack_test_app(5);
        app.project_mut().issues.push(Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::Todo)
        });
        app.project_mut().live.pr_loading_more = true;
        act(&mut app, Action::ExpandStack);
        for missing in [false, true] {
            if missing {
                app.project_mut().live.pr_statuses_by_number.remove(&1);
                app.project_mut()
                    .live
                    .pr_statuses
                    .retain(|_, pr| pr.number != 1);
            }
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|frame| crate::ui::stack_details::render(frame, &app))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            let spinner: String = app
                .spinner_frame()
                .iter()
                .map(|filled| if *filled { '●' } else { '○' })
                .collect();
            assert_eq!(text.contains(&spinner), missing);
            assert!(!text.contains("loading"));
            if !missing {
                assert!(text.contains("CI:"));
            }
        }
    }

    #[test]
    fn github_spinner_uses_global_corner_and_stops_after_loading() {
        let mut app = stack_test_app(1);
        for (loading, missing_cli) in [(true, false), (false, false), (true, true)] {
            app.project_mut().live.pr_loading_more = loading;
            app.project_mut().live.gh_missing = missing_cli;
            app.input_mode = InputMode::Normal;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 1)).unwrap();
            terminal
                .draw(|frame| crate::ui::status_bar::render_footer(frame, &app, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let corner: String = (34..39).map(|x| buffer[(x, 0)].symbol()).collect();
            let spinner: String = app
                .spinner_frame()
                .iter()
                .map(|filled| if *filled { '●' } else { '○' })
                .collect();
            assert_eq!(corner == spinner, loading && !missing_cli);
        }
    }

    #[test]
    fn update_notice_cannot_overlap_global_loading_spinner() {
        let mut app = stack_test_app(1);
        app.project_mut().live.pr_loading_more = true;
        app.update_available = true;
        app.input_mode = InputMode::Normal;
        for width in [40, 160] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| crate::ui::status_bar::render_footer(frame, &app, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let corner: String = (width - 6..width - 1)
                .map(|x| buffer[(x, 0)].symbol())
                .collect();
            let spinner: String = app
                .spinner_frame()
                .iter()
                .map(|filled| if *filled { '●' } else { '○' })
                .collect();
            assert_eq!(corner, spinner);
            assert_eq!(buffer[(width - 8, 0)].symbol(), " ");
            assert_eq!(buffer[(width - 7, 0)].symbol(), " ");
            if width == 160 {
                let text: String = (0..width - 6).map(|x| buffer[(x, 0)].symbol()).collect();
                assert!(text.contains("↑ Update Available"));
            }
        }
    }

    #[test]
    fn global_spinner_clears_between_requests_on_the_same_screen() {
        let mut app = stack_test_app(1);
        app.input_mode = InputMode::Normal;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 1)).unwrap();
        // Idle before first poll, initial fetch, batches, done, refresh, failure.
        for (done, refreshing, more, error, visible) in [
            (false, false, false, false, false),
            (false, true, false, false, true),
            (true, false, true, false, true),
            (true, false, false, false, false),
            (true, true, false, false, true),
            (true, false, false, true, false),
        ] {
            let live = &mut app.project_mut().live;
            live.pr_poll_done = done;
            live.pr_refreshing = refreshing;
            live.pr_loading_more = more;
            live.github_error = error.then(|| "Request failed".into());
            terminal
                .draw(|frame| crate::ui::status_bar::render_footer(frame, &app, frame.area()))
                .unwrap();
            let corner: String = (94..99)
                .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                .collect();
            let spinner: String = app
                .spinner_frame()
                .iter()
                .map(|filled| if *filled { '●' } else { '○' })
                .collect();
            assert_eq!(corner == spinner, visible);
        }
    }

    #[test]
    fn github_picker_uses_spinner_without_hiding_errors() {
        let mut app = stack_test_app(1);
        let ctx = app.action_context();
        app.open_import_picker(&ctx);
        app.picker_tab = ImportSource::GitHub;
        for loading in [true, false] {
            app.project_mut().live.pr_loading_more = loading;
            app.project_mut().live.github_error =
                (!loading).then(|| "GitHub authentication failed".into());
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| crate::ui::linear_picker::render_import_picker(frame, &app))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            let spinner: String = app
                .spinner_frame()
                .iter()
                .map(|filled| if *filled { '●' } else { '○' })
                .collect();
            assert_eq!(text.contains(&spinner), loading);
            assert!(!text.contains("Loading"));
            assert_eq!(text.contains("GitHub authentication failed"), !loading);
        }
    }

    #[test]
    fn stack_attachment_survives_edit_and_serialization() {
        let mut app = stack_test_app(2);
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::Todo)
        };
        app.project_mut().issues.push(issue.clone());
        let ctx = app.action_context();
        app.open_edit_dialog(&issue, 0, &ctx);
        act(&mut app, Action::DialogSubmit);
        let json = serde_json::to_string(&app.project().issues[0]).unwrap();
        let saved: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(saved.github_stack, Some(42));
        app.project_mut().issues[0] = saved;
        act(&mut app, Action::ExpandStack);
        assert_eq!(app.input_mode, InputMode::StackDetails);
        assert!(app.message.is_none());
    }

    #[test]
    fn stack_details_scrolls_large_stacks_and_handles_removed_issue() {
        let mut app = stack_test_app(120);
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::Todo)
        };
        app.project_mut().issues.push(issue);
        act(&mut app, Action::ExpandStack);
        for _ in 0..150 {
            act(&mut app, Action::StackDown);
        }
        assert_eq!(app.stack_details.as_ref().unwrap().selected, 119);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::stack_details::render(frame, &app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("#120"));
        assert!(text.contains("Esc close"));
        assert!(!text.contains("#1 "));
        app.project_mut().issues.clear();
        act(&mut app, Action::StackUp);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.stack_details.is_none());
    }

    fn card_rows(app: &App, issue: &Issue, width: u16, size: crate::app::CardSize) -> Vec<String> {
        use crate::ui::card::{render_card, CardContext, CARD_HEIGHT, CARD_HEIGHT_MEDIUM};
        let height = match size {
            crate::app::CardSize::Full => CARD_HEIGHT,
            crate::app::CardSize::Medium => CARD_HEIGHT_MEDIUM,
        };
        let context = CardContext {
            issue,
            selected: true,
            marked: false,
            session_alive: false,
            agent_status: crate::types::AgentStatus::Idle,
            activity: None,
            git_status: None,
            pr: None,
            project: app.project(),
            ports: None,
            search_query: "",
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_card(frame, &context, frame.area(), size))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn card_footer_keeps_links_left_without_shortcut_hints_in_both_layouts() {
        let app = stack_test_app(2);
        for size in [crate::app::CardSize::Full, crate::app::CardSize::Medium] {
            for kind in [
                IssueKind::Agentic,
                IssueKind::NonAgentic,
                IssueKind::Orchestrator,
            ] {
                let issue = Issue {
                    kind,
                    github_stack: Some(42),
                    linked_issues: vec!["bork-2".into()],
                    linear_links: vec![LinkedLinear {
                        id: "id".into(),
                        identifier: "DOCS-3902".into(),
                        url: String::new(),
                        imported: false,
                    }],
                    ..test_issue("bork-1", Column::Todo)
                };
                for width in [28, 44, 70] {
                    let rows = card_rows(&app, &issue, width, size);
                    let footer = &rows[rows.len() - 2];
                    assert!(footer.contains("∞1 ◈"), "{footer}");
                    assert!(!footer.contains("s expand"), "{footer}");
                    assert!(!rows[2].contains('∞'));
                    let kind_label = match kind {
                        IssueKind::Orchestrator => "orch",
                        IssueKind::NonAgentic => "todo",
                        IssueKind::Agentic => "",
                    };
                    if !kind_label.is_empty() {
                        assert!(rows[0].contains(&format!("bork-1 · {kind_label}")));
                        assert!(!rows[2].to_lowercase().contains(kind_label));
                    }
                    if width >= 44 {
                        assert!(footer.contains("DOCS-3902"));
                    }
                    if size == crate::app::CardSize::Full {
                        assert!(rows[3].contains("┌ #1"));
                    } else {
                        assert!(rows[3].contains("2 PRs"));
                    }
                }
            }
        }
    }

    #[test]
    fn individual_pr_card_hides_diff_for_draft_and_merged_prs() {
        let mut app = stack_test_app(1);
        let issue = Issue {
            github_pr_links: vec![LinkedGithubPr {
                number: 1,
                imported: false,
                import_source: None,
            }],
            ..test_issue("bork-1", Column::Todo)
        };
        for (draft, state) in [
            (false, crate::types::PrState::Open),
            (true, crate::types::PrState::Open),
            (false, crate::types::PrState::Merged),
        ] {
            let pr = app
                .project_mut()
                .live
                .pr_statuses_by_number
                .get_mut(&1)
                .unwrap();
            pr.is_draft = draft;
            pr.state = state;
            pr.additions = 182;
            pr.deletions = 7;
            let text = card_rows(&app, &issue, 70, crate::app::CardSize::Full).join("\n");
            assert_eq!(
                text.contains("+182/-7"),
                !draft && state != crate::types::PrState::Merged
            );
            assert_eq!(
                text.contains("merged"),
                state == crate::types::PrState::Merged
            );
            assert_eq!(text.contains("draft"), draft);
        }
    }

    #[test]
    fn github_card_distinguishes_loading_errors_and_optional_capabilities() {
        let mut app = stack_test_app(2);
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::Todo)
        };
        app.project_mut().live.github_stacks.clear();
        app.project_mut().live.stacks_available = false;
        app.project_mut().live.github_error = Some("network failed".into());
        for phase in 0..3 {
            let live = &mut app.project_mut().live;
            live.pr_poll_done = phase != 0;
            live.pr_refreshing = phase == 1;
            live.pr_loading_more = phase == 2;
            let text = card_rows(&app, &issue, 70, crate::app::CardSize::Full).join("\n");
            assert!(!text.contains("loading"));
            assert!(text.contains("Stack #42"));
            assert!(!text.contains("unavailable"));
        }
        app.project_mut().live.pr_loading_more = false;
        let text = card_rows(&app, &issue, 70, crate::app::CardSize::Full).join("\n");
        assert!(text.contains("unavailable"));
        app.project_mut().live.github_error = None;
        let text = card_rows(&app, &issue, 70, crate::app::CardSize::Full).join("\n");
        assert!(text.contains("not found"));
        for missing_cli in [false, true] {
            app.project_mut().live.gh_missing = missing_cli;
            app.project_mut().live.stacks_unsupported = !missing_cli;
            let text = card_rows(&app, &issue, 70, crate::app::CardSize::Full).join("\n");
            assert!(!text.contains("Stack"));
            assert!(!text.contains("s expand"));
            assert!(!text.contains("unavailable"));
        }
    }

    #[test]
    fn small_stack_card_shows_each_pr_without_hiding_draft_or_review() {
        use crate::types::{AgentStatus, ChecksStatus, ReviewDecision};
        let mut app = stack_test_app(2);
        for old_number in [1, 2] {
            let project = app.project_mut();
            let mut pr = project
                .live
                .pr_statuses_by_number
                .remove(&old_number)
                .unwrap();
            pr.number += 52616;
            pr.is_draft = old_number == 1;
            pr.checks = Some(ChecksStatus::Success);
            pr.review = Some(ReviewDecision::ChangesRequested);
            project.live.github_stacks[0].pull_requests[(old_number - 1) as usize].number =
                pr.number;
            project.live.pr_statuses_by_number.insert(pr.number, pr);
        }
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::Todo)
        };
        for width in [28, 44, 70] {
            let context = crate::ui::card::CardContext {
                issue: &issue,
                selected: true,
                marked: false,
                session_alive: false,
                agent_status: AgentStatus::Idle,
                activity: None,
                git_status: None,
                pr: None,
                project: app.project(),
                ports: None,
                search_query: "",
            };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 7)).unwrap();
            terminal
                .draw(|frame| {
                    crate::ui::card::render_card(
                        frame,
                        &context,
                        frame.area(),
                        crate::app::CardSize::Full,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("┌ #52617 ✓ ● draft"));
            assert!(text.contains("└ #52618 ✓ ●"));
            assert!(!text.contains('ø'));
            assert!(!text.contains("CI:"));
            assert!(!text.contains("Stack #"));
            assert!(!text.contains("s expand"));
            for row in 1..6 {
                assert_eq!(buffer[(width - 1, row)].symbol(), "│");
            }
        }
    }

    #[test]
    fn large_stack_summary_stays_on_one_readable_line() {
        let mut app = stack_test_app(5);
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::Todo)
        };
        for pr in app.project_mut().live.pr_statuses_by_number.values_mut() {
            pr.checks = Some(crate::types::ChecksStatus::Pending);
        }
        for size in [crate::app::CardSize::Full, crate::app::CardSize::Medium] {
            for width in [28, 44, 70] {
                let rows = card_rows(&app, &issue, width, size);
                assert!(rows[3].contains("5 PRs · ◌ 5 pending"), "{}", rows[3]);
                assert!(!rows[3].contains('▸'));
                assert!(!rows[rows.len() - 2].contains("s expand"));
                if size == crate::app::CardSize::Full {
                    assert!(rows[4].trim_matches('│').trim().is_empty());
                }
            }
        }
    }

    #[test]
    fn stack_card_uses_summary_and_shows_refresh_failures() {
        use crate::types::{AgentStatus, ChecksStatus};
        let mut app = stack_test_app(12);
        let issue = Issue {
            github_stack: Some(42),
            ..test_issue("bork-1", Column::Todo)
        };
        app.project_mut()
            .live
            .pr_statuses_by_number
            .get_mut(&1)
            .unwrap()
            .checks = Some(ChecksStatus::Failure);
        for width in [28, 44, 70] {
            for available in [true, false] {
                app.project_mut().live.stacks_available = available;
                app.project_mut().live.github_error =
                    (!available).then(|| "GitHub failed".to_string());
                let project = app.project();
                let context = crate::ui::card::CardContext {
                    issue: &issue,
                    selected: true,
                    marked: false,
                    session_alive: false,
                    agent_status: AgentStatus::Idle,
                    activity: None,
                    git_status: None,
                    pr: None,
                    project,
                    ports: None,
                    search_query: "",
                };
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 7)).unwrap();
                terminal
                    .draw(|frame| {
                        crate::ui::card::render_card(
                            frame,
                            &context,
                            frame.area(),
                            crate::app::CardSize::Full,
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let text = buffer
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains("12 PRs"));
                if available {
                    assert!(text.contains("failed") || text.contains("✗ 1"));
                } else {
                    assert!(text.contains("unavailable"));
                }
                assert!(!text.contains("+100"));
                for row in 1..6 {
                    assert_eq!(buffer[(width - 1, row)].symbol(), "│");
                }
            }
        }
    }

    // ================================================================
    // Dialog: prompt field stays empty on field navigation
    // ================================================================

    #[test]
    fn dialog_next_field_does_not_auto_fill_prompt() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);

        // Type a title (starts on Title field = 3 for Agentic with agents, no linear)
        handle_action(&mut app, Action::DialogChar('H'), &ctx, &test_channels());
        handle_action(&mut app, Action::DialogChar('i'), &ctx, &test_channels());

        // Move from title (field 3) to prompt (field 4)
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.focused_field, 4);
        assert_eq!(
            dialog.prompt_text(),
            "",
            "prompt should remain empty after navigating from title"
        );
    }

    #[test]
    fn dialog_next_field_preserves_user_typed_prompt() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);

        // Move to prompt field
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());

        // Type something in the prompt
        handle_action(&mut app, Action::DialogChar('g'), &ctx, &test_channels());
        handle_action(&mut app, Action::DialogChar('o'), &ctx, &test_channels());

        let dialog = app.dialog.as_ref().unwrap();
        assert_eq!(dialog.prompt_text(), "go");
    }

    #[test]
    fn dialog_next_field_advances_through_all_fields() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);

        // Agentic with agents, no linear: Kind(0), Agent(1), Mode(2), Title(3), Prompt(4)
        // Starts on Title (field 3)
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 3);

        // Tab: Title(3) -> Prompt(4)
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 4);

        // Tab on last field (Prompt) wraps to Kind(0)
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn dialog_prev_field_goes_back() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);

        // Starts on Title (field 3). Advance to Prompt (field 4)
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 4);

        // Go back to Title (field 3)
        handle_action(&mut app, Action::DialogPrevField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 3);
    }

    #[test]
    fn dialog_prev_field_wraps_to_last_field() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);

        // Agentic with agents, no linear: Kind(0), Agent(1), Mode(2), Title(3), Prompt(4)
        // Starts on Title (field 3). Three Shift+Tabs -> Kind(0)
        handle_action(&mut app, Action::DialogPrevField, &ctx, &test_channels());
        handle_action(&mut app, Action::DialogPrevField, &ctx, &test_channels());

        handle_action(&mut app, Action::DialogPrevField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);

        // One more Shift+Tab wraps to last field (Prompt = 4)
        handle_action(&mut app, Action::DialogPrevField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 4);
    }

    #[test]
    fn dialog_space_char_appended_to_prompt() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);

        // Move to prompt field
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());

        handle_action(&mut app, Action::DialogChar('a'), &ctx, &test_channels());
        handle_action(&mut app, Action::DialogChar(' '), &ctx, &test_channels());
        handle_action(&mut app, Action::DialogChar('b'), &ctx, &test_channels());

        assert_eq!(app.dialog.as_ref().unwrap().prompt_text(), "a b");
    }

    #[test]
    fn dialog_cancel_closes_dialog() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);
        assert!(app.dialog.is_some());

        handle_action(&mut app, Action::DialogCancel, &ctx, &test_channels());
        assert!(app.dialog.is_none());
    }

    #[test]
    fn start_session_on_non_agentic_opens_edit_dialog() {
        let mut app = test_app();
        app.project_mut().issues.push(crate::types::Issue {
            kind: crate::types::IssueKind::NonAgentic,
            prompt: Some("scratch".to_string()),
            ..test_issue_titled("bork-1", "Manual task", Column::Todo)
        });

        let ctx = app.action_context();
        let post = handle_action(&mut app, Action::StartSession, &ctx, &test_channels());

        assert!(matches!(post, PostAction::None));
        assert_eq!(app.input_mode, InputMode::Dialog);
    }

    #[test]
    fn start_session_on_agentic_returns_launch_without_popup() {
        let mut app = test_app();
        app.project_mut().issues.push(crate::types::Issue {
            worktree: Some("main".to_string()),
            ..test_issue_titled("bork-1", "Work", Column::Todo)
        });

        let ctx = app.action_context();
        let post = handle_action(&mut app, Action::StartSession, &ctx, &test_channels());

        assert!(matches!(
            post,
            PostAction::Launch {
                open_popup: false,
                ..
            }
        ));
        assert_eq!(app.busy_count, 1);
    }

    #[test]
    fn start_session_on_live_session_is_noop() {
        let mut app = test_app();
        app.project_mut().issues.push(crate::types::Issue {
            worktree: Some("main".to_string()),
            ..test_issue_titled("bork-1", "Work", Column::InProgress)
        });

        let session_name = app.project().issues[0].session_name(&app.project().config.project_name);
        app.project_mut().live.active_sessions.insert(session_name);

        let ctx = app.action_context();
        let post = handle_action(&mut app, Action::StartSession, &ctx, &test_channels());

        assert!(matches!(post, PostAction::None));
        assert_eq!(app.busy_count, 0);
    }

    #[test]
    fn open_session_on_non_agentic_opens_edit_dialog() {
        let mut app = test_app();
        app.project_mut().issues.push(crate::types::Issue {
            kind: crate::types::IssueKind::NonAgentic,
            prompt: Some("scratch".to_string()),
            ..test_issue_titled("bork-1", "Manual task", Column::Todo)
        });

        let ctx = app.action_context();
        let post = handle_action(&mut app, Action::OpenSession, &ctx, &test_channels());

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
            worktree: Some("main".to_string()),
            ..test_issue_titled("bork-1", "Test", Column::Todo)
        });

        // Open edit dialog
        let ctx = app.action_context();
        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0, &ctx);

        // Move from title to prompt
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());

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
        app.project_mut().live.linear_issues =
            vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        let ctx = app.action_context();
        app.open_linear_picker(&ctx);
        assert_eq!(app.input_mode, crate::app::InputMode::LinearPicker);

        handle_action(&mut app, Action::LinearPickerSelect, &ctx, &test_channels());

        assert_eq!(app.input_mode, crate::app::InputMode::Normal);
        assert_eq!(app.project().issues.len(), 1);
        assert_eq!(app.project().issues[0].id, "test-1");
        assert_eq!(app.project().issues[0].title, "First issue");
        assert_eq!(app.project().issues[0].column, Column::Todo);
        assert_eq!(app.project().issues[0].linear_links.len(), 1);
        assert_eq!(app.project().issues[0].linear_links[0].id, "uuid-1");
        assert_eq!(app.project().issues[0].linear_links[0].identifier, "TEST-1");
    }

    #[test]
    fn linear_picker_import_rejects_duplicate() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live.linear_issues =
            vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        let ctx = app.action_context();

        // Import once
        app.open_linear_picker(&ctx);
        handle_action(&mut app, Action::LinearPickerSelect, &ctx, &test_channels());
        assert_eq!(app.project().issues.len(), 1);

        // Try to import again (should show issue but reject the import)
        app.open_linear_picker(&ctx);

        // The picker should still show the issue (visible but marked as imported)
        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 1);

        // Attempting to import should not create a duplicate
        handle_action(&mut app, Action::LinearPickerSelect, &ctx, &test_channels());
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn linear_picker_search_filters_issues() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "Login page"),
            test_linear_issue("uuid-2", "TEST-2", "Dashboard bug"),
        ];

        let ctx = app.action_context();
        app.open_linear_picker(&ctx);

        // Type search
        handle_action(
            &mut app,
            Action::LinearPickerChar('l'),
            &ctx,
            &test_channels(),
        );
        handle_action(
            &mut app,
            Action::LinearPickerChar('o'),
            &ctx,
            &test_channels(),
        );
        handle_action(
            &mut app,
            Action::LinearPickerChar('g'),
            &ctx,
            &test_channels(),
        );

        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].identifier, "TEST-1");
    }

    #[test]
    fn linear_picker_navigation() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "First"),
            test_linear_issue("uuid-2", "TEST-2", "Second"),
            test_linear_issue("uuid-3", "TEST-3", "Third"),
        ];

        let ctx = app.action_context();
        app.open_linear_picker(&ctx);
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 0);

        handle_action(&mut app, Action::LinearPickerDown, &ctx, &test_channels());
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 1);

        handle_action(&mut app, Action::LinearPickerDown, &ctx, &test_channels());
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 2);

        // Should not go past the last item
        handle_action(&mut app, Action::LinearPickerDown, &ctx, &test_channels());
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 2);

        handle_action(&mut app, Action::LinearPickerUp, &ctx, &test_channels());
        assert_eq!(app.linear_picker.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn linear_picker_close_restores_normal() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        app.project_mut().live.linear_issues = vec![test_linear_issue("uuid-1", "TEST-1", "First")];

        let ctx = app.action_context();
        app.open_linear_picker(&ctx);
        handle_action(&mut app, Action::LinearPickerClose, &ctx, &test_channels());

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

        app.project_mut()
            .issues
            .push(test_issue_titled("bork-1", "Test issue", Column::Todo));

        let ctx = app.action_context();
        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0, &ctx);

        // Agentic + agents + linear: Kind(0), Agent(1), Mode(2), Linear(3), Title(4), Prompt(5)
        // Should start on Title (field 4)
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 4);
        assert_eq!(app.dialog.as_ref().unwrap().active_field_count(), 6);

        // Tab: Title(4) -> Prompt(5)
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 5);

        // Tab on Prompt (last field) wraps to Kind(0)
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn edit_dialog_with_linear_tab_wraps_without_issues_loaded() {
        let mut app = test_app();
        app.project_mut().linear_available = true;
        // No linear issues loaded
        app.project_mut().live.linear_issues = vec![];

        app.project_mut()
            .issues
            .push(test_issue_titled("bork-1", "Test issue", Column::Todo));

        let ctx = app.action_context();
        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0, &ctx);

        // Agentic + agents + linear: Kind(0), Agent(1), Mode(2), Linear(3), Title(4), Prompt(5)
        // Starts on Title(4). Tab to Prompt(5), then tab wraps.
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 5);

        // Tab on Prompt (last field) wraps to Kind(0)
        handle_action(&mut app, Action::DialogNextField, &ctx, &test_channels());
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 0);
    }

    #[test]
    fn edit_dialog_with_linear_shift_enter_submits_from_any_field() {
        let mut app = test_app();
        app.project_mut().linear_available = true;

        app.project_mut()
            .issues
            .push(test_issue_titled("bork-1", "Test issue", Column::Todo));

        let ctx = app.action_context();
        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0, &ctx);

        // Starts on Title(4). Shift+Enter should submit from any field.
        assert_eq!(app.dialog.as_ref().unwrap().focused_field, 4);

        handle_action(&mut app, Action::DialogSubmit, &ctx, &test_channels());
        assert_eq!(app.input_mode, crate::app::InputMode::Normal);
        assert!(app.dialog.is_none());
    }

    #[test]
    fn untouched_dialog_keeps_concurrent_agent_change() {
        let mut app = test_app();
        app.project_mut()
            .issues
            .push(test_issue_titled("bork-1", "Test issue", Column::Todo));

        let ctx = app.action_context();
        let issue = app.project().issues[0].clone();
        app.open_edit_dialog(&issue, 0, &ctx);

        // While the dialog is open, a CLI edit lands via the state merge and
        // switches the issue's agent to something else.
        let concurrent_agent = if issue.agent_kind == AgentKind::Claude {
            AgentKind::OpenCode
        } else {
            AgentKind::Claude
        };
        let _ = app.project_mut().issues[0].set_agent_kind(concurrent_agent);
        app.project_mut().issues[0]
            .sessions
            .insert(concurrent_agent, "ses_live".to_string());

        // Submitting the untouched dialog must not write its stale agent
        // back over the concurrent change (or kill the live session).
        handle_action(&mut app, Action::DialogSubmit, &ctx, &test_channels());
        assert_eq!(app.project().issues[0].agent_kind, concurrent_agent);
        assert_eq!(
            app.project().issues[0].current_session_id(),
            Some("ses_live")
        );
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

    fn test_channels() -> ActionChannels<'static> {
        // Leak the senders so the references are 'static for test convenience.
        // The receivers are dropped, but that's fine — sends just return Err.
        ActionChannels {
            action_tx: Box::leak(Box::new(mpsc::channel().0)),
            pr_wake_tx: Box::leak(Box::new(mpsc::channel().0)),
            linear_wake_tx: Box::leak(Box::new(mpsc::channel().0)),
            git_wake_tx: Box::leak(Box::new(mpsc::channel().0)),
            reload_tx: Box::leak(Box::new(mpsc::channel().0)),
        }
    }

    fn act(app: &mut App, action: Action) -> PostAction {
        let ctx = app.action_context();
        let ch = test_channels();
        handle_action(app, action, &ctx, &ch)
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
    fn delete_confirm_waits_for_teardown_before_removing_issue() {
        let mut app = app_with_issues();
        let project_id = app.project().id();
        act(&mut app, Action::DeleteIssue);
        assert_eq!(app.input_mode, InputMode::Confirm);
        act(&mut app, Action::ConfirmYes);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.project().issues.len(), 3);

        assert!(delete_issue_from_app(&mut app, &project_id, "bork-1"));
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

    #[test]
    fn archive_issue_opens_confirm_with_archive_action() {
        let mut app = app_with_issues();
        act(&mut app, Action::ArchiveIssue);
        assert_eq!(app.input_mode, InputMode::Confirm);
        match app.pending_confirm.as_ref().unwrap() {
            ConfirmAction::ArchiveIssue { issue_id, .. } => assert_eq!(issue_id, "bork-1"),
            _ => panic!("expected ArchiveIssue"),
        }
    }

    #[test]
    fn archive_issue_in_app_moves_to_done_and_drops_worktree() {
        let mut app = app_with_issues();
        let project_id = app.project().id();
        app.project_mut().issues[0].worktree = Some("bork-1-slug".to_string());

        assert!(archive_issue_in_app(&mut app, &project_id, "bork-1"));

        let issue = &app.project().issues[0];
        assert_eq!(issue.column, Column::Done);
        assert!(issue.done_at.is_some());
        assert!(issue.worktree.is_none());
        assert!(app.project().state_dirty);
    }

    #[test]
    fn archive_issue_in_app_unknown_id_is_noop() {
        let mut app = app_with_issues();
        let project_id = app.project().id();
        assert!(!archive_issue_in_app(&mut app, &project_id, "bork-99"));
    }

    #[test]
    fn dialog_create_in_done_column_stamps_done_at() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog_in_column(Column::Done, &ctx);
        if let Some(ref mut dialog) = app.dialog {
            dialog.title = "Born done".to_string();
        }
        submit_dialog(&mut app, &ctx);

        let issue = app.project().issues.last().unwrap();
        assert_eq!(issue.column, Column::Done);
        assert!(
            issue.done_at.is_some(),
            "TUI create in Done must stamp done_at like the CLI"
        );
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
        assert_eq!(app.search_query, "ab");
    }

    #[test]
    fn search_backspace_removes() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        act(&mut app, Action::SearchChar('a'));
        act(&mut app, Action::SearchChar('b'));
        act(&mut app, Action::SearchBackspace);
        assert_eq!(app.search_query, "a");
    }

    #[test]
    fn search_confirm_returns_to_normal() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        act(&mut app, Action::SearchChar('x'));
        act(&mut app, Action::SearchConfirm);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.search_query, "x"); // query preserved
    }

    #[test]
    fn search_cancel_clears_and_returns() {
        let mut app = test_app();
        act(&mut app, Action::SearchStart);
        act(&mut app, Action::SearchChar('x'));
        act(&mut app, Action::SearchCancel);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.search_query, ""); // query cleared
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
        let (msg, kind) = app.message.as_ref().unwrap();
        assert!(msg.contains("No active session"));
        assert_eq!(*kind, MessageKind::Warning);
    }

    #[test]
    fn kill_session_with_active_session_opens_confirm() {
        let mut app = app_with_issues();
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());
        act(&mut app, Action::KillSession);
        assert_eq!(app.input_mode, InputMode::Confirm);
    }

    // ================================================================
    // Warning messages for missing prerequisites
    // ================================================================

    #[test]
    fn open_pr_no_pr_shows_warning() {
        let mut app = app_with_issues();
        act(&mut app, Action::OpenPR);
        let (msg, kind) = app.message.as_ref().unwrap();
        assert!(msg.contains("No PR found"));
        assert_eq!(*kind, MessageKind::Warning);
    }

    #[test]
    fn open_linear_no_url_shows_warning() {
        let mut app = app_with_issues();
        act(&mut app, Action::OpenLinear);
        let (msg, kind) = app.message.as_ref().unwrap();
        assert!(msg.contains("No Linear issue linked"));
        assert_eq!(*kind, MessageKind::Warning);
    }

    // ================================================================
    // summarize_open_links: batch-open outcome summaries
    // ================================================================

    #[test]
    fn open_links_single_success_names_the_link() {
        let result = summarize_open_links("PR", 1, "#42", vec![]);
        assert_eq!(result.message, "Opened PR #42");
        assert_eq!(result.message_kind, MessageKind::Info);
    }

    #[test]
    fn open_links_multi_success_reports_count() {
        let result = summarize_open_links("PR", 3, "#1", vec![]);
        assert_eq!(result.message, "Opened 3 PRs");
        assert_eq!(result.message_kind, MessageKind::Info);
    }

    #[test]
    fn open_links_single_failure_includes_label_and_error() {
        let result = summarize_open_links("PR", 1, "#42", vec!["#42: no handler".to_string()]);
        assert_eq!(result.message, "Failed to open PR #42: no handler");
        assert_eq!(result.message_kind, MessageKind::Error);
    }

    #[test]
    fn open_links_partial_failure_reports_opened_count() {
        let result = summarize_open_links("PR", 3, "#1", vec!["#2: gone".to_string()]);
        assert_eq!(result.message, "Opened 2 of 3 PRs; #2: gone");
        assert_eq!(result.message_kind, MessageKind::Error);
    }

    #[test]
    fn open_links_extra_failures_are_counted() {
        let failures = vec!["BORK-1: no handler".to_string(), "BORK-2: gone".to_string()];
        let result = summarize_open_links("Linear issue", 2, "BORK-1", failures);
        assert_eq!(
            result.message,
            "Failed to open Linear issue BORK-1: no handler (+1 more failed)"
        );
        assert_eq!(result.message_kind, MessageKind::Error);
    }

    #[test]
    fn debug_inspect_no_issue_shows_warning() {
        let mut app = test_app();
        app.project_mut().config.debug = true;
        act(&mut app, Action::DebugInspect);
        let (msg, kind) = app.message.as_ref().unwrap();
        assert!(msg.contains("No issue selected"));
        assert_eq!(*kind, MessageKind::Warning);
    }

    #[test]
    fn submit_empty_title_shows_warning() {
        let mut app = test_app();
        let ctx = app.action_context();
        app.open_dialog(&ctx);
        // Title is empty by default, submit immediately
        act(&mut app, Action::DialogSubmit);
        assert_eq!(app.input_mode, InputMode::Normal);
        let (msg, kind) = app.message.as_ref().unwrap();
        assert!(msg.contains("Title cannot be empty"));
        assert_eq!(*kind, MessageKind::Warning);
    }

    // --- Multi-project / sidebar tests ---

    fn test_config_named(name: &str) -> AppConfig {
        AppConfig {
            project_name: name.to_string(),
            project_root: PathBuf::from(format!("/tmp/test-{}", name)),
            agent_kind: crate::types::AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            default_prompt: None,
            review_prompt: None,
            stack_review_prompt: None,
            orchestrator_prompt: None,
            setup_script: None,
            teardown_script: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
            auto_import_reviews: true,
            auto_import_authored_prs: true,
            agents_allowlist: None,
            prune_threshold: crate::config::DEFAULT_PRUNE_THRESHOLD,
            auto_prune_check_interval: crate::config::DEFAULT_AUTO_PRUNE_CHECK_INTERVAL,
            agent_launch: std::collections::HashMap::new(),
        }
    }

    fn test_multi_app() -> App {
        let mut app = App::new(
            test_config_named("alpha"),
            crate::config::AppState {
                last_prune_at: None,
                issues: vec![test_issue("alpha-1", Column::Todo)],
            },
        );
        app.add_background_project(
            test_config_named("beta"),
            crate::config::AppState {
                last_prune_at: None,
                issues: vec![test_issue("beta-1", Column::InProgress)],
            },
        );
        app.add_background_project(
            test_config_named("gamma"),
            crate::config::AppState {
                last_prune_at: None,
                issues: vec![test_issue("gamma-1", Column::Todo)],
            },
        );
        app.enable_sidebar();
        app
    }

    #[test]
    fn sidebar_toggle_opens_and_closes() {
        let mut app = test_multi_app();
        assert_eq!(app.input_mode, InputMode::Normal);

        let post = handle_sidebar(&mut app, Action::ToggleSidebar);
        assert!(matches!(post, PostAction::None));
    }

    #[test]
    fn sidebar_toggle_opens_immediately_and_dispatches_reload() {
        crate::global_config::tests::with_temp_config("sidebar-reload", || {
            let mut app = test_multi_app();
            assert_eq!(app.input_mode, InputMode::Normal);
            assert!(!app.sidebar.as_ref().unwrap().visible);

            let (reload_tx, reload_rx) = mpsc::channel();
            let ctx = app.action_context();
            let ch = ActionChannels {
                action_tx: Box::leak(Box::new(mpsc::channel().0)),
                pr_wake_tx: Box::leak(Box::new(mpsc::channel().0)),
                linear_wake_tx: Box::leak(Box::new(mpsc::channel().0)),
                git_wake_tx: Box::leak(Box::new(mpsc::channel().0)),
                reload_tx: &reload_tx,
            };
            handle_action(&mut app, Action::ToggleSidebar, &ctx, &ch);

            assert_eq!(app.input_mode, InputMode::Sidebar);
            assert!(app.sidebar.as_ref().unwrap().visible);
            assert!(app.sidebar.as_ref().unwrap().focused);

            // Background thread should send a ReloadResult
            let result = reload_rx.recv_timeout(std::time::Duration::from_secs(5));
            assert!(result.is_ok());
        });
    }

    #[test]
    fn sidebar_navigation_wraps_around() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().focused = true;
        app.input_mode = InputMode::Sidebar;

        handle_sidebar(&mut app, Action::SidebarUp);
        assert_eq!(app.sidebar.as_ref().unwrap().selected, 2);

        handle_sidebar(&mut app, Action::SidebarDown);
        assert_eq!(app.sidebar.as_ref().unwrap().selected, 0);

        handle_sidebar(&mut app, Action::SidebarDown);
        assert_eq!(app.sidebar.as_ref().unwrap().selected, 1);

        handle_sidebar(&mut app, Action::SidebarDown);
        assert_eq!(app.sidebar.as_ref().unwrap().selected, 2);

        handle_sidebar(&mut app, Action::SidebarDown);
        assert_eq!(app.sidebar.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn sidebar_select_returns_switch_project() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().selected = 1;

        let post = handle_sidebar(&mut app, Action::SidebarSelect);
        assert!(matches!(post, PostAction::SwitchProject { .. }));
        if let PostAction::SwitchProject { id } = &post {
            assert_eq!(*id, app.projects[1].id());
        }
        assert_eq!(
            app.sidebar.as_ref().unwrap().swimlanes,
            vec![app.projects[1].id()]
        );
    }

    #[test]
    fn sidebar_select_same_project_no_switch() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().selected = 0;

        let post = handle_sidebar(&mut app, Action::SidebarSelect);
        assert!(matches!(post, PostAction::None));
    }

    #[test]
    fn sidebar_toggle_swimlane_add_remove() {
        let mut app = test_multi_app();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().selected = 1;

        handle_sidebar(&mut app, Action::SidebarToggleSwimlane);
        assert!(app.sidebar.as_ref().unwrap().swimlanes.contains(&beta_id));

        app.sidebar.as_mut().unwrap().selected = 1;
        handle_sidebar(&mut app, Action::SidebarToggleSwimlane);
        assert!(!app.sidebar.as_ref().unwrap().swimlanes.contains(&beta_id));
    }

    #[test]
    fn sidebar_toggle_swimlane_max_three() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().swimlanes = vec![
            app.projects[0].id(),
            app.projects[1].id(),
            app.projects[2].id(),
        ];

        app.add_background_project(
            test_config_named("delta"),
            crate::config::AppState::default(),
        );

        app.sidebar.as_mut().unwrap().selected = 3;
        handle_sidebar(&mut app, Action::SidebarToggleSwimlane);
        assert_eq!(app.sidebar.as_ref().unwrap().swimlanes.len(), 3);
        assert!(app.message.is_some());
    }

    #[test]
    fn sidebar_toggle_swimlane_cant_remove_last() {
        let mut app = test_multi_app();
        let alpha_id = app.projects[0].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone()];
        app.sidebar.as_mut().unwrap().selected = 0;

        handle_sidebar(&mut app, Action::SidebarToggleSwimlane);
        assert_eq!(app.sidebar.as_ref().unwrap().swimlanes, vec![alpha_id]);
    }

    #[test]
    fn next_prev_swimlane_wraps() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().swimlanes = vec![
            app.projects[0].id(),
            app.projects[1].id(),
            app.projects[2].id(),
        ];
        app.focused_swimlane = 0;

        act(&mut app, Action::NextSwimlane);
        assert_eq!(app.focused_swimlane, 1);

        act(&mut app, Action::NextSwimlane);
        assert_eq!(app.focused_swimlane, 2);

        act(&mut app, Action::NextSwimlane);
        assert_eq!(app.focused_swimlane, 0);

        act(&mut app, Action::PrevSwimlane);
        assert_eq!(app.focused_swimlane, 2);
    }

    #[test]
    fn next_swimlane_noop_single() {
        let mut app = test_multi_app();
        app.focused_swimlane = 0;

        act(&mut app, Action::NextSwimlane);
        assert_eq!(app.focused_swimlane, 0);
    }

    // --- High-impact multi-project tests ---

    #[test]
    fn create_issue_on_swimlane_goes_to_correct_project() {
        let mut app = test_multi_app();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id, beta_id];
        app.focused_swimlane = 1; // focus beta

        let ctx = app.action_context();
        assert_eq!(ctx.project_id, app.projects[1].id());

        // Open dialog on beta
        app.open_dialog(&ctx);
        assert_eq!(app.input_mode, InputMode::Dialog);

        if let Some(ref mut dialog) = app.dialog {
            dialog.title = "Beta issue".to_string();
        }

        submit_dialog(&mut app, &ctx);

        // Verify issue is in beta (projects[1]), not alpha (projects[0])
        assert_eq!(
            app.projects[0].issues.len(),
            1,
            "alpha should still have 1 issue"
        );
        assert_eq!(
            app.projects[1].issues.len(),
            2,
            "beta should now have 2 issues"
        );
        assert!(
            app.projects[1]
                .issues
                .last()
                .unwrap()
                .id
                .starts_with("beta-"),
            "new issue should have beta prefix"
        );
    }

    #[test]
    fn delete_on_swimlane_deletes_from_correct_project() {
        let mut app = test_multi_app();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone(), beta_id.clone()];
        app.focused_swimlane = 1; // focus beta

        let ctx = app.action_context();
        // Select first issue in beta
        app.context_project_mut(&ctx).selected_column = 1; // InProgress
        app.context_project_mut(&ctx).selected_row[1] = 0;

        act(&mut app, Action::DeleteIssue);
        assert_eq!(app.input_mode, InputMode::Confirm);

        // Verify ConfirmAction has beta's project_id
        match app.pending_confirm.as_ref().unwrap() {
            ConfirmAction::DeleteIssue { project_id, .. } => {
                assert_eq!(*project_id, beta_id);
            }
            _ => panic!("expected DeleteIssue"),
        }
    }

    #[test]
    fn enter_collapses_to_single_swimlane() {
        let mut app = test_multi_app();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        let gamma_id = app.projects[2].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id, beta_id.clone(), gamma_id];

        assert_eq!(app.visible_swimlane_count(), 3);

        // Select beta in sidebar and press Enter
        app.sidebar.as_mut().unwrap().selected = 1;
        app.sidebar.as_mut().unwrap().focused = true;
        app.input_mode = InputMode::Sidebar;

        let post = handle_sidebar(&mut app, Action::SidebarSelect);
        match post {
            PostAction::SwitchProject { id } => {
                assert_eq!(id, beta_id);
            }
            _ => panic!("expected SwitchProject"),
        }

        // Swimlanes should be collapsed to just beta
        assert_eq!(app.sidebar.as_ref().unwrap().swimlanes, vec![beta_id]);
        assert_eq!(app.focused_swimlane, 0);
    }

    #[test]
    fn remove_middle_swimlane_adjusts_focus() {
        let mut app = test_multi_app();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        let gamma_id = app.projects[2].id();
        app.sidebar.as_mut().unwrap().swimlanes =
            vec![alpha_id.clone(), beta_id.clone(), gamma_id.clone()];
        app.focused_swimlane = 2; // focused on gamma (index 2)

        // Remove beta (index 1) via sidebar
        app.sidebar.as_mut().unwrap().selected = 1;
        handle_sidebar(&mut app, Action::SidebarToggleSwimlane);

        // Beta should be gone
        assert_eq!(app.sidebar.as_ref().unwrap().swimlanes.len(), 2);
        assert!(!app.sidebar.as_ref().unwrap().swimlanes.contains(&beta_id));

        // focused_swimlane was 2, removed at position 1, so it should shift to 1
        assert_eq!(app.focused_swimlane, 1);

        // Gamma should still be accessible
        let lanes = app.visible_swimlanes();
        assert_eq!(lanes[1], gamma_id);
    }

    #[test]
    fn pr_data_stays_per_project() {
        let mut app = test_multi_app();

        // Simulate PR data arriving for alpha only
        let pr = crate::types::PrStatus {
            number: 42,
            title: "Alpha PR".to_string(),
            url: "https://github.com/test/alpha/pull/42".to_string(),
            author: "testuser".to_string(),
            state: crate::types::PrState::Open,
            is_draft: false,
            checks: None,
            review: None,
            additions: 10,
            deletions: 5,
            head_branch: "feature".to_string(),
            is_cross_repository: false,
        };
        app.projects[0]
            .live
            .pr_statuses
            .insert("feature".to_string(), pr);

        // Alpha should have PR data
        assert_eq!(app.projects[0].live.pr_statuses.len(), 1);

        // Beta and gamma should NOT
        assert_eq!(app.projects[1].live.pr_statuses.len(), 0);
        assert_eq!(app.projects[2].live.pr_statuses.len(), 0);
    }

    // ================================================================
    // DebugReset: only sets should_quit, no side effects
    // ================================================================

    #[test]
    fn debug_reset_sets_should_quit_when_debug_enabled() {
        let mut config = test_config();
        config.debug = true;
        let state = crate::config::AppState::default();
        let mut app = App::new(config, state);
        assert!(!app.should_quit);

        act(&mut app, Action::DebugReset);

        assert!(app.should_quit);
    }

    #[test]
    fn debug_reset_is_noop_when_debug_disabled() {
        let mut app = test_app(); // debug: false
        assert!(!app.should_quit);

        act(&mut app, Action::DebugReset);

        assert!(!app.should_quit);
    }

    // ================================================================
    // Prune dialog
    // ================================================================

    /// App whose project root is a tempdir holding `names` as fake git
    /// worktrees (dir + `.git` file), plus a `main/` that must be excluded.
    /// The prune dialog discovers worktrees from disk, not the poll cache.
    fn test_app_with_worktrees(names: &[&str]) -> (tempfile::TempDir, App) {
        let dir = tempfile::TempDir::new().unwrap();
        for name in names.iter().chain(&["main"]) {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join(".git"), "gitdir: x").unwrap();
        }
        let mut config = test_config();
        config.project_root = dir.path().to_path_buf();
        let app = App::new(config, crate::config::AppState::default());
        (dir, app)
    }

    #[test]
    fn open_prune_dialog_with_no_candidates_shows_message() {
        let (_dir, mut app) = test_app_with_worktrees(&[]);
        act(&mut app, Action::OpenPruneDialog);
        // Only main/ on disk => no candidates => message, no dialog
        assert!(app.prune_dialog.is_none());
        assert!(app.input_mode == InputMode::Normal);
        assert!(app
            .message
            .as_ref()
            .is_some_and(|(m, _)| m.contains("No worktrees")));
    }

    #[test]
    fn open_prune_dialog_with_candidates_enters_dialog_mode() {
        let (_dir, mut app) = test_app_with_worktrees(&["wt-1"]);
        act(&mut app, Action::OpenPruneDialog);
        assert!(app.prune_dialog.is_some());
        assert_eq!(app.input_mode, InputMode::PruneDialog);
        // 'main' is excluded
        assert_eq!(app.prune_dialog.as_ref().unwrap().candidates.len(), 1);
    }

    #[test]
    fn open_prune_dialog_works_without_git_poll_data() {
        // Regression: with a cold poll cache (e.g. a project with hundreds
        // of worktrees whose first poll round hasn't finished), the dialog
        // must still list what's on disk instead of "No worktrees to prune".
        let (_dir, mut app) = test_app_with_worktrees(&["wt-1", "wt-2"]);
        assert!(app.project().live.worktree_branches.is_empty());
        act(&mut app, Action::OpenPruneDialog);
        let dialog = app.prune_dialog.as_ref().unwrap();
        assert_eq!(dialog.candidates.len(), 2);
        // Unknown status => conservative Keep defaults.
        for c in &dialog.candidates {
            assert!(c.status.is_none());
            assert_eq!(c.action, crate::prune::PruneAction::Keep);
        }
    }

    #[test]
    fn prune_cancel_closes_dialog() {
        let (_dir, mut app) = test_app_with_worktrees(&["wt-1"]);
        act(&mut app, Action::OpenPruneDialog);
        act(&mut app, Action::PruneCancel);
        assert!(app.prune_dialog.is_none());
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn prune_toggle_flips_action_for_selected_row() {
        let (_dir, mut app) = test_app_with_worktrees(&["wt-1"]);
        act(&mut app, Action::OpenPruneDialog);
        let before = app.prune_dialog.as_ref().unwrap().candidates[0].action;
        act(&mut app, Action::PruneToggle);
        let after = app.prune_dialog.as_ref().unwrap().candidates[0].action;
        assert_ne!(before, after);
    }

    #[test]
    fn prune_select_all_remove_and_keep() {
        let (_dir, mut app) = test_app_with_worktrees(&["wt-1", "wt-2"]);
        act(&mut app, Action::OpenPruneDialog);
        act(&mut app, Action::PruneSelectAllKeep);
        for c in &app.prune_dialog.as_ref().unwrap().candidates {
            assert_eq!(c.action, crate::prune::PruneAction::Keep);
        }
        act(&mut app, Action::PruneSelectAllRemove);
        for c in &app.prune_dialog.as_ref().unwrap().candidates {
            assert_eq!(c.action, crate::prune::PruneAction::Remove);
        }
    }

    #[test]
    fn prune_confirm_refuses_when_dirty_selected_for_removal() {
        let (_dir, mut app) = test_app_with_worktrees(&["wt-1"]);
        app.project_mut().live.worktree_statuses.insert(
            "wt-1".into(),
            crate::types::WorktreeStatus {
                staged: 1,
                unstaged: 0,
            },
        );
        act(&mut app, Action::OpenPruneDialog);
        // Force Remove despite dirty
        if let Some(d) = app.prune_dialog.as_mut() {
            d.candidates[0].action = crate::prune::PruneAction::Remove;
        }
        act(&mut app, Action::PruneConfirm);
        // Dialog stays open with an error
        assert!(app.prune_dialog.is_some());
        let err = app.prune_dialog.as_ref().unwrap().error.clone();
        assert!(err.is_some_and(|s| s.contains("dirty")));
    }

    #[test]
    fn prune_confirm_with_no_safe_removals_just_closes() {
        let (_dir, mut app) = test_app_with_worktrees(&["wt-1"]);
        act(&mut app, Action::OpenPruneDialog);
        act(&mut app, Action::PruneSelectAllKeep);
        act(&mut app, Action::PruneConfirm);
        assert!(app.prune_dialog.is_none());
        assert_eq!(app.input_mode, InputMode::Normal);
    }
}
