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

    let cmd = if pr_mode {
        "tuicr --pr || tuicr"
    } else {
        "tuicr"
    };
    let target = format!("{session}:tuicr");
    tmux::send_keys(&target, cmd)?;

    Ok(())
}
