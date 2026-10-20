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
const MODE_OPTION: &str = "@bork-tuicr-mode";

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu",
];

/// Show tuicr in `session`, reusing its tuicr window when there is one.
pub fn open_in_session(session: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    let Some(window) = tmux::window_ids(session, TUICR_WINDOW)?.into_iter().next() else {
        tmux::create_window(session, TUICR_WINDOW, cwd)?;
        start_tuicr(&format!("{session}:{TUICR_WINDOW}"), pr_mode)?;
        return tmux::select_window(session, TUICR_WINDOW);
    };

    resume_tuicr(&format!("{session}:{window}"), cwd, pr_mode)?;
    tmux::select_window(session, &window)
}

/// tuicr is started only at a shell prompt, and restarted when it runs in
/// the other mode. Any other program in the window is left alone rather than
/// typed into.
fn resume_tuicr(target: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    let wanted = mode(pr_mode);
    match tmux::pane_current_command(target).as_deref() {
        Some("tuicr") if tmux::window_option(target, MODE_OPTION).as_deref() == Some(wanted) => {
            Ok(())
        }
        Some("tuicr") => {
            tmux::respawn_pane(target, cwd)?;
            start_tuicr(target, pr_mode)
        }
        Some(command) if SHELLS.contains(&command.trim_start_matches('-')) => {
            start_tuicr(target, pr_mode)
        }
        _ => Ok(()),
    }
}

/// Create a fresh tmux session whose first window runs tuicr.
/// Used when there is no agent session for the issue but the user wants to review.
pub fn launch_review_session(session: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    tmux::create_session(session, cwd)?;
    tmux::rename_window(session, TUICR_WINDOW)?;
    start_tuicr(session, pr_mode)?;
    tmux::create_window(session, "terminal", cwd)?;
    Ok(())
}

fn start_tuicr(target: &str, pr_mode: bool) -> Result<(), AppError> {
    tmux::set_window_option(target, MODE_OPTION, mode(pr_mode))?;
    tmux::send_keys(target, &tuicr_cmd(pr_mode))
}

fn mode(pr_mode: bool) -> &'static str {
    if pr_mode {
        "pr"
    } else {
        "local"
    }
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

    /// A session whose new windows run a plain `/bin/sh` with a fake `tuicr`
    /// on `PATH`. The fake is compiled rather than scripted because macOS
    /// reports a script by its interpreter's name; it logs each launch's
    /// arguments and stays in the foreground as `tuicr`, like the real one.
    struct FakeTuicrSession {
        name: String,
        log: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    const FAKE_TUICR: &str = r#"
use std::io::Write;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = std::env::var("FAKE_TUICR_LOG").unwrap();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
    writeln!(file, "{}", args.join(" ")).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(600));
}
"#;

    impl FakeTuicrSession {
        fn start(label: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let source = dir.path().join("fake_tuicr.rs");
            std::fs::write(&source, FAKE_TUICR).unwrap();
            let compiled = Command::new("rustc")
                .arg("-o")
                .arg(bin.join("tuicr"))
                .arg(&source)
                .status()
                .unwrap();
            assert!(compiled.success());

            let log = dir.path().join("launches");
            let name = format!("bork-tuicr-{label}-{}", std::process::id());
            tmux::create_session(&name, dir.path()).unwrap();
            // A login shell would rebuild PATH from the system profile.
            tmux::set_session_option(
                &name,
                "default-command",
                &format!(
                    "PATH='{}:/usr/bin:/bin' FAKE_TUICR_LOG='{}' exec /bin/sh",
                    bin.display(),
                    log.display()
                ),
            );
            Self {
                name,
                log,
                _dir: dir,
            }
        }

        fn cwd(&self) -> &Path {
            self._dir.path()
        }

        fn launches(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn wait_for_launches(&self, count: usize) {
            wait_until(|| self.launches().len() >= count);
        }
    }

    impl Drop for FakeTuicrSession {
        fn drop(&mut self) {
            let _ = tmux::kill_session(&self.name);
        }
    }

    fn wait_until(condition: impl Fn() -> bool) {
        for _ in 0..50 {
            if condition() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn tuicr_is_running(session: &str) -> bool {
        tmux::pane_current_command(&format!("{session}:tuicr")).as_deref() == Some("tuicr")
    }

    #[test]
    fn reopening_a_running_review_does_not_restart_tuicr() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let session = FakeTuicrSession::start("running");

        open_in_session(&session.name, session.cwd(), false).unwrap();
        // The pane reports tuicr before it has logged its launch.
        wait_until(|| tuicr_is_running(&session.name));
        session.wait_for_launches(1);
        open_in_session(&session.name, session.cwd(), false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        assert_eq!(session.launches(), [""]);
    }

    #[test]
    fn reopening_a_review_in_the_other_mode_restarts_tuicr() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let session = FakeTuicrSession::start("mode");

        open_in_session(&session.name, session.cwd(), false).unwrap();
        // The pane reports tuicr before it has logged its launch.
        wait_until(|| tuicr_is_running(&session.name));
        session.wait_for_launches(1);
        open_in_session(&session.name, session.cwd(), true).unwrap();
        session.wait_for_launches(2);

        assert_eq!(session.launches(), ["", "--pr"]);
        assert_eq!(tuicr_windows(&session.name), 1);
    }

    #[test]
    fn a_program_running_in_the_tuicr_window_is_not_typed_into() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let session = FakeTuicrSession::start("foreign");
        tmux::create_window(&session.name, "tuicr", session.cwd()).unwrap();
        let target = format!("{}:tuicr", session.name);
        tmux::send_keys(&target, "cat").unwrap();
        wait_until(|| tmux::pane_current_command(&target).as_deref() == Some("cat"));

        open_in_session(&session.name, session.cwd(), false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        assert_eq!(tmux::pane_current_command(&target).as_deref(), Some("cat"));
        assert!(!tmux::capture_pane(&target).contains("tuicr"));
        assert_eq!(
            tmux::active_window_name(&session.name).as_deref(),
            Some("tuicr")
        );
    }

    #[test]
    fn a_session_left_with_two_tuicr_windows_still_opens() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let session = FakeTuicrSession::start("duplicates");
        tmux::create_window(&session.name, "tuicr", session.cwd()).unwrap();
        tmux::create_window(&session.name, "tuicr", session.cwd()).unwrap();

        open_in_session(&session.name, session.cwd(), false).unwrap();
        session.wait_for_launches(1);

        assert_eq!(session.launches(), [""]);
        assert_eq!(
            tmux::active_window_name(&session.name).as_deref(),
            Some("tuicr")
        );
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
