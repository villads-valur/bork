mod app;
mod config;
mod error;
mod external;
mod handler;
mod input;
mod types;
mod ui;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;
use handler::{ActionResult, PostAction};
use input::map_key_to_action;
use types::AgentStatusInfo;

use external::git::GitPollResult;

const TICK_RATE: Duration = Duration::from_millis(50);
const TMUX_POLL_INTERVAL: Duration = Duration::from_secs(2);
const GIT_POLL_INTERVAL: Duration = Duration::from_secs(3);

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

fn spawn_git_status_worker(project_root: PathBuf) -> mpsc::Receiver<GitPollResult> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let result = external::git::poll_all_worktrees(&project_root);
        if tx.send(result).is_err() {
            break;
        }
        thread::sleep(GIT_POLL_INTERVAL);
    });

    rx
}

fn main() -> anyhow::Result<()> {
    // --- CLI subcommands ---
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("install") => return external::hooks::install(),
        Some("uninstall") => return external::hooks::uninstall(),
        _ => {}
    }

    // --- Tmux auto-wrap ---
    match external::tmux::ensure_bork_session()? {
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

    // --- Load config + state ---
    let config = config::load_config();
    let state = config::load_state(&config.project_root);
    let mut app = App::new(config, state);

    // --- Ensure agent-status dir ---
    config::ensure_agent_status_dir(&app.config.project_root);

    // --- Channels ---
    let (action_tx, action_rx) = mpsc::channel::<ActionResult>();
    let status_dir = config::agent_status_dir(&app.config.project_root);
    let session_rx = spawn_session_status_worker(status_dir);
    let git_rx = spawn_git_status_worker(app.config.project_root.clone());

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
                        let post_action = handler::handle_action(&mut app, action, &action_tx);

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

            if let Some(session_name) = result.session_to_open {
                if let Some(launch_idx) = pending_popup_for_launch.take() {
                    app.issues[launch_idx].tmux_session = Some(session_name.clone());
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

        while let Ok(git_result) = git_rx.try_recv() {
            app.worktree_statuses = git_result.statuses;
            app.worktree_branches = git_result.branches;
        }

        if app.busy_count > 0 {
            app.spinner_tick = app.spinner_tick.wrapping_add(1);
        }
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
