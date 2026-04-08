mod app;
mod config;
mod error;
mod external;
mod global_config;
mod handler;
mod init;
mod input;
mod lock;
mod types;
mod ui;
mod worktree;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, InputMode};
use handler::{ActionResult, PostAction};
use input::map_key_to_action;
use types::{AgentKind, AgentStatusInfo};

use external::git::GitPollResult;
use external::linear::LinearPollResult;
use external::ports::PortPollResult;
use types::PrStatus;

struct PrPollResult {
    prs: HashMap<String, PrStatus>,
    user_prs: Vec<PrStatus>,
    github_user: Option<String>,
}

const TICK_RATE: Duration = Duration::from_millis(50);
const TMUX_POLL_INTERVAL: Duration = Duration::from_secs(2);
const GIT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const PORT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const LINEAR_POLL_INTERVAL: Duration = Duration::from_secs(45);
const PR_POLL_INTERVAL: Duration = Duration::from_secs(60);

struct SessionPollResult {
    sessions: HashSet<String>,
    agent_statuses: HashMap<String, AgentStatusInfo>,
}

fn spawn_session_status_worker(status_dir: PathBuf) -> mpsc::Receiver<SessionPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let sessions = external::tmux::list_sessions();
        let agent_statuses = read_agent_statuses(&status_dir);
        let result = SessionPollResult {
            sessions,
            agent_statuses,
        };
        if tx.send(result).is_err() {
            break;
        }
        thread::sleep(TMUX_POLL_INTERVAL);
    });

    rx
}

fn spawn_port_poll_worker(sessions: Arc<Mutex<HashSet<String>>>) -> mpsc::Receiver<PortPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let sessions = sessions.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let ports = external::ports::poll_listening_ports(&sessions);
        if tx.send(PortPollResult { ports }).is_err() {
            break;
        }
        thread::sleep(PORT_POLL_INTERVAL);
    });

    rx
}

fn read_agent_statuses(status_dir: &Path) -> HashMap<String, AgentStatusInfo> {
    let mut statuses = HashMap::new();
    let entries = match std::fs::read_dir(status_dir) {
        Ok(e) => e,
        Err(_) => return statuses,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(session_name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(info) = serde_json::from_str::<AgentStatusInfo>(&contents) {
            statuses.insert(session_name.to_string(), info);
        }
    }
    statuses
}

fn spawn_git_status_worker(
    project_root: PathBuf,
    skip: Arc<Mutex<HashSet<String>>>,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<GitPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let skip_set = skip.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let result = external::git::poll_all_worktrees(&project_root, &skip_set);
        if tx.send(result).is_err() {
            break;
        }
        if !sleep_with_wake(&wake_rx, GIT_POLL_INTERVAL) {
            break;
        }
    });

    rx
}

/// Sleep until `interval` elapses or `wake_rx` signals.
/// Returns `false` if the wake channel disconnected (caller should exit).
fn sleep_with_wake(wake_rx: &mpsc::Receiver<()>, interval: Duration) -> bool {
    let deadline = Instant::now() + interval;
    loop {
        if Instant::now() >= deadline {
            return true;
        }
        match wake_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(()) => return true,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn spawn_linear_worker(wake_rx: mpsc::Receiver<()>) -> mpsc::Receiver<LinearPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let issues = external::linear::fetch_assigned_issues().unwrap_or_default();
        if tx.send(LinearPollResult { issues }).is_err() {
            break;
        }
        if !sleep_with_wake(&wake_rx, LINEAR_POLL_INTERVAL) {
            break;
        }
    });

    rx
}

fn spawn_pr_poll_worker(
    main_worktree: PathBuf,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<PrPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        // Run the 3 independent gh api calls in parallel
        let result = thread::scope(|s| {
            let prs_handle = s.spawn(|| {
                let prs = external::github::fetch_prs(&main_worktree);
                external::github::index_by_branch(prs)
            });
            let user_prs_handle = s.spawn(|| external::github::fetch_user_prs(&main_worktree));
            let user_handle = s.spawn(|| external::github::fetch_current_user(&main_worktree));

            PrPollResult {
                prs: prs_handle.join().unwrap_or_default(),
                user_prs: user_prs_handle.join().unwrap_or_default(),
                github_user: user_handle.join().ok().flatten(),
            }
        });
        if tx.send(result).is_err() {
            break;
        }
        if !sleep_with_wake(&wake_rx, PR_POLL_INTERVAL) {
            break;
        }
    });

    rx
}

