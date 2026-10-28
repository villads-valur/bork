use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

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
pub fn open_in_session(session: &str, cwd: &Path, pr: Option<u32>) -> Result<(), AppError> {
    let Some(window) = tmux::window_ids(session, TUICR_WINDOW)?.into_iter().next() else {
        tmux::create_window(session, TUICR_WINDOW, cwd)?;
        start_tuicr(&format!("{session}:{TUICR_WINDOW}"), pr)?;
        return tmux::select_window(session, TUICR_WINDOW);
    };

    resume_tuicr(&format!("{session}:{window}"), cwd, pr)?;
    tmux::select_window(session, &window)
}

/// tuicr is started only at a shell prompt, and restarted when it reviews
/// something else. Any other program in the window is left alone rather than
/// typed into.
fn resume_tuicr(target: &str, cwd: &Path, pr: Option<u32>) -> Result<(), AppError> {
    let wanted = mode(pr);
    match tmux::pane_current_command(target).as_deref() {
        Some("tuicr") if tmux::window_option(target, MODE_OPTION) == Some(wanted) => Ok(()),
        Some("tuicr") => {
            tmux::respawn_pane(target, cwd)?;
            start_tuicr(target, pr)
        }
        Some(command) if SHELLS.contains(&command.trim_start_matches('-')) => {
            start_tuicr(target, pr)
        }
        _ => Ok(()),
    }
}

/// Create a fresh tmux session whose first window runs tuicr.
/// Used when there is no agent session for the issue but the user wants to review.
pub fn launch_review_session(session: &str, cwd: &Path, pr: Option<u32>) -> Result<(), AppError> {
    tmux::create_session(session, cwd)?;
    tmux::rename_window(session, TUICR_WINDOW)?;
    start_tuicr(session, pr)?;
    tmux::create_window(session, "terminal", cwd)?;
    Ok(())
}

fn start_tuicr(target: &str, pr: Option<u32>) -> Result<(), AppError> {
    tmux::set_window_option(target, MODE_OPTION, &mode(pr))?;
    tmux::send_keys(target, &tuicr_cmd(pr))
}

fn mode(pr: Option<u32>) -> String {
    match pr {
        Some(number) => format!("pr:{number}"),
        None => "local".to_string(),
    }
}

/// Falls back to the local review when tuicr can't load the PR.
fn tuicr_cmd(pr: Option<u32>) -> String {
    match pr {
        Some(number) => format!("tuicr pr {number} || tuicr"),
        None => "tuicr".to_string(),
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

/// Unposted tuicr comments, keyed by what they review.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftComments {
    /// Local reviews, keyed by the reviewed checkout path.
    pub by_worktree: HashMap<PathBuf, u32>,
    /// PR reviews, keyed by the PR's GitHub URL.
    pub by_pr_url: HashMap<String, u32>,
}

/// Counts drafts across every persisted tuicr session. Only sessions tuicr
/// reports as having comments are read.
pub fn poll_drafts() -> Option<DraftComments> {
    let mut command = Command::new("tuicr");
    crate::external::harden_command(&mut command);
    let output = command
        .args(["review", "list", "--all"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sessions: Vec<Value> = serde_json::from_slice(&output.stdout).ok()?;

    let mut drafts = DraftComments::default();
    for session in &sessions {
        if session.get("comment_count").and_then(Value::as_u64) == Some(0) {
            continue;
        }
        let Some(path) = session.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(session) = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            continue;
        };
        let count = draft_count(&session);
        if count == 0 {
            continue;
        }
        match review_target(&session) {
            Some(ReviewTarget::PrUrl(url)) => *drafts.by_pr_url.entry(url).or_default() += count,
            Some(ReviewTarget::Worktree(path)) => {
                *drafts.by_worktree.entry(path).or_default() += count
            }
            None => {}
        }
    }
    Some(drafts)
}

#[derive(Debug, PartialEq)]
enum ReviewTarget {
    PrUrl(String),
    Worktree(PathBuf),
}

/// The GitHub PR or local checkout a session reviews.
fn review_target(session: &Value) -> Option<ReviewTarget> {
    let Some(key) = session.get("pr_session_key").filter(|key| !key.is_null()) else {
        let repo_path = session.get("repo_path")?.as_str()?.trim_end_matches('/');
        return Some(ReviewTarget::Worktree(PathBuf::from(repo_path)));
    };
    let repository = key.get("repository")?;
    if repository.get("kind").and_then(Value::as_str) != Some("git_hub") {
        return None;
    }
    let field = |name: &str| repository.get(name).and_then(Value::as_str);
    Some(ReviewTarget::PrUrl(format!(
        "https://{}/{}/{}/pull/{}",
        field("host")?,
        field("owner")?,
        field("name")?,
        key.get("number")?.as_u64()?
    )))
}

/// Drafts across the three places tuicr stores comments: per file, per line,
/// and for the whole review.
fn draft_count(session: &Value) -> u32 {
    let files = session
        .get("files")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|files| files.values());
    let mut lists: Vec<&Value> = Vec::new();
    for file in files {
        lists.extend(file.get("file_comments"));
        lists.extend(
            file.get("line_comments")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|lines| lines.values()),
        );
    }
    lists.extend(session.get("review_comments"));
    lists
        .into_iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter(|comment| is_draft(comment))
        .count() as u32
}

