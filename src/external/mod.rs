pub mod browser;
pub mod git;
pub mod github;
pub mod hooks;
pub mod linear;
pub mod opencode;
pub mod ports;
pub mod process;
pub mod tmux;
pub mod tuicr;

use std::process::{Command, Stdio};

/// Prevent an external command from ever blocking a poller thread on a tty
/// credential prompt. Closes stdin and disables git's terminal/askpass
/// prompting so a missing credential fails fast instead of hanging forever.
pub fn harden_command(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "");
}

/// A `git` command that can never hang on a credential prompt.
pub fn git_command() -> Command {
    let mut command = Command::new("git");
    harden_command(&mut command);
    command
}

/// A `gh` command that can never hang on a credential prompt.
pub fn gh_command() -> Command {
    let mut command = Command::new("gh");
    harden_command(&mut command);
    command
}
