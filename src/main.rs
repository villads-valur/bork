mod agent_config;
mod app;
mod config;
mod dialog_state;
mod error;
mod external;
mod global_config;
mod handler;
mod init;
mod input;
mod lock;
mod ops;
mod toml_lite;
mod types;
mod ui;
mod update;
mod worktree;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{
        self, Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, InputMode, ProjectId};
use global_config::ReloadResult;
use handler::{ActionResult, PostAction};
use input::map_key_to_action;
use types::{AgentKind, AgentMode, AgentStatusInfo, Column, IssueKind};

use external::git::GitPollResult;
use external::linear::LinearPollResult;
use external::ports::PortPollResult;
use types::PrStatus;

struct PrPollResult {
    prs: HashMap<String, PrStatus>,
    user_prs: Vec<PrStatus>,
    review_requested_prs: Vec<PrStatus>,
    github_user: Option<String>,
}

/// Kitty keyboard protocol flags we negotiate so Ghostty/kitty/foot/WezTerm/recent
/// Alacritty report Shift+Enter (and other modified keys) as distinct events instead
/// of collapsing them to plain Enter. Terminals without support silently ignore them.
const KITTY_KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;

const TICK_RATE: Duration = Duration::from_millis(50);
const TMUX_POLL_INTERVAL: Duration = Duration::from_secs(2);
const GIT_POLL_INTERVAL: Duration = Duration::from_secs(5);
// `lsof -iTCP` scans every process and routinely takes 100ms+ on macOS, so the
// port poll runs at a slower cadence than the other pollers.
const PORT_POLL_INTERVAL: Duration = Duration::from_secs(10);
const LINEAR_POLL_INTERVAL: Duration = Duration::from_secs(45);
const PR_POLL_INTERVAL: Duration = Duration::from_secs(60);
const STATE_POLL_TICKS: usize = 40; // 40 * 50ms = 2s

/// Sleep while polling is suspended (e.g. a tmux popup owns the terminal).
/// Keeps workers from spawning subprocesses nobody will consume.
fn wait_while_suspended(suspended: &AtomicBool) {
    while suspended.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }
}

/// Single shared `tmux list-sessions` poller. Tmux sessions are server-global,
/// so one worker serves every project/swimlane instead of N identical polls.
fn spawn_tmux_session_worker(
    suspended: Arc<AtomicBool>,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<HashSet<String>> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        wait_while_suspended(&suspended);
        let sessions = external::tmux::list_sessions();
        if tx.send(sessions).is_err() {
            break;
        }
        if !sleep_with_wake(&wake_rx, TMUX_POLL_INTERVAL) {
            break;
        }
    });

    rx
}

/// Per-project poller for the agent status files in `.bork/agent-status/`.
fn spawn_agent_status_worker(
    status_dir: PathBuf,
    suspended: Arc<AtomicBool>,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<HashMap<String, AgentStatusInfo>> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        wait_while_suspended(&suspended);
        let agent_statuses = read_agent_statuses(&status_dir);
        if tx.send(agent_statuses).is_err() {
            break;
        }
        if !sleep_with_wake(&wake_rx, TMUX_POLL_INTERVAL) {
            break;
        }
    });

    rx
}

fn spawn_port_poll_worker(
    sessions: Arc<Mutex<HashSet<String>>>,
    suspended: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> mpsc::Receiver<PortPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        wait_while_suspended(&suspended);
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
    suspended: Arc<AtomicBool>,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<GitPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        wait_while_suspended(&suspended);
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
    match wake_rx.recv_timeout(interval) {
        Ok(()) => {
            // Drain queued wakes so mashing a refresh key triggers one
            // poll round, not N back-to-back rounds.
            while wake_rx.try_recv().is_ok() {}
            true
        }
        Err(mpsc::RecvTimeoutError::Timeout) => true,
        Err(mpsc::RecvTimeoutError::Disconnected) => false,
    }
}

