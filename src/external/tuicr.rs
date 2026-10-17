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

const TUICR_WINDOW: &str = "tuicr";

/// Show tuicr in `session`, reusing its tuicr window when there is one: tmux
/// cannot resolve `session:tuicr` once two windows share the name. tuicr is
/// started again only when it is no longer running there.
pub fn open_in_session(session: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    let target = format!("{session}:{TUICR_WINDOW}");

    if !tmux::window_names(session)
        .iter()
        .any(|name| name == TUICR_WINDOW)
    {
        tmux::create_window(session, TUICR_WINDOW, cwd)?;
    }

    if tmux::pane_current_command(&target).as_deref() != Some("tuicr") {
        tmux::send_keys(&target, &tuicr_cmd(pr_mode))?;
    }

    tmux::select_window(session, TUICR_WINDOW)
}

/// Create a fresh tmux session whose first window runs tuicr.
/// Used when there is no agent session for the issue but the user wants to review.
pub fn launch_review_session(session: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    tmux::create_session(session, cwd)?;
    tmux::rename_window(session, TUICR_WINDOW)?;
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

pub fn open_stack(session: &str, cwd: &Path, numbers: &[u32], alive: bool) -> Result<(), AppError> {
    if !alive {
        tmux::create_session(session, cwd)?;
    }
    tmux::create_command_window(session, "stack-review", cwd, &stack_command(numbers))
}

fn stack_command(numbers: &[u32]) -> String {
    let numbers = numbers
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "set -- {numbers}; while [ \"$#\" -gt 0 ]; do tuicr pr \"$1\" || {{ printf 'Review failed. Enter to return to the terminal: '; read -r answer; break; }}; shift; [ \"$#\" -gt 0 ] || break; printf 'Next: PR #%s. Enter to continue, q to stop: ' \"$1\"; read -r answer || break; [ \"$answer\" != q ] || break; done"
    );
    format!("sh -c '{}'", script.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn run_stack(input: &str, fail: bool) -> String {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("tuicr");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$REVIEW_LOG\"\n[ \"$FAIL_REVIEW\" != yes ]\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("reviews");
        let mut child = Command::new("/bin/sh")
            .args(["-c", &stack_command(&[18, 4, 72])])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .env("REVIEW_LOG", &log)
            .env("FAIL_REVIEW", if fail { "yes" } else { "no" })
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        std::fs::read_to_string(log).unwrap()
    }

    #[test]
    fn stack_review_preserves_order_and_waits_between_prs() {
        assert_eq!(run_stack("\n\n", false), "pr 18\npr 4\npr 72\n");
    }

    #[test]
    fn stack_review_can_stop_and_does_not_skip_failed_reviews() {
        assert_eq!(run_stack("q\n", false), "pr 18\n");
        assert_eq!(run_stack("\n\n", true), "pr 18\n");
    }

    fn tuicr_windows(session: &str) -> usize {
        tmux::window_names(session)
            .iter()
            .filter(|name| *name == "tuicr")
            .count()
    }

    #[test]
    fn reopening_a_review_reuses_the_tuicr_window() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }

        let session = format!("bork-tuicr-reopen-{}", std::process::id());
        let cwd = std::env::temp_dir();
        tmux::create_session(&session, &cwd).unwrap();

        let first = open_in_session(&session, &cwd, false);
        let second = open_in_session(&session, &cwd, false);
        let windows = tuicr_windows(&session);
        let _ = tmux::kill_session(&session);

        first.unwrap();
        second.unwrap();
        assert_eq!(windows, 1);
    }

    #[test]
    fn opening_a_review_in_a_review_session_reuses_its_tuicr_window() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }

        let session = format!("bork-tuicr-review-session-{}", std::process::id());
        let cwd = std::env::temp_dir();

        let launched = launch_review_session(&session, &cwd, false);
        let opened = open_in_session(&session, &cwd, false);
        let windows = tmux::window_names(&session);
        let _ = tmux::kill_session(&session);

        launched.unwrap();
        opened.unwrap();
        assert_eq!(windows, ["tuicr", "terminal"]);
    }

    #[test]
    fn opening_a_review_in_an_existing_session_focuses_tuicr() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }

        let session = format!("bork-tuicr-focus-{}", std::process::id());
        let cwd = std::env::temp_dir();
        tmux::create_session(&session, &cwd).unwrap();
        tmux::create_window(&session, "terminal", &cwd).unwrap();

        let opened = open_in_session(&session, &cwd, false);
        let active = tmux::active_window_name(&session);
        let _ = tmux::kill_session(&session);

        opened.unwrap();
        assert_eq!(active.as_deref(), Some("tuicr"));
    }
}
