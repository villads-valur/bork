mod app;
mod config;
mod error;
mod external;
mod handler;
mod input;
mod types;
mod ui;

use std::collections::HashSet;
use std::io;
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

const TICK_RATE: Duration = Duration::from_millis(50);
const TMUX_POLL_INTERVAL: Duration = Duration::from_secs(2);

// --- Tmux status worker (persistent background thread) ---

fn spawn_tmux_status_worker() -> mpsc::Receiver<HashSet<String>> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || loop {
        let sessions = external::tmux::list_sessions();
        if tx.send(sessions).is_err() {
            break;
        }
        thread::sleep(TMUX_POLL_INTERVAL);
    });

    rx
}

fn main() -> anyhow::Result<()> {
    // --- Tmux auto-wrap ---
    // If we're not inside tmux, wrap ourselves in a tmux session.
    match external::tmux::ensure_bork_session()? {
        external::tmux::EnsureResult::AlreadyInside => {}
        external::tmux::EnsureResult::Wrapped { exit_code } => {
            std::process::exit(exit_code);
        }
    }

    // --- Panic hook: restore terminal even on panic ---
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

    // --- Load config + state, create App ---
    let config = config::load_config();
    let state = config::load_state();
    let mut app = App::new(config, state);

    // --- Channels ---
    let (action_tx, action_rx) = mpsc::channel::<ActionResult>();
    let tmux_rx = spawn_tmux_status_worker();

    // Track if we're waiting for a session launch before opening popup
    let mut pending_popup_session: Option<String> = None;
    let mut pending_popup_for_launch: Option<usize> = None;

    // --- Main event loop ---
    loop {
        // 1. Render
        terminal.draw(|frame| {
            ui::render(frame, &app);
        })?;

        // 2. Poll + drain key events
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

        // 3. Check quit
        if app.should_quit {
            break;
        }

        // 4. Drain action results (from background threads)
        while let Ok(result) = action_rx.try_recv() {
            app.busy_count = app.busy_count.saturating_sub(1);
            app.set_message(result.message);

            if let Some(session_name) = result.session_to_open {
                // If this was the session we were waiting on, open the popup
                if let Some(launch_idx) = pending_popup_for_launch.take() {
                    app.issues[launch_idx].tmux_session = Some(session_name.clone());
                    pending_popup_session = Some(session_name);
                }
            }
        }

        // Open popup if a session launch just completed
        if let Some(session_name) = pending_popup_session.take() {
            // Save state before opening popup
            let _ = config::save_state(&app.to_state());

            open_tmux_popup(&mut terminal, &session_name)?;
            app.message = None;
        }

        // 5. Drain tmux status updates (from background worker)
        while let Ok(sessions) = tmux_rx.try_recv() {
            app.active_sessions = sessions;
        }

        // 6. Tick spinner when busy
        if app.busy_count > 0 {
            app.spinner_tick = app.spinner_tick.wrapping_add(1);
        }
    }

    // --- Save state before exit ---
    let _ = config::save_state(&app.to_state());

    // --- Restore terminal ---
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    Ok(())
}

/// Open a tmux popup for a session.
/// This temporarily leaves the alternate screen so tmux can take over,
/// then restores everything when the popup closes (user detaches).
fn open_tmux_popup(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session_name: &str,
) -> anyhow::Result<()> {
    // Leave our TUI so tmux popup can render
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    // This blocks until the user detaches from the popup
    let _ = external::tmux::open_popup(session_name);

    // Restore our TUI
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;

    // Force full redraw
    terminal.clear()?;

    Ok(())
}