fn spawn_linear_worker(
    suspended: Arc<AtomicBool>,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<LinearPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        wait_while_suspended(&suspended);
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
    suspended: Arc<AtomicBool>,
    wake_rx: mpsc::Receiver<()>,
) -> mpsc::Receiver<PrPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        wait_while_suspended(&suspended);
        // Run the 4 independent gh api calls in parallel
        let result = thread::scope(|s| {
            let prs_handle = s.spawn(|| {
                let prs = external::github::fetch_prs(&main_worktree);
                external::github::index_by_branch(prs)
            });
            let user_prs_handle = s.spawn(|| external::github::fetch_user_prs(&main_worktree));
            let review_handle =
                s.spawn(|| external::github::fetch_review_requested_prs(&main_worktree));
            let user_handle = s.spawn(|| external::github::fetch_current_user(&main_worktree));

            PrPollResult {
                prs: prs_handle.join().unwrap_or_default(),
                user_prs: user_prs_handle.join().unwrap_or_default(),
                review_requested_prs: review_handle.join().unwrap_or_default(),
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

/// Quickstart block shown before Options on the top-level `bork --help`.
/// Aimed at AI agents driving bork from the CLI: where to start, plus the
/// --agent / --mode knobs. Kept accurate against external/opencode.rs and
/// config.rs.
const AGENTS_START_HERE: &str = "\
Start here (for AI agents):
  bork issue list                List the kanban board (use --json to parse)
  bork issue create \"<title>\"    Add an issue (its agent runs in a worktree)
  bork issue start \"<title>\"     Create issue + worktree + agent in one step
                                 (--no-worktree to skip it, --base <branch> to
                                  pick the worktree's base; defaults to main)
  bork worktree <id> <slug>      Create the git worktree for an issue
                                 (--base <branch> to pick its base branch)

  Each issue runs one coding agent. Pick which and how with:
    --agent   opencode (default), claude, codex     # must be on your PATH
    --mode    plan (read-only), build, yolo          # yolo: claude/codex only

  Defaults + allowlist live in ~/.config/bork/config.toml
  (default_agent, agents = [...]); per-project override in .bork/config.toml.";

/// One-line pointer shown before Options on every subcommand `--help`, so the
/// quickstart is discoverable at any level without repeating the full block.
const AGENTS_POINTER: &str =
    "Agents: run 'bork --help' for the AI-agent quickstart (--agent / --mode).";

/// Build a help template that injects `agents_block` between the usage line and
/// the auto-generated argument/command listing. clap only substitutes its own
/// tags (`{all-args}`, `{usage}`, ...), so the agents text is interpolated here
/// rather than left as a placeholder. Mirrors Harbor's "Start here" layout:
/// the block sits up top, before Commands and Options.
fn help_template_with_agents(agents_block: &str) -> String {
    format!(
        "{{about-with-newline}}\n\
         {{usage-heading}} {{usage}}\n\n\
         {agents_block}\n\n\
         {{all-args}}{{after-help}}"
    )
}

/// Apply the agents help template to a command and, recursively, to all of its
/// subcommands. The root gets the full quickstart; every nested command gets
/// the one-line pointer.
fn apply_agents_help(cmd: clap::Command, agents_block: &str) -> clap::Command {
    cmd.help_template(help_template_with_agents(agents_block))
        .mut_subcommands(|sub| apply_agents_help(sub, AGENTS_POINTER))
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

    /// Install agent status hooks (OpenCode/Pi plugins + Claude Code/Codex hooks)
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

        /// Branch to base the new worktree on (defaults to main/master)
        #[arg(long)]
        base: Option<String>,
    },

    /// Manage registered bork projects
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },

    /// Manage issues on the board
    Issue {
        #[command(subcommand)]
        command: IssueCommand,
    },

    /// Manage integrations (Linear, GitHub PRs)
    Integration {
        #[command(subcommand)]
        command: IntegrationCommand,
    },

    /// Update bork to the latest version (git pull + cargo build)
    Update {
        /// Only check whether a new version is available; don't pull or build.
        /// Refreshes the update cache so a running TUI picks up the result
        /// within seconds.
        #[arg(long)]
        check: bool,
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

#[derive(Subcommand)]
enum IssueCommand {
    /// List all issues
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Filter by column (todo, in-progress, code-review, done)
        #[arg(long, value_parser = parse_column)]
        column: Option<Column>,
    },

    /// Create a new issue
    Create {
        /// Issue title
        title: String,

        /// Column to place in (todo, in-progress, code-review, done)
        #[arg(long, value_parser = parse_column)]
        column: Option<Column>,

        /// Agent kind (opencode, claude, codex, pi)
        #[arg(long, value_parser = parse_agent_kind)]
        agent: Option<AgentKind>,

        /// Agent mode (plan, build, yolo)
        #[arg(long, value_parser = parse_agent_mode)]
        mode: Option<AgentMode>,

        /// Prompt text for the agent
        #[arg(long)]
        prompt: Option<String>,

        /// Issue kind (agentic, todo)
        #[arg(long, value_parser = parse_issue_kind)]
        kind: Option<IssueKind>,
    },

    /// Create an issue, create a worktree, and start its agent session
    Start {
        /// Issue title
        title: String,

        /// Prompt text for the agent
        #[arg(long)]
        prompt: Option<String>,

        /// Agent kind (opencode, claude, codex)
        #[arg(long, value_parser = parse_agent_kind)]
        agent: Option<AgentKind>,

        /// Agent mode (plan, build, yolo). Defaults to build.
        #[arg(long, value_parser = parse_agent_mode)]
        mode: Option<AgentMode>,

        /// Branch/worktree slug. Defaults to a slug generated from the title.
        #[arg(long)]
        slug: Option<String>,

        /// Skip creating a git worktree before launching the agent
        #[arg(long)]
        no_worktree: bool,

        /// Branch to base the new worktree on (defaults to main/master)
        #[arg(long)]
        base: Option<String>,

        /// Project name or path to start the issue in. Defaults to current project.
        #[arg(long)]
        project: Option<String>,
    },

    /// Update an existing issue
    Update {
        /// Issue ID (e.g. bork-1)
        id: String,

        /// New title
        #[arg(long)]
        title: Option<String>,

        /// Move to column (todo, in-progress, code-review, done)
        #[arg(long, value_parser = parse_column)]
        column: Option<Column>,

        /// Change agent kind (opencode, claude, codex, pi)
        #[arg(long, value_parser = parse_agent_kind)]
        agent: Option<AgentKind>,

        /// Change agent mode (plan, build, yolo)
        #[arg(long, value_parser = parse_agent_mode)]
        mode: Option<AgentMode>,

        /// Update prompt text (empty string clears it)
        #[arg(long)]
        prompt: Option<String>,
    },

    /// Delete an issue
    Delete {
        /// Issue ID (e.g. bork-1)
        id: String,
    },

    /// Show issue details
    Show {
        /// Issue ID (e.g. bork-1)
        id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Move an issue to a column
    Move {
        /// Issue ID (e.g. bork-1)
        id: String,

        /// Target column (todo, in-progress, code-review, done)
        #[arg(value_parser = parse_column)]
        column: Column,
    },
}

#[derive(Subcommand)]
enum IntegrationCommand {
    /// Link a Linear ticket to an issue
    AttachLinear {
        /// Issue ID (e.g. bork-1)
        issue_id: String,

        /// Linear issue identifier (e.g. VIL-123)
        linear_identifier: String,
    },

    /// Link a GitHub PR to an issue
    AttachPr {
        /// Issue ID (e.g. bork-1)
        issue_id: String,

        /// GitHub PR number
        pr_number: u32,
    },
}

fn parse_column(s: &str) -> Result<Column, String> {
    match s.to_lowercase().as_str() {
        "todo" | "to-do" | "to_do" => Ok(Column::Todo),
        "in-progress" | "in_progress" | "inprogress" => Ok(Column::InProgress),
        "code-review" | "code_review" | "codereview" | "review" => Ok(Column::CodeReview),
        "done" => Ok(Column::Done),
        _ => Err(format!(
            "Unknown column '{}'. Options: todo, in-progress, code-review, done",
            s
        )),
    }
}

fn parse_agent_kind(s: &str) -> Result<AgentKind, String> {
    AgentKind::parse(s).ok_or_else(|| {
        format!(
            "Unknown agent '{}'. Options: opencode, claude, codex, pi",
            s
        )
    })
}

fn parse_agent_mode(s: &str) -> Result<AgentMode, String> {
    match s.to_lowercase().as_str() {
        "plan" => Ok(AgentMode::Plan),
        "build" => Ok(AgentMode::Build),
        "yolo" => Ok(AgentMode::Yolo),
        _ => Err(format!("Unknown mode '{}'. Options: plan, build, yolo", s)),
    }
}

fn parse_issue_kind(s: &str) -> Result<IssueKind, String> {
    match s.to_lowercase().as_str() {
        "agentic" => Ok(IssueKind::Agentic),
        "todo" | "non-agentic" | "nonagentic" => Ok(IssueKind::NonAgentic),
        _ => Err(format!(
            "Unknown issue kind '{}'. Options: agentic, todo",
            s
        )),
    }
}

#[derive(Clone, ValueEnum)]
enum AgentKindArg {
    Opencode,
    Claude,
    Codex,
    Pi,
}

impl From<AgentKindArg> for AgentKind {
    fn from(arg: AgentKindArg) -> Self {
        match arg {
            AgentKindArg::Opencode => AgentKind::OpenCode,
            AgentKindArg::Claude => AgentKind::Claude,
            AgentKindArg::Codex => AgentKind::Codex,
            AgentKindArg::Pi => AgentKind::Pi,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let command = apply_agents_help(Cli::command(), AGENTS_START_HERE);
    let matches = command.get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

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
            base,
        }) => worktree::run_worktree(
            &issue_id,
            slug.as_deref(),
            title.as_deref(),
            base.as_deref(),
        ),
        Some(Command::Project { command }) => run_project_command(command),
        Some(Command::Issue { command }) => run_issue_command(command),
        Some(Command::Integration { command }) => run_integration_command(command),
        Some(Command::Update { check }) => {
            if check {
                update::run_check_command()
            } else {
                update::run_update()
            }
        }
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
            if !target.join(".bork").join("config.toml").exists() {
                anyhow::bail!(
                    "No bork project found in {}. Run 'bork init' first.",
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

fn run_issue_command(command: IssueCommand) -> anyhow::Result<()> {
    let project_root = config::find_project_root();

    match command {
        IssueCommand::List { json, column } => {
            let output = ops::list_issues(&project_root, &ops::ListOptions { column, json })?;
            println!("{}", output);
            Ok(())
        }
        IssueCommand::Create {
            title,
            column,
            agent,
            mode,
            prompt,
            kind,
        } => {
            let issue = ops::create_issue(
                &project_root,
                ops::CreateOptions {
                    title,
                    column,
                    agent_kind: agent,
                    agent_mode: mode,
                    prompt,
                    kind,
                },
            )?;
            println!("Created {}: \"{}\"", issue.id, issue.title);
            Ok(())
        }
        IssueCommand::Start {
            title,
            prompt,
            agent,
            mode,
            slug,
            no_worktree,
            base,
            project,
        } => {
            let project_root = resolve_start_project_root(project.as_deref())?;
            let report = start_issue(
                &project_root,
                StartIssueOptions {
                    title,
                    prompt,
                    agent_kind: agent,
                    agent_mode: mode,
                    slug,
                    no_worktree,
                    base_branch: base,
                },
            )?;
            println!("Started {}: \"{}\"", report.issue_id, report.title);
            if let Some(worktree_dir) = report.worktree_dir {
                println!("Worktree: {}/", worktree_dir);
            }
            println!("Session:  {}", report.session_name);
            println!("Attach:   tmux attach -t {}", report.session_name);
            Ok(())
        }
        IssueCommand::Update {
            id,
            title,
            column,
            agent,
            mode,
            prompt,
        } => {
            let issue = ops::update_issue(
                &project_root,
                &id,
                ops::UpdateOptions {
                    title,
                    column,
                    agent_kind: agent,
                    agent_mode: mode,
                    prompt,
                },
            )?;
            println!("Updated {}: \"{}\"", issue.id, issue.title);
            Ok(())
        }
        IssueCommand::Delete { id } => {
            let issue = ops::delete_issue(&project_root, &id)?;
            println!("Deleted {}: \"{}\"", issue.id, issue.title);
            Ok(())
        }
        IssueCommand::Show { id, json } => {
            let output = ops::show_issue(&project_root, &id, json)?;
            println!("{}", output);
            Ok(())
        }
        IssueCommand::Move { id, column } => {
            let issue = ops::move_issue(&project_root, &id, column)?;
            println!("Moved {} to {}", issue.id, issue.column);
            Ok(())
        }
    }
}

struct StartIssueOptions {
    title: String,
    prompt: Option<String>,
    agent_kind: Option<AgentKind>,
    agent_mode: Option<AgentMode>,
    slug: Option<String>,
    no_worktree: bool,
    base_branch: Option<String>,
}

struct StartIssueReport {
    issue_id: String,
    title: String,
    worktree_dir: Option<String>,
    session_name: String,
}

fn resolve_start_project_root(project: Option<&str>) -> anyhow::Result<PathBuf> {
    let Some(project) = project else {
        return Ok(config::find_project_root());
    };

    let project_path = Path::new(project);
    if project_path.exists() {
        if let Some(root) = find_project_root_from(project_path) {
            return Ok(root);
        }
    }

    global_config::prune_stale_projects();
    global_config::list_projects()
        .into_iter()
        .find(|entry| entry.name == project)
        .map(|entry| entry.path)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found", project))
}

fn find_project_root_from(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_file() { path.parent()? } else { path };

    loop {
        if dir.join(".bork").is_dir() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

fn start_issue(project_root: &Path, opts: StartIssueOptions) -> anyhow::Result<StartIssueReport> {
    let config = config::load_config_from(project_root);
    let mut issue = ops::create_issue(
        project_root,
        ops::CreateOptions {
            title: opts.title.clone(),
            column: None,
            agent_kind: opts.agent_kind,
            agent_mode: Some(opts.agent_mode.unwrap_or(AgentMode::Build)),
            prompt: opts.prompt,
            kind: Some(IssueKind::Agentic),
        },
    )?;

    let worktree_dir = if opts.no_worktree {
        None
    } else {
        let slug = opts
            .slug
            .unwrap_or_else(|| worktree::slugify_title(&opts.title));
        let result = worktree::create_worktree_in(
            &config,
            &issue.id,
            Some(&slug),
            None,
            opts.base_branch.as_deref(),
        )?;
        issue.worktree = Some(result.worktree_dir.clone());
        Some(result.worktree_dir)
    };

    let (session_name, agent_session_id) = external::opencode::launch_session(&issue, &config)
        .map_err(|e| anyhow::anyhow!("Failed to launch agent: {e}"))?;

    // Reload state so we don't clobber concurrent updates that happened during launch
    let mut state = config::load_state(project_root);
    if let Some(saved) = state.issues.iter_mut().find(|i| i.id == issue.id) {
        if saved.column == Column::Todo {
            saved.column = Column::InProgress;
        }
        if let Some(sid) = agent_session_id {
            saved.session_id = Some(sid);
        }
    }
    config::save_state(&state, project_root)?;

    Ok(StartIssueReport {
        issue_id: issue.id,
        title: issue.title,
        worktree_dir,
        session_name,
    })
}

fn run_integration_command(command: IntegrationCommand) -> anyhow::Result<()> {
    let project_root = config::find_project_root();

    match command {
        IntegrationCommand::AttachLinear {
            issue_id,
            linear_identifier,
        } => {
            let issue = ops::attach_linear(&project_root, &issue_id, &linear_identifier)?;
            println!(
                "Linked {} to Linear {}",
                issue.id,
                issue.linear_identifier.as_deref().unwrap_or("?")
            );
            Ok(())
        }
        IntegrationCommand::AttachPr {
            issue_id,
            pr_number,
        } => {
            let issue = ops::attach_pr(&project_root, &issue_id, pr_number)?;
            println!("Linked {} to PR #{}", issue.id, pr_number);
            Ok(())
        }
    }
}

struct SharedWorkers {
    tmux_rx: mpsc::Receiver<HashSet<String>>,
    tmux_wake_tx: mpsc::Sender<()>,
    port_rx: mpsc::Receiver<PortPollResult>,
    port_sessions: Arc<Mutex<HashSet<String>>>,
    linear_rx: Option<mpsc::Receiver<LinearPollResult>>,
    linear_wake_tx: mpsc::Sender<()>,
    linear_wake_rx: Option<mpsc::Receiver<()>>,
    shutdown: Arc<AtomicBool>,
    /// While set, all pollers idle instead of spawning subprocesses. Used when
    /// the terminal is handed over to a tmux popup or external editor.
    poll_suspended: Arc<AtomicBool>,
}

struct ProjectWorkers {
    session_rx: mpsc::Receiver<HashMap<String, AgentStatusInfo>>,
    session_wake_tx: mpsc::Sender<()>,
    git_rx: mpsc::Receiver<GitPollResult>,
    git_wake_tx: mpsc::Sender<()>,
    git_skip_set: Arc<Mutex<HashSet<String>>>,
    pr_rx: mpsc::Receiver<PrPollResult>,
    pr_wake_tx: mpsc::Sender<()>,
}

fn spawn_shared_workers() -> SharedWorkers {
    let shutdown = Arc::new(AtomicBool::new(false));
    let poll_suspended = Arc::new(AtomicBool::new(false));

    let (tmux_wake_tx, tmux_wake_rx) = mpsc::channel::<()>();
    let tmux_rx = spawn_tmux_session_worker(poll_suspended.clone(), tmux_wake_rx);

    let port_sessions = Arc::new(Mutex::new(HashSet::<String>::new()));
    let port_rx = spawn_port_poll_worker(
        port_sessions.clone(),
        poll_suspended.clone(),
        shutdown.clone(),
    );

    let (linear_wake_tx, linear_wake_rx) = mpsc::channel::<()>();

    SharedWorkers {
        tmux_rx,
        tmux_wake_tx,
        port_rx,
        port_sessions,
        linear_rx: None,
        linear_wake_tx,
        linear_wake_rx: Some(linear_wake_rx),
        shutdown,
        poll_suspended,
    }
}

fn spawn_project_workers(project: &app::Project, suspended: &Arc<AtomicBool>) -> ProjectWorkers {
    let project_root = project.config.project_root.clone();

    let status_dir = config::agent_status_dir(&project_root);
    let (session_wake_tx, session_wake_rx) = mpsc::channel::<()>();
    let session_rx = spawn_agent_status_worker(status_dir, suspended.clone(), session_wake_rx);

    let git_skip_set = Arc::new(Mutex::new(project.done_worktree_names()));
    let (git_wake_tx, git_wake_rx) = mpsc::channel::<()>();
    let git_rx = spawn_git_status_worker(
        project_root.clone(),
        git_skip_set.clone(),
        suspended.clone(),
        git_wake_rx,
    );

    let (pr_wake_tx, pr_wake_rx) = mpsc::channel::<()>();
    let main_worktree = project_root.join("main");
    let pr_rx = spawn_pr_poll_worker(main_worktree, suspended.clone(), pr_wake_rx);

    ProjectWorkers {
        session_rx,
        session_wake_tx,
        git_rx,
        git_wake_tx,
        git_skip_set,
        pr_rx,
        pr_wake_tx,
    }
}

const ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(5);

fn spawn_activity_poller(
    projects: Vec<(ProjectId, PathBuf)>,
    suspended: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> mpsc::Receiver<HashMap<ProjectId, bool>> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        wait_while_suspended(&suspended);
        let mut activity: HashMap<ProjectId, bool> = HashMap::new();
        for (id, root) in &projects {
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
            activity.insert(id.clone(), has_activity);
        }
        if tx.send(activity).is_err() {
            break;
        }
        thread::sleep(ACTIVITY_POLL_INTERVAL);
    });

    rx
}

fn run_tui() -> anyhow::Result<()> {
    // --- Determine which project to focus ---
    global_config::prune_stale_projects();
    let local_root = config::find_project_root();
    let has_local_project = local_root.join(".bork").join("config.toml").exists();

    let config;
    let state;

    if has_local_project {
        config = config::load_config_from(&local_root);
        state = config::load_state(&local_root);
        config::ensure_agent_status_dir(&config.project_root);
    } else {
        let registered = global_config::list_projects();
        if registered.is_empty() {
            anyhow::bail!(
                "No bork project found. Run 'bork init <repo>' to create one, \
                 or 'bork project add <path>' to register an existing project."
            );
        }
        let entry = &registered[0];
        config = config::load_config_from(&entry.path);
        state = config::load_state(&entry.path);
        config::ensure_agent_status_dir(&config.project_root);
    }

    // Tmux auto-wrap: use a dedicated session name that can't collide with project names
    match external::tmux::ensure_bork_session(external::tmux::BORK_TUI_SESSION)? {
        external::tmux::EnsureResult::AlreadyInside => {}
        external::tmux::EnsureResult::Wrapped { exit_code } => {
            std::process::exit(exit_code);
        }
    }

    // --- Single-instance lock (global, not per-project) ---
    let lock_root = global_config::global_config_dir();
    lock::acquire_lock(&lock_root)?;

    // --- Panic hook ---
    let panic_lock_root = lock_root.clone();
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        lock::release_lock(&panic_lock_root);
        pop_kitty_flags(&mut io::stdout());
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
    push_kitty_flags(&mut stdout);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config, state);

    // One-time warning if user still has the legacy agents.toml lying around.
    agent_config::warn_if_legacy_agents_file();

    // Resolve available agents from layered config + PATH detection.
    let agent_selection =
        agent_config::resolve_agent_selection(Some(&app.project().config.project_root));
    app.set_available_agents(agent_selection.available, agent_selection.default_agent);

    // --- Register current project and load others for multi-project sidebar ---
    let current_root = app.project().config.project_root.clone();
    let _ = global_config::register_if_absent(&app.project().config.project_name, &current_root);
    let current_canonical =
        std::fs::canonicalize(&current_root).unwrap_or_else(|_| current_root.clone());
    for entry in &global_config::load_global_config().projects {
        let canonical = std::fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone());
        if canonical == current_canonical || !entry.path.join(".bork").join("config.toml").exists()
        {
            continue;
        }
        let proj_config = config::load_config_from(&entry.path);
        let proj_state = config::load_state(&entry.path);
        app.add_background_project(proj_config, proj_state);
    }
    app.enable_sidebar();

    // Clean up tmp files left behind by writers that crashed mid-save.
    for project in &app.projects {
        config::sweep_stale_tmp_files(&project.config.project_root);
    }

    // --- Workers ---
    let (action_tx, action_rx) = mpsc::channel::<ActionResult>();
    let (reload_tx, reload_rx) = mpsc::channel::<ReloadResult>();
    let mut shared = spawn_shared_workers();
    let mut workers = spawn_project_workers(app.project(), &shared.poll_suspended);
    let mut swimlane_workers: HashMap<ProjectId, ProjectWorkers> = HashMap::new();

    // --- Activity poller for sidebar markers ---
    let activity_rx = if app.sidebar.is_some() {
        let project_paths: Vec<(ProjectId, PathBuf)> = app
            .projects
            .iter()
            .map(|p| (p.id(), p.config.project_root.clone()))
            .collect();
        Some(spawn_activity_poller(
            project_paths,
            shared.poll_suspended.clone(),
            shared.shutdown.clone(),
        ))
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

    let (update_check_tx, update_check_rx) = mpsc::channel::<bool>();
    let update_shutdown = shared.shutdown.clone();
    thread::spawn(move || {
        // Long-lived worker: re-checks every CHECK_INTERVAL_SECS so users who
        // keep bork open for days still see new-version banners. Sleeps in 1s
        // slices to stay responsive to shutdown.
        loop {
            if update_check_tx.send(update::check_for_update()).is_err() {
                return;
            }
            for _ in 0..update::CHECK_INTERVAL_SECS {
                if update_shutdown.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }
        }
    });

    // Cheap polling of the update cache file's mtime: when `bork update --check`
    // runs in a second terminal it rewrites the cache, and we pick up the new
    // result here within seconds without waiting for the 6h periodic worker.
    let mut last_update_cache_mtime = update::cache_mtime_secs();

    let mut pending_popup_session: Option<(String, String)> = None;
    // Launches in flight, keyed by issue ID so concurrent launches can't get
    // their results crossed: (project_id, popup_title, open_popup).
    let mut pending_popup_for_launch: HashMap<String, (ProjectId, String, bool)> = HashMap::new();
    let mut needs_redraw = true;
    let mut state_poll_counter: usize = 0;

    // --- Main event loop ---
    loop {
        if needs_redraw {
            terminal.draw(|frame| {
                ui::render(frame, &app);
            })?;
            needs_redraw = false;
        }

        match event::poll(TICK_RATE) {
            Ok(false) => {}
            Err(_) => break, // Terminal broken (e.g. tmux killed)
            Ok(true) => loop {
                let event = match event::read() {
                    Ok(e) => e,
                    Err(_) => {
                        app.should_quit = true;
                        break;
                    }
                };
                if let Event::Resize(..) = event {
                    needs_redraw = true;
                }
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        needs_redraw = true;
                        let dialog_field = app.dialog.as_ref().map(|d| d.current_field());
                        let action = map_key_to_action(
                            key,
                            app.input_mode,
                            dialog_field,
                            app.visible_swimlane_count(),
                        );
                        let ctx = app.action_context();
                        // Route wakes to the active swimlane's workers, not the
                        // focused project's: refresh actions in a swimlane must
                        // wake that lane's pollers.
                        let active_workers =
                            swimlane_workers.get(&ctx.project_id).unwrap_or(&workers);
                        let ch = handler::ActionChannels {
                            action_tx: &action_tx,
                            pr_wake_tx: &active_workers.pr_wake_tx,
                            linear_wake_tx: &shared.linear_wake_tx,
                            git_wake_tx: &active_workers.git_wake_tx,
                            reload_tx: &reload_tx,
                        };
                        let post_action = handler::handle_action(&mut app, action, &ctx, &ch);

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
                                    app.project_mut().update_base_snapshot();
                                }
                                open_tmux_popup(
                                    &mut terminal,
                                    &session_name,
                                    &popup_title,
                                    &app.project().config.project_name,
                                    &shared.poll_suspended,
                                )?;
                                app.message = None;
                            }
                            PostAction::LaunchAndOpenPopup {
                                issue_id,
                                popup_title,
                                open_popup,
                            } => {
                                pending_popup_for_launch.insert(
                                    issue_id,
                                    (app.active_project_id(), popup_title, open_popup),
                                );
                            }
                            PostAction::OpenEditor { initial_content } => {
                                if let Some(edited) = open_external_editor(
                                    &mut terminal,
                                    &initial_content,
                                    &app.project().config.project_name,
                                    &shared.poll_suspended,
                                )? {
                                    if let Some(dialog) = app.dialog.as_mut() {
                                        dialog.set_prompt_text(&edited);
                                    }
                                }
                            }
                            PostAction::SwitchProject { id } => {
                                if app.find_project(&id).is_some() && id != app.focused_project {
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
                                        app.project_mut().update_base_snapshot();
                                    }

                                    let old_focused = app.focused_project.clone();
                                    app.focused_project = id.clone();
                                    app.focused_swimlane = 0;

                                    let old_workers =
                                        if let Some(existing) = swimlane_workers.remove(&id) {
                                            std::mem::replace(&mut workers, existing)
                                        } else {
                                            std::mem::replace(
                                                &mut workers,
                                                spawn_project_workers(
                                                    app.project(),
                                                    &shared.poll_suspended,
                                                ),
                                            )
                                        };

                                    let still_swimlane = app
                                        .sidebar
                                        .as_ref()
                                        .is_some_and(|s| s.swimlanes.contains(&old_focused));
                                    if still_swimlane {
                                        swimlane_workers.insert(old_focused, old_workers);
                                    }
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
                match event::poll(Duration::ZERO) {
                    Ok(true) => continue,
                    _ => break,
                }
            },
        }

        if app.should_quit || lock::signal_received() {
            break;
        }

        let mut action_results_arrived = false;
        while let Ok(result) = action_rx.try_recv() {
            needs_redraw = true;
            action_results_arrived = true;
            app.busy_count = app.busy_count.saturating_sub(1);
            app.show_message(result.message, result.message_kind);

            if let Some((issue_id, agent_sid)) = result.session_id {
                for project in &mut app.projects {
                    if let Some(issue) = project.issues.iter_mut().find(|i| i.id == issue_id) {
                        issue.session_id = Some(agent_sid);
                        project.mark_dirty();
                        break;
                    }
                }
            }

            if let Some(launch_id) = result.launched_issue_id {
                app.launches_in_flight.remove(&launch_id);
                let pending = pending_popup_for_launch.remove(&launch_id);
                // Only act on a successful launch; failures already surfaced
                // their error message above.
                if let Some(session_name) = result.session_to_open {
                    if let Some((proj_id, popup_title, open_popup)) = pending {
                        if let Some(project) = app.find_project_mut(&proj_id) {
                            if let Some(issue) =
                                project.issues.iter_mut().find(|i| i.id == launch_id)
                            {
                                if issue.column == types::Column::Todo {
                                    issue.column = types::Column::InProgress;
                                    project.mark_dirty();
                                }
                            }
                        }
                        if open_popup {
                            pending_popup_session = Some((session_name, popup_title));
                        }
                    }
                }
            } else if let Some(session_name) = result.session_to_open {
                if let Some(popup_title) = result.popup_title {
                    pending_popup_session = Some((session_name, popup_title));
                }
            }
        }

        // Completed actions usually changed tmux session state (launch, kill,
        // terminal). Wake the session pollers so cards update within ms, not
        // a full 2s poll interval.
        if action_results_arrived {
            let _ = shared.tmux_wake_tx.send(());
            let _ = workers.session_wake_tx.send(());
            for sw in swimlane_workers.values() {
                let _ = sw.session_wake_tx.send(());
            }
        }

        if let Some((session_name, popup_title)) = pending_popup_session.take() {
            // Flush state before yielding terminal to tmux popup (could last a long time)
            let _ = config::save_state(
                &app.project().to_state(),
                &app.project().config.project_root,
            );
            app.project_mut().state_dirty = false;
            app.project_mut().update_base_snapshot();
            open_tmux_popup(
                &mut terminal,
                &session_name,
                &popup_title,
                &app.project().config.project_name,
                &shared.poll_suspended,
            )?;
            app.message = None;
            needs_redraw = true;
        }

        // --- Sync swimlane workers ---
        if let Some(ref sidebar) = app.sidebar {
            let active_swimlanes: HashSet<ProjectId> = sidebar
                .swimlanes
                .iter()
                .filter(|id| **id != app.focused_project)
                .cloned()
                .collect();
            for id in &active_swimlanes {
                if !swimlane_workers.contains_key(id) {
                    if let Some(project) = app.find_project(id) {
                        swimlane_workers.insert(
                            id.clone(),
                            spawn_project_workers(project, &shared.poll_suspended),
                        );
                    }
                }
            }
            swimlane_workers.retain(|id, _| active_swimlanes.contains(id));
        } else {
            swimlane_workers.clear();
        }

        // Drain all queued poll results but only redraw when data changed:
        // workers send every interval regardless, and undiffed assignment
        // would rebuild the whole widget tree every 2s while idle.
        let mut sessions_changed = false;

        // Shared: tmux sessions are server-global, distribute to all projects.
        while let Ok(sessions) = shared.tmux_rx.try_recv() {
            for project in &mut app.projects {
                if project.live.active_sessions != sessions {
                    project.live.active_sessions = sessions.clone();
                    sessions_changed = true;
                    needs_redraw = true;
                }
            }
        }

        while let Ok(statuses) = workers.session_rx.try_recv() {
            let live = &mut app.project_mut().live;
            if live.agent_statuses != statuses {
                live.agent_statuses = statuses;
                needs_redraw = true;
            }
        }

        // --- Shared: port data (distributed to all projects) ---
        while let Ok(port_result) = shared.port_rx.try_recv() {
            for project in &mut app.projects {
                if project.live.listening_ports != port_result.ports {
                    project.live.listening_ports = port_result.ports.clone();
                    needs_redraw = true;
                }
            }
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
            app.project_mut().live.active_sessions.remove(&session_name);
            thread::spawn(move || {
                let _ = external::tmux::kill_session(&sn);
                let _ = std::fs::remove_file(&status_file);
            });
            app.set_message(format!("Auto-killed session '{}' (done TTL)", session_name));
        }

        let mut git_data_changed = false;
        while let Ok(git_result) = workers.git_rx.try_recv() {
            let live = &mut app.project_mut().live;
            // The first poll must always register (sets git_poll_done) even
            // when the data matches the empty default.
            if !live.git_poll_done
                || live.worktree_statuses != git_result.statuses
                || live.worktree_branches != git_result.branches
            {
                live.worktree_statuses = git_result.statuses;
                live.worktree_branches = git_result.branches;
                live.git_poll_done = true;
                git_data_changed = true;
                needs_redraw = true;
            }
        }

        let mut pr_data_changed = false;
        while let Ok(pr_result) = workers.pr_rx.try_recv() {
            let live = &mut app.project_mut().live;
            let changed = !live.pr_poll_done
                || live.pr_statuses != pr_result.prs
                || live.user_prs != pr_result.user_prs
                || live.review_requested_prs != pr_result.review_requested_prs;
            if pr_result.github_user.is_some() && live.github_user != pr_result.github_user {
                live.github_user = pr_result.github_user;
                needs_redraw = true;
            }
            if !changed {
                continue;
            }
            needs_redraw = true;
            pr_data_changed = true;
            live.pr_statuses = pr_result.prs;
            live.user_prs = pr_result.user_prs;
            live.review_requested_prs = pr_result.review_requested_prs;
            live.pr_poll_done = true;

            let p = app.project_mut();
            let pr_titles: Vec<(u32, String)> = p
                .live
                .pr_statuses
                .values()
                .chain(p.live.review_requested_prs.iter())
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

        // --- Auto-assign worktrees ---
        // Runs when git data changed OR when issues changed (state_dirty):
        // a freshly created issue whose worktree already exists must be
        // assigned even if the git poll data is identical. Gated on
        // git_poll_done so an empty pre-poll branch map can't wipe worktrees.
        if git_data_changed || (app.project().state_dirty && app.project().live.git_poll_done) {
            let mut worktree_changed = app.project_mut().auto_assign_worktrees();
            worktree_changed = app.project_mut().clear_stale_worktrees() || worktree_changed;
            if worktree_changed {
                let _ = workers.git_wake_tx.send(());
                app.project_mut().mark_dirty();
            }
        }

        // --- Update git skip set when issues changed columns or git data arrived ---
        if git_data_changed || app.project().state_dirty {
            let mut skip = workers
                .git_skip_set
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *skip = app.project().done_worktree_names();
        }

        // --- Update check (periodic worker results) ---
        // The `bork update --check` cache-mtime poll lives in the 2s state
        // poll block below; no need to stat the file every 50ms tick.
        let mut new_update_available: Option<bool> = None;
        while let Ok(available) = update_check_rx.try_recv() {
            new_update_available = Some(available);
        }
        if let Some(available) = new_update_available {
            if app.update_available != available {
                app.update_available = available;
                needs_redraw = true;
            }
        }

        // --- tuicr: check availability ---
        if let Ok(true) = tuicr_check_rx.try_recv() {
            for p in &mut app.projects {
                p.tuicr_available = true;
            }
        }

        // --- Linear: check availability then consume poll results (shared) ---
        if let Ok(true) = linear_check_rx.try_recv() {
            needs_redraw = true;
            for p in &mut app.projects {
                p.linear_available = true;
            }
            if let Some(wake_rx) = shared.linear_wake_rx.take() {
                shared.linear_rx =
                    Some(spawn_linear_worker(shared.poll_suspended.clone(), wake_rx));
            }
        }
        if let Some(ref rx) = shared.linear_rx {
            while let Ok(result) = rx.try_recv() {
                needs_redraw = true;
                for project in &mut app.projects {
                    project.live.linear_issues = result.issues.clone();
                    let linear_titles: Vec<(String, String)> = project
                        .live
                        .linear_issues
                        .iter()
                        .map(|i| (i.id.clone(), i.title.clone()))
                        .collect();
                    for issue in &mut project.issues {
                        if issue.is_any_linear_imported() {
                            if let Some(first_link) = issue.linear_links.first() {
                                if let Some((_, title)) =
                                    linear_titles.iter().find(|(id, _)| id == &first_link.id)
                                {
                                    issue.title = title.clone();
                                }
                            }
                        }
                    }
                }
            }
        }

        // --- Drain swimlane workers (per-project: session, git, pr only) ---
        let sw_ids: Vec<ProjectId> = swimlane_workers.keys().cloned().collect();
        for proj_id in &sw_ids {
            let Some(sw) = swimlane_workers.get(proj_id) else {
                continue;
            };
            let Some(proj_pos) = app.projects.iter().position(|p| p.id() == *proj_id) else {
                continue;
            };
            while let Ok(statuses) = sw.session_rx.try_recv() {
                let live = &mut app.projects[proj_pos].live;
                if live.agent_statuses != statuses {
                    live.agent_statuses = statuses;
                    needs_redraw = true;
                }
            }
            let mut sw_git_changed = false;
            while let Ok(git_result) = sw.git_rx.try_recv() {
                let live = &mut app.projects[proj_pos].live;
                if !live.git_poll_done
                    || live.worktree_statuses != git_result.statuses
                    || live.worktree_branches != git_result.branches
                {
                    live.worktree_statuses = git_result.statuses;
                    live.worktree_branches = git_result.branches;
                    live.git_poll_done = true;
                    sw_git_changed = true;
                    needs_redraw = true;
                }
            }
            let sw_state_dirty =
                app.projects[proj_pos].state_dirty && app.projects[proj_pos].live.git_poll_done;
            if sw_git_changed || sw_state_dirty {
                let changed = app.projects[proj_pos].auto_assign_worktrees();
                let stale = app.projects[proj_pos].clear_stale_worktrees();
                if changed || stale {
                    app.projects[proj_pos].mark_dirty();
                }
                let mut skip = sw.git_skip_set.lock().unwrap_or_else(|e| e.into_inner());
                *skip = app.projects[proj_pos].done_worktree_names();
            }
            let mut sw_pr_changed = false;
            while let Ok(pr_result) = sw.pr_rx.try_recv() {
                let live = &mut app.projects[proj_pos].live;
                if pr_result.github_user.is_some() && live.github_user != pr_result.github_user {
                    live.github_user = pr_result.github_user;
                    needs_redraw = true;
                }
                if !live.pr_poll_done
                    || live.pr_statuses != pr_result.prs
                    || live.user_prs != pr_result.user_prs
                    || live.review_requested_prs != pr_result.review_requested_prs
                {
                    live.pr_statuses = pr_result.prs;
                    live.user_prs = pr_result.user_prs;
                    live.review_requested_prs = pr_result.review_requested_prs;
                    live.pr_poll_done = true;
                    sw_pr_changed = true;
                    needs_redraw = true;
                }
            }
            if sw_pr_changed {
                let (changed, msg) = app.projects[proj_pos].sync_prs_as_issues();
                if let Some(m) = msg {
                    app.set_message(m);
                }
                if changed {
                    app.projects[proj_pos].mark_dirty();
                }
            }
        }

        // Rebuild shared port sessions, but only when session data actually
        // changed; no need to take the lock 20x/sec against the port worker.
        if sessions_changed {
            let mut port_sess = shared
                .port_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            port_sess.clear();
            for project in &app.projects {
                port_sess.extend(project.live.active_sessions.iter().cloned());
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

        while let Ok(result) = reload_rx.try_recv() {
            if !result.new_projects.is_empty() {
                app.apply_reload_result(result);
                needs_redraw = true;
            }
        }

        if app.is_busy_visible() {
            app.spinner_tick = app.spinner_tick.wrapping_add(1);
            // The spinner advances one frame every 2 ticks; redraw only when
            // the visible frame actually changes.
            if app.spinner_tick.is_multiple_of(2) {
                needs_redraw = true;
            }
        }
        if app.tick_busy_visibility() {
            needs_redraw = true;
        }

        // --- Detect external state.json changes (every ~2s) ---
        state_poll_counter += 1;
        if state_poll_counter >= STATE_POLL_TICKS {
            state_poll_counter = 0;

            // Pick up `bork update --check` runs from other terminals.
            let mtime = update::cache_mtime_secs();
            if mtime != last_update_cache_mtime {
                last_update_cache_mtime = mtime;
                let available = update::cached_update_available();
                if app.update_available != available {
                    app.update_available = available;
                    needs_redraw = true;
                }
            }

            for project in &mut app.projects {
                let current_mtime = config::state_mtime(&project.config.project_root);
                if current_mtime != project.last_state_mtime {
                    // Skip the merge when the file is unreadable/corrupt:
                    // merging a defaulted empty state would wipe the board.
                    if let Some(new_state) = config::try_load_state(&project.config.project_root) {
                        project.merge_external_state(new_state);
                        // Issues created externally (e.g. `bork issue create`)
                        // may already have a matching worktree on disk.
                        if project.live.git_poll_done {
                            let changed = project.auto_assign_worktrees();
                            let stale = project.clear_stale_worktrees();
                            if changed || stale {
                                project.mark_dirty();
                            }
                        }
                    }
                    project.last_state_mtime = current_mtime;
                    needs_redraw = true;
                }
            }
        }

        // --- Flush dirty state to disk (once per tick, not per action) ---
        for project in &mut app.projects {
            if project.state_dirty {
                let _ = config::save_state(&project.to_state(), &project.config.project_root);
                project.state_dirty = false;
                project.update_base_snapshot();
            }
        }

        if app.clear_expired_message() {
            needs_redraw = true;
        }
    }

    shared.shutdown.store(true, Ordering::Relaxed);

    for project in &app.projects {
        if project.state_dirty {
            let _ = config::save_state(&project.to_state(), &project.config.project_root);
        }
    }
    lock::release_lock(&lock_root);

    pop_kitty_flags(terminal.backend_mut());
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, SetTitle(""))?;

    Ok(())
}

/// Best-effort: ignored by terminals that don't support the kitty keyboard protocol.
fn push_kitty_flags<W: io::Write>(out: &mut W) {
    let _ = execute!(out, PushKeyboardEnhancementFlags(KITTY_KEYBOARD_FLAGS));
}

fn pop_kitty_flags<W: io::Write>(out: &mut W) {
    let _ = execute!(out, PopKeyboardEnhancementFlags);
}

/// RAII guard that pauses worker polling for its lifetime. Used while the
/// terminal is handed over to a tmux popup or external editor, so workers
/// don't keep spawning subprocesses and queueing results nobody consumes.
struct SuspendGuard<'a>(&'a AtomicBool);

impl<'a> SuspendGuard<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Relaxed);
        SuspendGuard(flag)
    }
}

impl Drop for SuspendGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

fn open_tmux_popup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session_name: &str,
    title: &str,
    project_name: &str,
    suspended: &AtomicBool,
) -> anyhow::Result<()> {
    let _guard = SuspendGuard::new(suspended);
    pop_kitty_flags(terminal.backend_mut());
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let _ = external::tmux::open_popup(session_name, title);

    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        SetTitle(format!("bork: {}", project_name))
    )?;
    push_kitty_flags(terminal.backend_mut());
    terminal.clear()?;

    Ok(())
}

fn open_external_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    initial_content: &str,
    project_name: &str,
    suspended: &AtomicBool,
) -> anyhow::Result<Option<String>> {
    let _guard = SuspendGuard::new(suspended);
    let Some((editor_cmd, editor_args)) = resolve_editor() else {
        return Err(anyhow::anyhow!("No editor found. Set $EDITOR or $VISUAL."));
    };

    let temp_path = std::env::temp_dir().join(format!(".bork-edit-{}.md", std::process::id()));
    fs::write(&temp_path, initial_content)?;

    pop_kitty_flags(terminal.backend_mut());
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
    push_kitty_flags(terminal.backend_mut());
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
        if agent_config::command_exists(name) {
            return Some((name.to_string(), vec![]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The wrapper tmux session name must never collide with agent session names.
    // Agent sessions follow the pattern "{project_name}-{issue_id}" where
    // issue_id is "{project_name}-{number}" (e.g. "bork-bork-1", "myapp-myapp-42").

    #[test]
    fn tui_session_name_does_not_match_any_project_agent_pattern() {
        let project_names = ["bork", "myapp", "tui", "bork-tui", "test"];
        for name in project_names {
            for n in 1..=100 {
                let agent_session = format!("{}-{}-{}", name, name, n);
                assert_ne!(
                    external::tmux::BORK_TUI_SESSION,
                    agent_session,
                    "wrapper session '{}' collides with agent session '{}'",
                    external::tmux::BORK_TUI_SESSION,
                    agent_session
                );
            }
        }
    }

    #[test]
    fn tui_session_name_does_not_equal_any_common_project_name() {
        let project_names = ["bork", "myapp", "test", "app", "project", "dev"];
        for name in project_names {
            assert_ne!(
                external::tmux::BORK_TUI_SESSION,
                name,
                "wrapper session '{}' collides with project name '{}'",
                external::tmux::BORK_TUI_SESSION,
                name
            );
        }
    }

    #[test]
    fn find_project_root_from_container_path() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".bork")).unwrap();

        assert_eq!(
            find_project_root_from(dir.path()),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn find_project_root_from_nested_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let nested = dir.path().join("main").join("src");
        std::fs::create_dir_all(dir.path().join(".bork")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            find_project_root_from(&nested),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn agent_status_worker_exits_when_wake_channel_disconnects() {
        let dir = std::env::temp_dir().join(format!("bork-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let (wake_tx, wake_rx) = mpsc::channel::<()>();
        let suspended = Arc::new(AtomicBool::new(false));
        let rx = spawn_agent_status_worker(dir.clone(), suspended, wake_rx);

        // First result arrives from the initial poll.
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "worker should deliver an initial poll result"
        );

        // Dropping the wake sender disconnects the channel; the worker must
        // notice during its sleep and exit, closing the result channel.
        drop(wake_tx);
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_secs(5)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ),
            "worker should exit once the wake channel disconnects"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_flag_stops_port_worker() {
        let shutdown = Arc::new(AtomicBool::new(true));
        let suspended = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(Mutex::new(HashSet::<String>::new()));

        let rx = spawn_port_poll_worker(sessions, suspended, shutdown);

        // Shutdown is pre-set, so the worker must exit before polling and
        // disconnect the channel instead of delivering a result.
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_secs(5)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ),
            "worker should exit without polling when shutdown is set"
        );
    }

    #[test]
    fn sleep_with_wake_drains_queued_wakes() {
        let (wake_tx, wake_rx) = mpsc::channel::<()>();
        // Queue several wakes (user mashing a refresh key).
        for _ in 0..5 {
            wake_tx.send(()).unwrap();
        }
        assert!(sleep_with_wake(&wake_rx, Duration::from_secs(5)));
        // All queued wakes were consumed by the single wake-up.
        assert!(wake_rx.try_recv().is_err());
    }

    #[test]
    fn sleep_with_wake_returns_false_on_disconnect() {
        let (wake_tx, wake_rx) = mpsc::channel::<()>();
        drop(wake_tx);
        assert!(!sleep_with_wake(&wake_rx, Duration::from_secs(5)));
    }
}
