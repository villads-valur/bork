use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::AppError;

/// Name of the tmux session that wraps the bork TUI. Dedicated so it can't
/// collide with project names.
pub const BORK_TUI_SESSION: &str = "bork-tui";

pub fn is_inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

pub enum EnsureResult {
    AlreadyInside,
    Wrapped { exit_code: i32 },
}

/// If we're not inside tmux, create/attach a project-scoped session that runs bork.
/// Returns Wrapped when the outer process should exit (the real bork is running inside tmux).
pub fn ensure_bork_session(project_name: &str) -> Result<EnsureResult, AppError> {
    if is_inside_tmux() {
        return Ok(EnsureResult::AlreadyInside);
    }

    // Verify tmux is installed
    Command::new("tmux")
        .arg("-V")
        .output()
        .map_err(|_| AppError::Tmux("tmux is not installed".to_string()))?;

    let session_name = project_name;

    // If the session exists but the pane process is dead (e.g. after a crash
    // or battery death), kill the stale session so we can recreate it cleanly.
    if session_exists(session_name) && !is_pane_alive(session_name) {
        let _ = kill_session(session_name);
    }

    if !session_exists(session_name) {
        let exe = std::env::current_exe()
            .map_err(|e| AppError::Tmux(format!("could not determine executable path: {e}")))?;

        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                session_name,
                "-n",
                session_name,
                exe.to_str().unwrap_or("bork"),
            ])
            .stderr(Stdio::null())
            .status()
            .map_err(|e| {
                AppError::Tmux(format!("failed to create session '{session_name}': {e}"))
            })?;

        if !status.success() {
            return Err(AppError::Tmux(format!(
                "failed to create session '{session_name}'"
            )));
        }

        // Hide the tmux status bar so our ratatui footer is the only chrome
        set_option(session_name, "status", "off");

        // Forward terminal title changes to the outer terminal (e.g. Ghostty tab title)
        set_option(session_name, "set-titles", "on");
        set_option(session_name, "set-titles-string", "#{pane_title}");

        configure_extended_keys(session_name);

        // Bind Ctrl+q to detach (scoped to this tmux server, not the user's outer tmux)
        let _ = Command::new("tmux")
            .args(["bind-key", "-n", "C-q", "detach-client"])
            .stderr(Stdio::null())
            .status();
    }

    // Verify the session is alive. If the inner bork crashed (e.g. lock
    // contention), the session may already be gone.
    if !session_exists(session_name) {
        return Err(AppError::Tmux(format!(
            "bork failed to start inside tmux session '{session_name}'. \
             Check .bork/bork.pid for a stale lock file."
        )));
    }

    // Attach to the session (blocks until user detaches)
    let status = Command::new("tmux")
        .args(["attach", "-t", session_name])
        .status()
        .map_err(|e| {
            AppError::Tmux(format!("failed to attach to session '{session_name}': {e}"))
        })?;

    Ok(EnsureResult::Wrapped {
        exit_code: status.code().unwrap_or(0),
    })
}

pub fn session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check whether the first pane's process is still alive.
/// After a crash or battery death, the tmux session may survive but the
/// process inside it (bork) is dead. tmux marks this with `pane_dead`.
fn is_pane_alive(session: &str) -> bool {
    let target = format!("{session}:0.0");
    let output = Command::new("tmux")
        .args(["display-message", "-t", &target, "-p", "#{pane_dead}"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let val = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // pane_dead is "1" when the process has exited, "0" when alive
            val != "1"
        }
        _ => false,
    }
}

/// List all tmux session names. Used by the background status worker.
pub fn list_sessions() -> HashSet<String> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect(),
        _ => HashSet::new(),
    }
}

pub fn create_session(name: &str, cwd: &Path) -> Result<(), AppError> {
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            cwd.to_str().unwrap_or("."),
        ])
        .stderr(Stdio::null())
        .status()
        .map_err(|e| AppError::Tmux(format!("failed to create session '{name}': {e}")))?;

    if !status.success() {
        return Err(AppError::Tmux(format!(
            "tmux new-session failed for '{name}'"
        )));
    }

    // Show a minimal status bar with detach hint
    set_option(name, "status", "on");
    set_option(name, "status-style", "bg=default,fg=colour8");
    set_option(name, "status-left", "");
    set_option(name, "status-right", " Ctrl+q: back to board ");
    set_option(name, "status-right-style", "bg=default,fg=colour8");
    set_option(name, "status-justify", "right");
    configure_extended_keys(name);

    Ok(())
}

/// Best-effort `tmux set-option`; failures are ignored (styling/key options are
/// non-critical and unsupported values are silently ignored on older tmux).
fn set_option(session_name: &str, option: &str, value: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-t", session_name, option, value])
        .stderr(Stdio::null())
        .status();
}