#[derive(Parser)]
#[command(
    name = "bork",
    about = "Terminal kanban board for orchestrating coding sessions across git worktrees",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new bork project from a git repository
    Init {
        /// Git repository (owner/repo, HTTPS URL, or SSH URL)
        repo: String,

        /// Container directory name (defaults to repo name)
        directory: Option<String>,

        /// Agent kind
        #[arg(long, default_value = "opencode")]
        agent: AgentKindArg,
    },

    /// Install agent status hooks (OpenCode plugin + Claude Code hooks)
    Install,

    /// Remove agent status hooks
    Uninstall,

    /// Create a git worktree and register it with bork
    Worktree {
        /// Issue ID (e.g. bork-14)
        issue_id: String,

        /// Branch slug (e.g. add-search -> branch bork-14/add-search)
        slug: Option<String>,

        /// Create the issue if it doesn't exist (with this title)
        #[arg(long)]
        title: Option<String>,
    },

    /// Manage registered bork projects
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// List all registered projects
    List,

    /// Register a project (defaults to current directory)
    Add {
        /// Path to project container (must have .bork/ directory)
        path: Option<String>,
    },

    /// Unregister a project (defaults to current directory)
    Remove {
        /// Path to project container
        path: Option<String>,
    },
}

#[derive(Clone, ValueEnum)]
enum AgentKindArg {
    Opencode,
    Claude,
}

impl From<AgentKindArg> for AgentKind {
    fn from(arg: AgentKindArg) -> Self {
        match arg {
            AgentKindArg::Opencode => AgentKind::OpenCode,
            AgentKindArg::Claude => AgentKind::Claude,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Init {
            repo,
            directory,
            agent,
        }) => init::run_init(&repo, directory.as_deref(), agent.into(), None),
        Some(Command::Install) => external::hooks::install(),
        Some(Command::Uninstall) => external::hooks::uninstall(),
        Some(Command::Worktree {
            issue_id,
            slug,
            title,
        }) => worktree::run_worktree(&issue_id, slug.as_deref(), title.as_deref()),
        Some(Command::Project { command }) => run_project_command(command),
        None => run_tui(),
    }
}

fn run_project_command(command: ProjectCommand) -> anyhow::Result<()> {
    match command {
        ProjectCommand::List => {
            global_config::prune_stale_projects();
            let projects = global_config::list_projects();
            if projects.is_empty() {
                println!("No projects registered.");
                println!("Run 'bork init' or 'bork project add' to register a project.");
            } else {
                for entry in &projects {
                    println!("  {} ({})", entry.name, entry.path.display());
                }
            }
            Ok(())
        }
        ProjectCommand::Add { path } => {
            let target = match path {
                Some(p) => PathBuf::from(p),
                None => std::env::current_dir()?,
            };
            if !target.join(".bork").is_dir() {
                anyhow::bail!(
                    "No .bork/ directory found in {}. Run 'bork init' first.",
                    target.display()
                );
            }
            let config = config::load_config_from(&target);
            global_config::register_project(&config.project_name, &target)?;
            println!(
                "Registered project '{}' at {}",
                config.project_name,
                target.display()
            );
            Ok(())
        }
        ProjectCommand::Remove { path } => {
            let target = match path {
                Some(p) => PathBuf::from(p),
                None => std::env::current_dir()?,
            };
            let removed = global_config::unregister_project(&target)?;
            if removed {
                println!("Unregistered project at {}", target.display());
            } else {
                println!("No project registered at {}", target.display());
            }
            Ok(())
        }
    }
}

struct ProjectWorkers {
    session_rx: mpsc::Receiver<SessionPollResult>,
    port_rx: mpsc::Receiver<PortPollResult>,
    port_sessions: Arc<Mutex<HashSet<String>>>,
    git_rx: mpsc::Receiver<GitPollResult>,
    git_wake_tx: mpsc::Sender<()>,
    git_skip_set: Arc<Mutex<HashSet<String>>>,
    pr_rx: mpsc::Receiver<PrPollResult>,
    pr_wake_tx: mpsc::Sender<()>,
    linear_rx: Option<mpsc::Receiver<LinearPollResult>>,
    linear_wake_tx: mpsc::Sender<()>,
    linear_wake_rx: Option<mpsc::Receiver<()>>,
}

