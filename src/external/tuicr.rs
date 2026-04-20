use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::AppError;
use crate::external::tmux;

pub fn check_available() -> bool {
    Command::new("tuicr")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn open_in_session(session: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    tmux::create_window(session, "tuicr", cwd)?;

    let target = format!("{session}:tuicr");
    tmux::send_keys(&target, &tuicr_cmd(pr_mode))?;

    Ok(())
}

/// Create a fresh tmux session whose first window runs tuicr.
/// Used when there is no agent session for the issue but the user wants to review.
pub fn launch_review_session(session: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    tmux::create_session(session, cwd)?;
    tmux::send_keys(session, &tuicr_cmd(pr_mode))?;
    tmux::create_window(session, "terminal", cwd)?;
    Ok(())
}

fn tuicr_cmd(pr_mode: bool) -> String {
    if pr_mode {
        "tuicr --pr || tuicr".to_string()
    } else {
        "tuicr".to_string()
    }
}
