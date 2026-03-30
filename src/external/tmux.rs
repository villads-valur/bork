use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

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

    if !session_exists(session_name) {
        // Get the path to ourselves
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
            .status();

        // Bind Ctrl+q to detach (scoped to this tmux server, not the user's outer tmux)
        let _ = Command::new("tmux")
            .args(["bind-key", "-n", "C-q", "detach-client"])
            .status();
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
        .status();
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            name,
            "status-style",
            "bg=default,fg=colour8",
        ])
        .status();
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "status-left", ""])
        .status();
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            name,
            "status-right",
            " Ctrl+q: back to board ",
        ])
        .status();
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            name,
            "status-right-style",
            "bg=default,fg=colour8",
        ])
        .status();
    let _ = Command::new("tmux")
        .args(["set-option", "-t", name, "status-justify", "right"])
        .status();

    Ok(())
}

pub fn kill_session(name: &str) -> Result<(), AppError> {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

pub fn send_keys(session: &str, keys: &str) -> Result<(), AppError> {
    let status = Command::new("tmux")
        .args(["send-keys", "-t", session, keys, "Enter"])
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
            "-t",
            session,
            "-n",
            window_name,
            "-c",
            cwd.to_str().unwrap_or("."),
        ])
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

    // Switch back to the first window so the agent is visible when opening the popup
    let _ = Command::new("tmux")
        .args(["select-window", "-t", &format!("{session}:0")])
        .status();

    Ok(())
}

/// Open a session as a tmux popup overlay (90% of the screen).
/// This blocks until the user detaches or the popup closes.
pub fn open_popup(session: &str) -> Result<(), AppError> {
    if !is_inside_tmux() {
        // Fallback: just attach directly
        let _ = Command::new("tmux")
            .args(["attach", "-t", session])
            .status();
        return Ok(());
    }

    let attach_cmd = format!("tmux attach -t {}", shell_escape(session));

    let status = Command::new("tmux")
        .args(["display-popup", "-E", "-w", "90%", "-h", "90%", &attach_cmd])
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
