use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::AppError;

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
        let _ = Command::new("tmux")
            .args(["set-option", "-t", session_name, "status", "off"])
            .stderr(Stdio::null())
            .status();

        // Forward terminal title changes to the outer terminal (e.g. Ghostty tab title)
        let _ = Command::new("tmux")
            .args(["set-option", "-t", session_name, "set-titles", "on"])
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("tmux")
            .args([
                "set-option",
                "-t",
                session_name,
                "set-titles-string",
                "#{pane_title}",
            ])
            .stderr(Stdio::null())
            .status();

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
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "status", "on"])
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            name,
            "status-style",
            "bg=default,fg=colour8",
        ])
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "status-left", ""])
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            name,
            "status-right",
            " Ctrl+q: back to board ",
        ])
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            name,
            "status-right-style",
            "bg=default,fg=colour8",
        ])
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "status-justify", "right"])
        .stderr(Stdio::null())
        .status();

    Ok(())
}

pub fn kill_session(name: &str) -> Result<(), AppError> {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .stderr(Stdio::null())
        .status();
    Ok(())
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