/// Comments written but not yet posted. Older sessions have no lifecycle field,
/// and everything in them is local.
fn is_draft(comment: &Value) -> bool {
    comment
        .get("lifecycle_state")
        .and_then(Value::as_str)
        .is_none_or(|state| state == "local_draft")
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

    fn session(pr_session_key: Value) -> Value {
        serde_json::json!({
            "repo_path": "/work/bork-1/",
            "pr_session_key": pr_session_key,
            "files": {
                "src/a.rs": {
                    "file_comments": [{"lifecycle_state": "local_draft"}],
                    "line_comments": {
                        "3": [{"lifecycle_state": "local_draft"}],
                        "9": [{"lifecycle_state": "posted"}, {}]
                    }
                }
            },
            "review_comments": [{"lifecycle_state": "local_draft"}]
        })
    }

    fn pr_key(kind: &str) -> Value {
        serde_json::json!({
            "repository": {"kind": kind, "host": "github.com", "owner": "acme", "name": "bork"},
            "number": 12
        })
    }

    #[test]
    fn draft_count_skips_posted_comments_and_keeps_old_unmarked_ones() {
        assert_eq!(draft_count(&session(Value::Null)), 4);
    }

    #[test]
    fn local_sessions_target_their_checkout() {
        assert_eq!(
            review_target(&session(Value::Null)),
            Some(ReviewTarget::Worktree(PathBuf::from("/work/bork-1")))
        );
    }

    #[test]
    fn pr_sessions_target_the_github_pr_url() {
        assert_eq!(
            review_target(&session(pr_key("git_hub"))),
            Some(ReviewTarget::PrUrl(
                "https://github.com/acme/bork/pull/12".into()
            ))
        );
        assert_eq!(review_target(&session(pr_key("git_lab"))), None);
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

        let first = open_in_session(&session, &cwd, None);
        let second = open_in_session(&session, &cwd, None);
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

        open_in_session(&session.name, session.cwd(), None).unwrap();
        // The pane reports tuicr before it has logged its launch.
        wait_until(|| tuicr_is_running(&session.name));
        session.wait_for_launches(1);
        open_in_session(&session.name, session.cwd(), None).unwrap();
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

        open_in_session(&session.name, session.cwd(), None).unwrap();
        // The pane reports tuicr before it has logged its launch.
        wait_until(|| tuicr_is_running(&session.name));
        session.wait_for_launches(1);
        open_in_session(&session.name, session.cwd(), Some(12)).unwrap();
        session.wait_for_launches(2);

        assert_eq!(session.launches(), ["", "pr 12"]);
        assert_eq!(tuicr_windows(&session.name), 1);
    }

    #[test]
    fn reopening_a_review_restarts_tuicr_only_for_a_different_pr() {
        if !tmux::tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let session = FakeTuicrSession::start("switch");

        open_in_session(&session.name, session.cwd(), Some(12)).unwrap();
        // The pane reports tuicr before it has logged its launch.
        wait_until(|| tuicr_is_running(&session.name));
        session.wait_for_launches(1);
        open_in_session(&session.name, session.cwd(), Some(12)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(session.launches(), ["pr 12"]);

        open_in_session(&session.name, session.cwd(), Some(13)).unwrap();
        session.wait_for_launches(2);

        assert_eq!(session.launches(), ["pr 12", "pr 13"]);
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

        open_in_session(&session.name, session.cwd(), None).unwrap();
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

        open_in_session(&session.name, session.cwd(), None).unwrap();
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

        let launched = launch_review_session(&session, &cwd, None);
        let opened = open_in_session(&session, &cwd, None);
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

        let opened = open_in_session(&session, &cwd, None);
        let active = tmux::active_window_name(&session);
        let _ = tmux::kill_session(&session);

        opened.unwrap();
        assert_eq!(active.as_deref(), Some("tuicr"));
    }
}