fn spawn_project_workers(project: &app::Project) -> ProjectWorkers {
    let project_root = project.config.project_root.clone();

    config::ensure_agent_status_dir(&project_root);

    let status_dir = config::agent_status_dir(&project_root);
    let session_rx = spawn_session_status_worker(status_dir);

    let port_sessions = Arc::new(Mutex::new(HashSet::<String>::new()));
    let port_rx = spawn_port_poll_worker(port_sessions.clone());

    let git_skip_set = Arc::new(Mutex::new(project.done_worktree_names()));
    let (git_wake_tx, git_wake_rx) = mpsc::channel::<()>();
    let git_rx = spawn_git_status_worker(project_root.clone(), git_skip_set.clone(), git_wake_rx);

    let (pr_wake_tx, pr_wake_rx) = mpsc::channel::<()>();
    let main_worktree = project_root.join("main");
    let pr_rx = spawn_pr_poll_worker(main_worktree, pr_wake_rx);

    let (linear_wake_tx, linear_wake_rx) = mpsc::channel::<()>();

    ProjectWorkers {
        session_rx,
        port_rx,
        port_sessions,
        git_rx,
        git_wake_tx,
        git_skip_set,
        pr_rx,
        pr_wake_tx,
        linear_rx: None,
        linear_wake_tx,
        linear_wake_rx: Some(linear_wake_rx),
    }
}

const ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(5);

fn spawn_activity_poller(projects: Vec<(usize, PathBuf)>) -> mpsc::Receiver<HashMap<usize, bool>> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let mut activity: HashMap<usize, bool> = HashMap::new();
        for (idx, root) in &projects {
            let status_dir = root.join(".bork").join("agent-status");
            let statuses = read_agent_statuses(&status_dir);
            let has_activity = statuses.values().any(|info| {
                matches!(
                    info.status,
                    types::AgentStatus::Busy
                        | types::AgentStatus::WaitingInput
                        | types::AgentStatus::WaitingPermission
                        | types::AgentStatus::WaitingApproval
                        | types::AgentStatus::Error
                )
            });
            activity.insert(*idx, has_activity);
        }
        if tx.send(activity).is_err() {
            break;
        }
        thread::sleep(ACTIVITY_POLL_INTERVAL);
    });

    rx
}

