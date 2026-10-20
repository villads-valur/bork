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

pub fn open_in_session(session: &str, cwd: &Path, pr_mode: bool) -> Result<(), AppError> {
    tmux::create_window(session, "tuicr", cwd)?;

    let target = format!("{session}:tuicr");
    tmux::send_keys(&target, &tuicr_cmd(pr_mode))?;
    tmux::select_window(session, "tuicr")?;

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
