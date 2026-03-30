mod app;
mod config;
mod error;
mod external;
mod handler;
mod init;
mod input;
mod types;
mod ui;
mod worktree;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;
use handler::{ActionResult, PostAction};
use input::map_key_to_action;
use types::{AgentKind, AgentStatusInfo};

use external::git::GitPollResult;
use external::linear::LinearPollResult;
use types::PrStatus;

const TICK_RATE: Duration = Duration::from_millis(50);
const TMUX_POLL_INTERVAL: Duration = Duration::from_secs(2);
const GIT_POLL_INTERVAL: Duration = Duration::from_secs(3);
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

fn read_agent_statuses(status_dir: &PathBuf) -> HashMap<String, AgentStatusInfo> {
    let mut statuses = HashMap::new();
    let entries = match std::fs::read_dir(status_dir) {
        Ok(e) => e,
        Err(_) => return statuses,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "json") {
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
) -> mpsc::Receiver<GitPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let skip_set = skip.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let result = external::git::poll_all_worktrees(&project_root, &skip_set);
        if tx.send(result).is_err() {
            break;
        }
        thread::sleep(GIT_POLL_INTERVAL);
    });

    rx
}

fn spawn_linear_worker() -> mpsc::Receiver<LinearPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let issues = match external::linear::fetch_assigned_issues() {
            Ok(issues) => issues,
            Err(_) => Vec::new(),
        };
        if tx.send(LinearPollResult { issues }).is_err() {
            break;
        }
        thread::sleep(LINEAR_POLL_INTERVAL);
    });

    rx
}

/// PR poll worker with wake-up support for force-sync.
fn spawn_pr_poll_worker(
    main_worktree: PathBuf,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<HashMap<String, PrStatus>> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let prs = external::github::fetch_prs(&main_worktree);
        let indexed = external::github::index_by_branch(prs);
        if tx.send(indexed).is_err() {
            break;
        }

        let deadline = Instant::now() + PR_POLL_INTERVAL;
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match wake_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
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
        None => run_tui(),
    }
}