fn run_tui() -> anyhow::Result<()> {
    // --- Load config + state (before tmux wrap so we have project_name) ---
    let config = config::load_config();
    let state = config::load_state(&config.project_root);

    // Tmux auto-wrap: outer process creates session + attaches, inner runs TUI
    match external::tmux::ensure_bork_session(&config.project_name)? {
        external::tmux::EnsureResult::AlreadyInside => {}
        external::tmux::EnsureResult::Wrapped { exit_code } => {
            std::process::exit(exit_code);
        }
    }

    // --- Single-instance lock (only the inner/TUI process holds the lock) ---
    lock::acquire_lock(&config.project_root)?;

    // --- Panic hook ---
    let panic_project_root = config.project_root.clone();
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        lock::release_lock(&panic_project_root);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, SetTitle(""));
        original_hook(panic_info);
    }));

    // --- Signal handlers (SIGTERM, SIGHUP) ---
    lock::install_signal_handlers();

    // --- Terminal setup ---
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        SetTitle(format!("bork: {}", config.project_name))
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config, state);

    // --- Register current project and load others for multi-project sidebar ---
    global_config::prune_stale_projects();
    let current_root = app.project().config.project_root.clone();
    let _ = global_config::register_project(&app.project().config.project_name, &current_root);
    let current_canonical =
        std::fs::canonicalize(&current_root).unwrap_or_else(|_| current_root.clone());
    for entry in &global_config::load_global_config().projects {
        let canonical = std::fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone());
        if canonical == current_canonical || !entry.path.join(".bork").is_dir() {
            continue;
        }
        let proj_config = config::load_config_from(&entry.path);
        let proj_state = config::load_state(&entry.path);
        app.add_background_project(proj_config, proj_state);
    }
    app.enable_sidebar();

    // --- Workers ---
    let (action_tx, action_rx) = mpsc::channel::<ActionResult>();
    let mut workers = spawn_project_workers(app.project());

    // --- Activity poller for sidebar markers ---
    let activity_rx = if app.sidebar.is_some() {
        let project_paths: Vec<(usize, PathBuf)> = app
            .projects
            .iter()
            .enumerate()
            .map(|(i, p)| (i, p.config.project_root.clone()))
            .collect();
        Some(spawn_activity_poller(project_paths))
    } else {
        None
    };

    let (linear_check_tx, linear_check_rx) = mpsc::channel::<bool>();
    thread::spawn(move || {
        let available = external::linear::check_available();
        let _ = linear_check_tx.send(available);
    });

    let (tuicr_check_tx, tuicr_check_rx) = mpsc::channel::<bool>();
    thread::spawn(move || {
        let available = external::tuicr::check_available();
        let _ = tuicr_check_tx.send(available);
    });

    let mut pending_popup_session: Option<(String, String)> = None;
    let mut pending_popup_for_launch: Option<(usize, String)> = None;
    let mut needs_redraw = true;

    // --- Main event loop ---
    loop {
        if needs_redraw {
            terminal.draw(|frame| {
                ui::render(frame, &app);
            })?;
            needs_redraw = false;
        }

        if event::poll(TICK_RATE)? {
            loop {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        needs_redraw = true;
                        let dialog_field = app.dialog.as_ref().map(|d| d.current_field());
                        let action = map_key_to_action(
                            key,
                            app.input_mode,
                            dialog_field,
                            app.visible_swimlane_count(),
                        );
                        let post_action = handler::handle_action(
                            &mut app,
                            action,
                            &action_tx,
                            &workers.pr_wake_tx,
                            &workers.linear_wake_tx,
                            &workers.git_wake_tx,
                        );

                        match post_action {
                            PostAction::None => {}
                            PostAction::OpenTmuxPopup {
                                session_name,
                                popup_title,
                            } => {
                                if app.project().state_dirty {
                                    let _ = config::save_state(
                                        &app.project().to_state(),
                                        &app.project().config.project_root,
                                    );
                                    app.project_mut().state_dirty = false;
                                }
                                open_tmux_popup(
                                    &mut terminal,
                                    &session_name,
                                    &popup_title,
                                    &app.project().config.project_name,
                                )?;
                                app.message = None;
                            }
                            PostAction::LaunchAndOpenPopup {
                                issue_index,
                                popup_title,
                            } => {
                                pending_popup_for_launch = Some((issue_index, popup_title));
                            }
                            PostAction::OpenEditor { initial_content } => {
                                if let Some(edited) = open_external_editor(
                                    &mut terminal,
                                    &initial_content,
                                    &app.project().config.project_name,
                                )? {
                                    if let Some(dialog) = app.dialog.as_mut() {
                                        dialog.set_prompt_text(&edited);
                                    }
                                }
                            }
                            PostAction::SwitchProject { index } => {
                                if index < app.projects.len() && index != app.focused_project {
                                    app.dialog = None;
                                    app.linear_picker = None;
                                    app.confirm_message = None;
                                    app.pending_confirm = None;
                                    app.debug_inspector_json = None;
                                    app.input_mode = InputMode::Normal;
                                    if app.project().state_dirty {
                                        let _ = config::save_state(
                                            &app.project().to_state(),
                                            &app.project().config.project_root,
                                        );
                                        app.project_mut().state_dirty = false;
                                    }

                                    app.focused_project = index;

                                    workers = spawn_project_workers(app.project());
                                    let _ = execute!(
                                        terminal.backend_mut(),
                                        SetTitle(format!(
                                            "bork: {}",
                                            app.project().config.project_name
                                        ))
                                    );
                                    app.set_message(format!(
                                        "Switched to {}",
                                        app.project().config.project_name
                                    ));
                                }
                            }
                        }
                    }
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        if app.should_quit || lock::signal_received() {
            break;
        }

        while let Ok(result) = action_rx.try_recv() {
            needs_redraw = true;
            app.busy_count = app.busy_count.saturating_sub(1);
            app.set_message(result.message);

            if let Some((issue_id, agent_sid)) = result.session_id {
                if let Some(issue) = app
                    .project_mut()
                    .issues
                    .iter_mut()
                    .find(|i| i.id == issue_id)
                {
                    issue.session_id = Some(agent_sid);
                }
            }

            if let Some(session_name) = result.session_to_open {
                if let Some(popup_title) = result.popup_title {
                    // Direct popup (e.g. OpenTerminal)
                    pending_popup_session = Some((session_name, popup_title));
                } else if let Some((launch_idx, popup_title)) = pending_popup_for_launch.take() {
                    // Agent session launch
                    if app.project().issues[launch_idx].column == types::Column::Todo {
                        app.project_mut().issues[launch_idx].column = types::Column::InProgress;
                    }
                    pending_popup_session = Some((session_name, popup_title));
                }
            }
        }

        if let Some((session_name, popup_title)) = pending_popup_session.take() {
            // Flush state before yielding terminal to tmux popup (could last a long time)
            let _ = config::save_state(
                &app.project().to_state(),
                &app.project().config.project_root,
            );
            app.project_mut().state_dirty = false;
            open_tmux_popup(
                &mut terminal,
                &session_name,
                &popup_title,
                &app.project().config.project_name,
            )?;
            app.message = None;
            needs_redraw = true;
        }

        while let Ok(poll) = workers.session_rx.try_recv() {
            needs_redraw = true;
            let live = app.project_mut().live_mut();
            live.active_sessions = poll.sessions;
            live.agent_statuses = poll.agent_statuses;
            // Update shared sessions set for the port poll worker
            if let Ok(mut shared) = workers.port_sessions.lock() {
                *shared = live.active_sessions.clone();
            }
        }

        while let Ok(port_result) = workers.port_rx.try_recv() {
            app.project_mut().live_mut().listening_ports = port_result.ports;
        }

        // --- Auto-kill Done sessions past TTL ---
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cleanup_indices = app.project().issues_needing_session_cleanup(now);
        for idx in cleanup_indices {
            needs_redraw = true;
            let session_name =
                app.project().issues[idx].session_name(&app.project().config.project_name);
            let status_file = config::agent_status_dir(&app.project().config.project_root)
                .join(format!("{}.json", session_name));
            let sn = session_name.clone();
            app.project_mut()
                .live_mut()
                .active_sessions
                .remove(&session_name);
            thread::spawn(move || {
                let _ = external::tmux::kill_session(&sn);
                let _ = std::fs::remove_file(&status_file);
            });
            app.set_message(format!("Auto-killed session '{}' (done TTL)", session_name));
        }

        let mut git_data_changed = false;
        while let Ok(git_result) = workers.git_rx.try_recv() {
            needs_redraw = true;
            git_data_changed = true;
            let live = app.project_mut().live_mut();
            live.worktree_statuses = git_result.statuses;
            live.worktree_branches = git_result.branches;
            live.git_poll_done = true;
        }

        let mut pr_data_changed = false;
        while let Ok(pr_result) = workers.pr_rx.try_recv() {
            needs_redraw = true;
            pr_data_changed = true;
            let live = app.project_mut().live_mut();
            live.pr_statuses = pr_result.prs;
            live.user_prs = pr_result.user_prs;
            if pr_result.github_user.is_some() {
                live.github_user = pr_result.github_user;
            }

            let p = &mut app.projects[app.focused_project];
            let pr_titles: Vec<(u32, String)> = p
                .live()
                .pr_statuses
                .values()
                .map(|pr| (pr.number, pr.title.clone()))
                .collect();
            for issue in &mut p.issues {
                if let Some(pr_num) = issue.pr_number {
                    if issue.pr_imported {
                        if let Some((_, title)) = pr_titles.iter().find(|(n, _)| *n == pr_num) {
                            issue.title = title.clone();
                        }
                    }
                }
            }
        }

        // --- Auto-import open PRs as issues (only when new PR data arrived) ---
        if pr_data_changed {
            let (changed, msg) = app.project_mut().sync_prs_as_issues();
            if let Some(m) = msg {
                app.set_message(m);
            }
            if changed {
                app.project_mut().mark_dirty();
            }
        }

        // --- Auto-assign worktrees (only when git data changed) ---
        if git_data_changed {
            let mut worktree_changed = app.project_mut().auto_assign_worktrees();
            worktree_changed = app.project_mut().clear_stale_worktrees() || worktree_changed;
            if worktree_changed {
                let _ = workers.git_wake_tx.send(());
                app.project_mut().mark_dirty();
            }
        }

        // --- Update git skip set when issues changed columns or git data arrived ---
        if git_data_changed || app.project().state_dirty {
            if let Ok(mut skip) = workers.git_skip_set.lock() {
                *skip = app.project().done_worktree_names();
            }
        }

        // --- tuicr: check availability ---
        if let Ok(true) = tuicr_check_rx.try_recv() {
            app.project_mut().tuicr_available = true;
        }

        // --- Linear: check availability then consume poll results ---
        if let Ok(true) = linear_check_rx.try_recv() {
            needs_redraw = true;
            app.project_mut().linear_available = true;
            if let Some(wake_rx) = workers.linear_wake_rx.take() {
                workers.linear_rx = Some(spawn_linear_worker(wake_rx));
            }
        }
        if let Some(ref rx) = workers.linear_rx {
            while let Ok(result) = rx.try_recv() {
                needs_redraw = true;
                app.project_mut().live_mut().linear_issues = result.issues;
                let p = &mut app.projects[app.focused_project];
                let linear_titles: Vec<(String, String)> = p
                    .live()
                    .linear_issues
                    .iter()
                    .map(|i| (i.id.clone(), i.title.clone()))
                    .collect();
                for issue in &mut p.issues {
                    if let Some(ref lid) = issue.linear_id {
                        if issue.linear_imported {
                            if let Some((_, title)) = linear_titles.iter().find(|(id, _)| id == lid)
                            {
                                issue.title = title.clone();
                            }
                        }
                    }
                }
            }
        }

        if let Some(ref rx) = activity_rx {
            while let Ok(activity) = rx.try_recv() {
                if let Some(ref mut sidebar) = app.sidebar {
                    if sidebar.activity != activity {
                        sidebar.activity = activity;
                        needs_redraw = true;
                    }
                }
            }
        }

        if app.busy_count > 0 {
            app.spinner_tick = app.spinner_tick.wrapping_add(1);
            needs_redraw = true;
        }

        // --- Flush dirty state to disk (once per tick, not per action) ---
        if app.project().state_dirty {
            let _ = config::save_state(
                &app.project().to_state(),
                &app.project().config.project_root,
            );
            app.project_mut().state_dirty = false;
        }

        if app.clear_expired_message() {
            needs_redraw = true;
        }
    }

    let _ = config::save_state(
        &app.project().to_state(),
        &app.project().config.project_root,
    );
    lock::release_lock(&app.project().config.project_root);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, SetTitle(""))?;

    Ok(())
}