fn configure_extended_keys(session_name: &str) {
    // Forward modifier-bearing keys (Shift+Enter, Ctrl+Enter, etc.) through to bork.
    // Crossterm expects CSI-u; tmux's default xterm format emits CSI 27;...~
    // sequences for Shift+Enter, which crossterm 0.29 does not parse.
    set_option(session_name, "extended-keys", "always");
    set_option(session_name, "extended-keys-format", "csi-u");
}

/// Kill a tmux session. Returns whether a live session was killed; a missing
/// session (or missing tmux entirely) is `Ok(false)`.
///
/// Issue sessions should go through `opencode::terminate_session` instead,
/// which also sweeps processes the agent leaked — calling this directly for
/// an issue session reintroduces orphan leaks.
pub fn kill_session(name: &str) -> Result<bool, AppError> {
    let output = match Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output()
    {
        Ok(output) => output,
        // No tmux binary means no session to kill; same outcome as a missing
        // session, and CLI subcommands must keep working without tmux.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(AppError::Tmux(format!(
                "failed to kill session '{name}': {e}"
            )))
        }
    };

    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_missing_session_error(&stderr) {
        return Ok(false);
    }

    Err(AppError::Tmux(format!(
        "tmux kill-session failed for '{name}': {}",
        stderr.trim()
    )))
}

/// Return the shell PID for every pane in a session. Missing sessions are an
/// empty set so termination remains idempotent.
#[cfg_attr(test, allow(dead_code))]
pub fn pane_pids(name: &str) -> Result<Vec<i32>, AppError> {
    let output = match Command::new("tmux")
        .args(["list-panes", "-s", "-t", name, "-F", "#{pane_pid}"])
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(AppError::Tmux(format!(
                "failed to list panes for session '{name}': {e}"
            )))
        }
    };

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_missing_session_error(&stderr) {
        return Ok(Vec::new());
    }

    Err(AppError::Tmux(format!(
        "tmux list-panes failed for '{name}': {}",
        stderr.trim()
    )))
}

fn is_missing_session_error(stderr: &str) -> bool {
    stderr.contains("can't find session") || stderr.contains("no server running")
}

pub fn send_keys(session: &str, keys: &str) -> Result<(), AppError> {
    let status = Command::new("tmux")
        .args(["send-keys", "-t", session, keys, "Enter"])
        .stderr(Stdio::null())
        .status()
        .map_err(|e| AppError::Tmux(format!("failed to send keys to '{session}': {e}")))?;

    if !status.success() {
        return Err(AppError::Tmux(format!(
            "tmux send-keys failed for '{session}'"
        )));
    }
    Ok(())
}

pub fn create_window(session: &str, window_name: &str, cwd: &Path) -> Result<(), AppError> {
    let status = Command::new("tmux")
        .args([
            "new-window",
            "-d",
            "-t",
            session,
            "-n",
            window_name,
            "-c",
            cwd.to_str().unwrap_or("."),
        ])
        .stderr(Stdio::null())
        .status()
        .map_err(|e| {
            AppError::Tmux(format!(
                "failed to create window '{window_name}' in '{session}': {e}"
            ))
        })?;

    if !status.success() {
        return Err(AppError::Tmux(format!(
            "tmux new-window failed for '{session}:{window_name}'"
        )));
    }

    Ok(())
}

/// Open a session as a tmux popup overlay (95% of the screen).
/// This blocks until the user detaches or the popup closes.
pub fn open_popup(session: &str, title: &str) -> Result<(), AppError> {
    if !is_inside_tmux() {
        // Fallback: just attach directly
        let _ = Command::new("tmux")
            .args(["attach", "-t", session])
            .stderr(Stdio::null())
            .status();
        return Ok(());
    }

    let attach_cmd = format!("tmux attach -t {}", shell_escape(session));
    let popup_title = format!(" {} ", title);

    let status = Command::new("tmux")
        .args([
            "display-popup",
            "-E",
            "-w",
            "95%",
            "-h",
            "95%",
            "-T",
            &popup_title,
            &attach_cmd,
        ])
        .stderr(Stdio::null())
        .status()
        .map_err(|e| AppError::Tmux(format!("failed to open popup for '{session}': {e}")))?;

    if !status.success() {
        return Err(AppError::Tmux(format!(
            "tmux display-popup failed for '{session}'"
        )));
    }

    Ok(())
}

fn shell_escape(s: &str) -> String {
    // Simple escaping: wrap in single quotes, escape any internal single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_session_errors_are_harmless() {
        assert!(is_missing_session_error("can't find session: bork-1"));
        assert!(is_missing_session_error("no server running on /tmp/tmux"));
    }

    #[test]
    fn connection_errors_are_not_treated_as_missing_sessions() {
        assert!(!is_missing_session_error(
            "error connecting to /tmp/tmux/default (Operation not permitted)"
        ));
    }
}