fn run_tui() -> anyhow::Result<()> {
    // --- Load config + state (before tmux wrap so we have project_name) ---
    let config = config::load_config();
    let state = config::load_state(&config.project_root);

    // --- Tmux auto-wrap (scoped to project name) ---
    match external::tmux::ensure_bork_session(&config.project_name)? {
        external::tmux::EnsureResult::AlreadyInside => {}
        external::tmux::EnsureResult::Wrapped { exit_code } => {
            std::process::exit(exit_code);
        }
    }

    // --- Panic hook ---
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    // --- Terminal setup ---
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config, state);

    // --- Ensure agent-status dir ---
    config::ensure_agent_status_dir(&app.config.project_root);

    // --- Channels ---
    let (action_tx, action_rx) = mpsc::channel::<ActionResult>();
    let status_dir = config::agent_status_dir(&app.config.project_root);
    let session_rx = spawn_session_status_worker(status_dir);
    let git_skip_set = Arc::new(Mutex::new(app.done_worktree_names()));
    let git_rx = spawn_git_status_worker(app.config.project_root.clone(), git_skip_set.clone());
    let (pr_wake_tx, pr_wake_rx) = mpsc::channel::<()>();
    let main_worktree = app.config.project_root.join("main");
    let pr_rx = spawn_pr_poll_worker(main_worktree, pr_wake_rx);

    // Linear: non-blocking availability check, poll worker starts only if CLI is found
    let (linear_check_tx, linear_check_rx) = mpsc::channel::<bool>();
    thread::spawn(move || {
        let available = external::linear::check_available();
        let _ = linear_check_tx.send(available);
    });
    let mut linear_rx: Option<mpsc::Receiver<LinearPollResult>> = None;

    let mut pending_popup_session: Option<String> = None;
    let mut pending_popup_for_launch: Option<usize> = None;

    // --- Main event loop ---
    loop {
        terminal.draw(|frame| {
            ui::render(frame, &app);
        })?;

        if event::poll(TICK_RATE)? {
            loop {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        let action = map_key_to_action(key, app.input_mode);
                        let post_action =
                            handler::handle_action(&mut app, action, &action_tx, &pr_wake_tx);

                        match post_action {
                            PostAction::None => {}
                            PostAction::OpenTmuxPopup { session_name } => {
                                open_tmux_popup(&mut terminal, &session_name)?;
                                app.message = None;
                            }
                            PostAction::LaunchAndOpenPopup { issue_index } => {
                                pending_popup_for_launch = Some(issue_index);
                            }
                        }
                    }
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        if app.should_quit {
            break;
        }

        while let Ok(result) = action_rx.try_recv() {
            app.busy_count = app.busy_count.saturating_sub(1);
            app.set_message(result.message);

            if let Some((issue_id, agent_sid)) = result.session_id {
                if let Some(issue) = app.issues.iter_mut().find(|i| i.id == issue_id) {
                    issue.session_id = Some(agent_sid);
                }
            }

            if let Some(session_name) = result.session_to_open {
                if let Some(launch_idx) = pending_popup_for_launch.take() {
                    app.issues[launch_idx].tmux_session = Some(session_name.clone());
                    if app.issues[launch_idx].column == types::Column::Todo {
                        app.issues[launch_idx].column = types::Column::InProgress;
                    }
                    pending_popup_session = Some(session_name);
                }
            }
        }

        if let Some(session_name) = pending_popup_session.take() {
            let _ = config::save_state(&app.to_state(), &app.config.project_root);
            open_tmux_popup(&mut terminal, &session_name)?;
            app.message = None;
        }

        while let Ok(poll) = session_rx.try_recv() {
            app.active_sessions = poll.sessions;
            app.agent_statuses = poll.agent_statuses;
        }

        // --- Auto-kill Done sessions past TTL ---
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cleanup_indices = app.issues_needing_session_cleanup(now);
        for idx in cleanup_indices {
            let session_name = app.issues[idx].session_name(&app.config.project_name);
            let status_file = config::agent_status_dir(&app.config.project_root)
                .join(format!("{}.json", session_name));
            let sn = session_name.clone();
            // Remove from active set immediately so we don't re-fire on next tick
            app.active_sessions.remove(&session_name);
            thread::spawn(move || {
                let _ = external::tmux::kill_session(&sn);
                let _ = std::fs::remove_file(&status_file);
            });
            app.set_message(format!("Auto-killed session '{}' (done TTL)", session_name));
        }

        while let Ok(git_result) = git_rx.try_recv() {
            app.worktree_statuses = git_result.statuses;
            app.worktree_branches = git_result.branches;
        }

        while let Ok(pr_result) = pr_rx.try_recv() {
            app.pr_statuses = pr_result;
        }

        // --- Auto-assign worktrees for issues that don't have one ---
        let mut worktree_changed = app.auto_assign_worktrees();
        worktree_changed = app.clear_stale_worktrees() || worktree_changed;
        if worktree_changed {
            let _ = config::save_state(&app.to_state(), &app.config.project_root);
        }

        // --- Update git skip set for Done worktrees ---
        if let Ok(mut skip) = git_skip_set.lock() {
            *skip = app.done_worktree_names();
        }

        // --- Linear: check availability then consume poll results ---
        if let Ok(true) = linear_check_rx.try_recv() {
            app.linear_available = true;
            linear_rx = Some(spawn_linear_worker());
        }
        if let Some(ref rx) = linear_rx {
            while let Ok(result) = rx.try_recv() {
                app.linear_issues = result.issues;
                // Refresh metadata for issues already on the board
                for issue in &mut app.issues {
                    if let Some(ref lid) = issue.linear_id {
                        if let Some(fresh) = app.linear_issues.iter().find(|i| i.id == *lid) {
                            issue.linear_state = Some(fresh.state_name.clone());
                            issue.title = fresh.title.clone();
                        }
                    }
                }
            }
        }

        if app.busy_count > 0 {
            app.spinner_tick = app.spinner_tick.wrapping_add(1);
        }

        app.clear_expired_message();
    }

    let _ = config::save_state(&app.to_state(), &app.config.project_root);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    Ok(())
}

fn open_tmux_popup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session_name: &str,
) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let _ = external::tmux::open_popup(session_name);

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;

    Ok(())
}