fn open_tmux_popup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session_name: &str,
    title: &str,
    project_name: &str,
) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let _ = external::tmux::open_popup(session_name, title);

    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        SetTitle(format!("bork: {}", project_name))
    )?;
    terminal.clear()?;

    Ok(())
}

fn open_external_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    initial_content: &str,
    project_name: &str,
) -> anyhow::Result<Option<String>> {
    let Some((editor_cmd, editor_args)) = resolve_editor() else {
        return Err(anyhow::anyhow!("No editor found. Set $EDITOR or $VISUAL."));
    };

    let temp_path = std::env::temp_dir().join(format!(".bork-edit-{}.md", std::process::id()));
    fs::write(&temp_path, initial_content)?;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let status = StdCommand::new(&editor_cmd)
        .args(&editor_args)
        .arg(&temp_path)
        .status();

    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        SetTitle(format!("bork: {}", project_name))
    )?;
    terminal.clear()?;

    let result = match status {
        Ok(s) if s.success() => fs::read_to_string(&temp_path).ok(),
        _ => None,
    };
    let _ = fs::remove_file(&temp_path);

    Ok(result)
}

fn resolve_editor() -> Option<(String, Vec<String>)> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                let mut parts = trimmed.split_whitespace();
                let cmd = parts.next().unwrap().to_string();
                let args: Vec<String> = parts.map(String::from).collect();
                return Some((cmd, args));
            }
        }
    }
    for name in ["vim", "nvim", "vi", "nano"] {
        if StdCommand::new("which")
            .arg(name)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Some((name.to_string(), vec![]));
        }
    }
    None
}
