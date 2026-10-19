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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewTarget {
    Worktree(PathBuf),
    PrUrl(String),
}

/// Counts drafts across every persisted tuicr session. Session files are only
/// re-read when tuicr reports a new `updated_at` for them.
#[derive(Default)]
pub struct DraftPoller {
    cache: HashMap<String, (String, Option<(ReviewTarget, u32)>)>,
}

impl DraftPoller {
    pub fn poll(&mut self) -> Option<DraftComments> {
        let output = Command::new("tuicr")
            .args(["review", "list", "--all"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let sessions: Vec<Value> = serde_json::from_slice(&output.stdout).ok()?;

        let mut seen = HashMap::new();
        for session in &sessions {
            let Some(path) = session.get("path").and_then(Value::as_str) else {
                continue;
            };
            let updated_at = session
                .get("updated_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let has_comments = session
                .get("comment_count")
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0);
            let entry = match self.cache.remove(path) {
                Some((cached_at, drafts)) if cached_at == updated_at => drafts,
                _ if !has_comments => None,
                _ => std::fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|value| session_drafts(&value)),
            };
            seen.insert(path.to_string(), (updated_at, entry));
        }
        self.cache = seen;

        let mut drafts = DraftComments::default();
        for (target, count) in self.cache.values().filter_map(|(_, entry)| entry.as_ref()) {
            match target {
                ReviewTarget::Worktree(path) => {
                    *drafts.by_worktree.entry(path.clone()).or_default() += count;
                }
                ReviewTarget::PrUrl(url) => {
                    *drafts.by_pr_url.entry(url.clone()).or_default() += count;
                }
            }
        }
        Some(drafts)
    }
}

/// What a session file reviews and how many of its comments are still drafts.
fn session_drafts(session: &Value) -> Option<(ReviewTarget, u32)> {
    let count = draft_count(session);
    if count == 0 {
        return None;
    }
    if let Some(key) = session.get("pr_session_key").filter(|key| !key.is_null()) {
        let repository = key.get("repository")?;
        if repository.get("kind").and_then(Value::as_str) != Some("git_hub") {
            return None;
        }
        let field = |name: &str| repository.get(name).and_then(Value::as_str);
        let url = format!(
            "https://{}/{}/{}/pull/{}",
            field("host")?,
            field("owner")?,
            field("name")?,
            key.get("number")?.as_u64()?
        );
        return Some((ReviewTarget::PrUrl(url), count));
    }
    let repo_path = session.get("repo_path")?.as_str()?.trim_end_matches('/');
    Some((ReviewTarget::Worktree(PathBuf::from(repo_path)), count))
}

fn draft_count(session: &Value) -> u32 {
    let files = session
        .get("files")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|files| files.values());
    let file_comments = files.clone().flat_map(|file| {
        file.get("file_comments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    });
    let line_comments = files.flat_map(|file| {
        file.get("line_comments")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|lines| lines.values())
            .filter_map(Value::as_array)
            .flatten()
    });
    let review_comments = session
        .get("review_comments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    file_comments
        .chain(line_comments)
        .chain(review_comments)
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
