use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{self, AppConfig};
use crate::error::AppError;
use crate::external::{github, tmux};
use crate::types::{AgentKind, AgentMode, Issue, LinkedGithubPr, LinkedLinear};

/// Launch an agent session for an issue.
/// Creates a tmux session with two windows:
///   1. The agent (opencode/claude/codex/pi) launched at the project root with issue context
///   2. A bare terminal
///
/// Exports BORK_SESSION and BORK_STATUS_DIR so hooks/plugins can write status files.
/// Returns (tmux_session_name, agent_session_id).
/// The agent_session_id is the agent's internal session ID for resuming conversations:
///   - Claude: UUID pre-assigned via --session-id
///   - OpenCode: ses_xxx detected by polling `opencode session list` after launch
///   - Codex: UUID detected from newest ~/.codex/sessions rollout transcript
pub fn launch_session(
    issue: &Issue,
    config: &AppConfig,
) -> Result<(String, Option<String>), AppError> {
    let session_name = issue.session_name(&config.project_name);
    let cwd = &config.project_root;

    if tmux::session_exists(&session_name) {
        return Ok((session_name, issue.session_id.clone()));
    }

    tmux::create_session(&session_name, cwd)?;

    let status_dir = config::agent_status_dir(&config.project_root);
    let status_dir_str = status_dir.to_str().unwrap_or("");

    let prompt_path = prompt_file_path(&status_dir, &session_name);
    let prompt_path_str = prompt_path.to_str().unwrap_or("");

    let (agent_cmd, pre_assigned_session_id, prompt_contents) = build_agent_cmd(
        issue,
        config,
        &session_name,
        status_dir_str,
        prompt_path_str,
    );

    // Multiline prompts can't be typed directly: tmux send-keys collapses
    // embedded newlines. Instead we write the prompt to a file and the launch
    // command reads it back via `"$(cat ...)"`, which preserves it verbatim.
    if let Some(contents) = &prompt_contents {
        write_prompt_file(&prompt_path, contents)?;
    }

    tmux::send_keys(&session_name, &agent_cmd)?;

    // Second window: bare terminal for ad-hoc commands
    tmux::create_window(&session_name, "terminal", cwd)?;

    // For OpenCode/Codex, detect session IDs after launch
    let agent_session_id = match pre_assigned_session_id {
        Some(id) => Some(id),
        None => match issue.agent_kind {
            AgentKind::OpenCode => detect_opencode_session_id(),
            AgentKind::Claude => None,
            AgentKind::Codex => detect_codex_session_id(),
            AgentKind::Pi => detect_pi_session_id(&config.project_root),
        },
    };

    Ok((session_name, agent_session_id))
}

/// Path for an issue's staged prompt file, scoped to its session name so
/// concurrent launches don't collide.
fn prompt_file_path(status_dir: &Path, session_name: &str) -> PathBuf {
    status_dir.join(format!("prompt-{session_name}.txt"))
}

/// Write the prompt to disk (creating the parent dir if needed) so the launch
/// command can read it back verbatim.
fn write_prompt_file(path: &Path, contents: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    fs::write(path, contents).map_err(AppError::Io)
}

/// Build the agent launch command and return
/// (command, pre_assigned_session_id, prompt_contents).
/// For Claude, pre-assigns a UUID and returns it. For OpenCode, returns None (ID detected post-launch).
/// `prompt_contents` is `Some` for fresh sessions (staged to a file by the
/// caller) and `None` for resume sessions, which carry no prompt.
fn build_agent_cmd(
    issue: &Issue,
    config: &AppConfig,
    session_name: &str,
    status_dir_str: &str,
    prompt_path_str: &str,
) -> (String, Option<String>, Option<String>) {
    let env_prefix = format!(
        "export BORK_SESSION='{}' BORK_STATUS_DIR='{}'",
        shell_escape_single_quotes(session_name),
        shell_escape_single_quotes(status_dir_str),
    );

    // Builds the full issue prompt text. Lazy: only invoked for fresh sessions,
    // since resume paths skip the prompt entirely. Returned verbatim (no shell
    // escaping) because it's staged to a file and read back via `"$(cat ...)"`.
    let build_prompt_contents = || {
        let default_prompt = config
            .default_prompt
            .as_deref()
            .unwrap_or(config::DEFAULT_PROMPT_FALLBACK);
        let main_worktree = config.project_root.join("main");
        let mut prompt = build_prompt(
            &issue.id,
            &issue.title,
            default_prompt,
            issue.prompt.as_deref(),
            &issue.linear_links,
            &issue.github_pr_links,
            |number| github::pr_url(&main_worktree, number),
        );
        prompt.push_str("\n\nBork project: ");
        prompt.push_str(&config.project_name);
        if let Some(worktree) = issue.worktree.as_deref() {
            prompt.push_str("\n\nAssigned worktree: ");
            prompt.push_str(worktree);
            prompt.push_str(". Do all work for this issue inside that directory.");
        }
        prompt
    };

    // Shell snippet that expands to the prompt at runtime by reading the staged
    // file, plus a cleanup suffix that removes the file afterwards. Using a file
    // sidesteps tmux send-keys mangling newlines in the typed command line.
    let escaped_prompt_path = shell_escape_single_quotes(prompt_path_str);
    let prompt_subst = format!("\"$(cat '{}')\"", escaped_prompt_path);
    let prompt_cleanup = format!("; rm -f '{}'", escaped_prompt_path);

    // Built-in mode flags. These are replaced when the user configured
    // per-mode args under `[agent.<name>.mode.<mode>]`.
    let builtin_mode_flag = match issue.agent_kind {
        AgentKind::OpenCode => match issue.agent_mode {
            // OpenCode has no yolo mode; treat it as Build.
            AgentMode::Plan => "--agent plan",
            AgentMode::Build | AgentMode::Yolo => "",
        },
        AgentKind::Claude => match issue.agent_mode {
            AgentMode::Plan => "--permission-mode plan",
            AgentMode::Yolo => "--dangerously-skip-permissions",
            AgentMode::Build => "",
        },
        AgentKind::Codex => match issue.agent_mode {
            // `--full-auto` is deprecated upstream; use explicit sandbox + approval flags.
            AgentMode::Plan => "--sandbox workspace-write --ask-for-approval on-request",
            AgentMode::Build => "--sandbox workspace-write --ask-for-approval never",
            AgentMode::Yolo => "--dangerously-bypass-approvals-and-sandbox",
        },
        // Pi has a single mode and no built-in plan/yolo flags. Users can still
        // add per-mode args via `[agent.pi.mode.<mode>]` if desired.
        AgentKind::Pi => "",
    };

    let trailing = trailing_args(
        config,
        issue.agent_kind,
        issue.agent_mode,
        builtin_mode_flag,
    );

    match issue.agent_kind {
        AgentKind::OpenCode => {
            if let Some(ref sid) = issue.session_id {
                // Resume existing session — skip --prompt, history is preserved
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && opencode --session '{}'{}",
                    env_prefix, escaped_sid, trailing,
                );
                (cmd, None, None)
            } else {
                let cmd = format!(
                    "{} && opencode --prompt {}{}{}",
                    env_prefix, prompt_subst, trailing, prompt_cleanup,
                );
                (cmd, None, Some(build_prompt_contents()))
            }
        }
        AgentKind::Claude => {
            let session_display_name = format!("{}: {}", issue.id, issue.title);
            let escaped_name = shell_escape_single_quotes(&session_display_name);

            if let Some(ref sid) = issue.session_id {
                // Resume existing session — skip the prompt, history is preserved
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && claude --name '{}'{} --resume '{}'",
                    env_prefix, escaped_name, trailing, escaped_sid,
                );
                (cmd, Some(sid.clone()), None)
            } else {
                // Fresh session: stage prompt and optionally pre-assign a UUID
                let prompt = build_prompt_contents();
                let uuid = generate_uuid().unwrap_or_default();
                if uuid.is_empty() {
                    let cmd = format!(
                        "{} && claude --name '{}'{} {}{}",
                        env_prefix, escaped_name, trailing, prompt_subst, prompt_cleanup,
                    );
                    (cmd, None, Some(prompt))
                } else {
                    let escaped_uuid = shell_escape_single_quotes(&uuid);
                    let cmd = format!(
                        "{} && claude --name '{}'{} --session-id '{}' {}{}",
                        env_prefix,
                        escaped_name,
                        trailing,
                        escaped_uuid,
                        prompt_subst,
                        prompt_cleanup,
                    );
                    (cmd, Some(uuid), Some(prompt))
                }
            }
        }
        AgentKind::Codex => {
            if let Some(ref sid) = issue.session_id {
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && codex resume '{}'{}",
                    env_prefix, escaped_sid, trailing
                );
                (cmd, Some(sid.clone()), None)
            } else {
                let cmd = format!(
                    "{} && codex{} {}{}",
                    env_prefix, trailing, prompt_subst, prompt_cleanup,
                );
                (cmd, None, Some(build_prompt_contents()))
            }
        }
        AgentKind::Pi => {
            let session_display_name = format!("{}: {}", issue.id, issue.title);
            let escaped_name = shell_escape_single_quotes(&session_display_name);

            if let Some(ref sid) = issue.session_id {
                // Resume existing session — skip the prompt, history is preserved.
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && pi --session '{}'{}",
                    env_prefix, escaped_sid, trailing,
                );
                (cmd, Some(sid.clone()), None)
            } else {
                let cmd = format!(
                    "{} && pi --name '{}'{} {}{}",
                    env_prefix, escaped_name, trailing, prompt_subst, prompt_cleanup,
                );
                (cmd, None, Some(build_prompt_contents()))
            }
        }
    }
}

/// Build the trailing args string (always starts with a leading space when
/// non-empty) that gets appended to each agent invocation.
///
/// Resolution:
/// - Base args from `[agent.<name>].args` are always appended.
/// - Per-mode args from `[agent.<name>.mode.<mode>].args` replace the
///   built-in mode flags. Set to `[]` to suppress mode flags entirely.
/// - When no per-mode override is configured, bork's built-in mode flags
///   are used.
///
/// All configured args are individually shell-escaped so values containing
/// spaces or quotes are passed through safely.
fn trailing_args(
    config: &AppConfig,
    kind: AgentKind,
    mode: AgentMode,
    builtin_mode_flag: &str,
) -> String {
    let (base_args, mode_args_override) = config.launch_args_for(kind, mode);

    let mut parts: Vec<String> = Vec::new();
    match mode_args_override {
        Some(args) => {
            for arg in args {
                parts.push(format!("'{}'", shell_escape_single_quotes(arg)));
            }
        }
        None => {
            let trimmed = builtin_mode_flag.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    for arg in base_args {
        parts.push(format!("'{}'", shell_escape_single_quotes(arg)));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

/// Generate a UUID using the system `uuidgen` command.
fn generate_uuid() -> Option<String> {
    Command::new("uuidgen")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
}

/// Poll `opencode session list` to detect a newly created session.
/// Returns the session ID if found within ~5 seconds, otherwise None.
fn detect_opencode_session_id() -> Option<String> {
    // Give OpenCode a moment to create its session before polling
    std::thread::sleep(Duration::from_millis(800));

    for _ in 0..9 {
        if let Some(sid) = newest_opencode_session() {
            return Some(sid);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Detect a newly created Codex session UUID by scanning ~/.codex/sessions.
/// Snapshots existing sessions before waiting, then polls for a new one.
fn detect_codex_session_id() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let sessions_root = PathBuf::from(home).join(".codex").join("sessions");

    let before = collect_codex_session_ids(&sessions_root);

    std::thread::sleep(Duration::from_millis(800));

    for _ in 0..9 {
        let after = collect_codex_session_ids(&sessions_root);
        for id in after.keys() {
            if !before.contains_key(id) {
                return Some(id.clone());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    // Fallback: return the newest session if no new one appeared
    newest_codex_session_id(&sessions_root)
}

/// Collect all Codex session IDs and their modification times.
fn collect_codex_session_ids(sessions_root: &Path) -> HashMap<String, SystemTime> {
    let mut sessions = HashMap::new();
    let mut stack = vec![sessions_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(session_id) = parse_codex_session_id_from_filename(file_name) else {
                continue;
            };
            let modified = fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            sessions.insert(session_id, modified);
        }
    }

    sessions
}

fn newest_codex_session_id(sessions_root: &Path) -> Option<String> {
    collect_codex_session_ids(sessions_root)
        .into_iter()
        .max_by_key(|(_, modified)| *modified)
        .map(|(id, _)| id)
}

fn parse_codex_session_id_from_filename(file_name: &str) -> Option<String> {
    let stem = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    if stem.len() < 36 {
        return None;
    }
    let candidate = &stem[stem.len() - 36..];
    if is_uuid_like(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}

fn is_uuid_like(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.chars().enumerate().all(|(i, ch)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            ch == '-'
        } else {
            ch.is_ascii_hexdigit()
        }
    })
}

/// Detect a newly created Pi session UUID by scanning Pi's per-cwd session
/// directory. Snapshots existing sessions before waiting, then polls for a new
/// one. Pi stores sessions under `<sessions_root>/--<cwd>--/` as
/// `<timestamp>_<uuid>.jsonl`, where `<cwd>` has `/` replaced by `-`.
fn detect_pi_session_id(project_root: &Path) -> Option<String> {
    let sessions_dir = pi_sessions_dir(project_root)?;

    let before = collect_pi_session_ids(&sessions_dir);

    std::thread::sleep(Duration::from_millis(800));

    for _ in 0..9 {
        let after = collect_pi_session_ids(&sessions_dir);
        for id in after.keys() {
            if !before.contains_key(id) {
                return Some(id.clone());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    // Fallback: return the newest session if no new one appeared.
    collect_pi_session_ids(&sessions_dir)
        .into_iter()
        .max_by_key(|(_, modified)| *modified)
        .map(|(id, _)| id)
}

/// Resolve Pi's session directory for a given working directory.
/// Honors `PI_CODING_AGENT_SESSION_DIR` (flat dir) and `PI_CODING_AGENT_DIR`
/// overrides, falling back to `~/.pi/agent/sessions/--<cwd>--/`.
fn pi_sessions_dir(project_root: &Path) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_SESSION_DIR") {
        return Some(PathBuf::from(dir));
    }

    let root = if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        PathBuf::from(dir)
    } else {
        let home = std::env::var("HOME").ok()?;
        PathBuf::from(home).join(".pi").join("agent")
    };

    let cwd = project_root.to_str()?;
    Some(
        root.join("sessions")
            .join(format!("--{}--", cwd.replace('/', "-"))),
    )
}

/// Collect Pi session UUIDs and their modification times from a session dir.
fn collect_pi_session_ids(sessions_dir: &Path) -> HashMap<String, SystemTime> {
    let mut sessions = HashMap::new();
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(session_id) = parse_pi_session_id_from_filename(file_name) else {
            continue;
        };
        let modified = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        sessions.insert(session_id, modified);
    }
    sessions
}

/// Extract the session UUID from a Pi session filename (`<timestamp>_<uuid>.jsonl`).
fn parse_pi_session_id_from_filename(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".jsonl")?;
    let candidate = stem.rsplit('_').next()?;
    if is_uuid_like(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// Run `opencode session list` and return the first (newest) session ID found.
/// Session IDs start with "ses_".
fn newest_opencode_session() -> Option<String> {
    let output = Command::new("opencode")
        .args(["session", "list"])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_newest_session_id(&stdout)
}

/// Parse the newest session ID from `opencode session list` output.
/// Expected format: each line starts with the session ID (ses_xxx).
fn parse_newest_session_id(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let token = line.split_whitespace().next()?;
        if token.starts_with("ses_") {
            Some(token.to_string())
        } else {
            None
        }
    })
}

/// Build the full prompt sent to the agent.
/// Always starts with issue context and the default prompt, then optionally
/// includes Linear tickets and GitHub PRs linked to the issue, then appends
/// the user's custom prompt (if any) after a blank line.
///
/// `pr_url_resolver` is called once per linked PR to fetch its canonical URL.
/// If it returns `None` (e.g. `gh` not available, non-GitHub remote), the PR
/// is rendered as `#{number}` with no URL.
fn build_prompt(
    id: &str,
    title: &str,
    default_prompt: &str,
    user_prompt: Option<&str>,
    linear_links: &[LinkedLinear],
    github_pr_links: &[LinkedGithubPr],
    pr_url_resolver: impl Fn(u32) -> Option<String>,
) -> String {
    let mut prompt = format!("You are working on {}: {}. {}", id, title, default_prompt);

    append_section(
        &mut prompt,
        linear_links,
        "Linear ticket",
        "Linear tickets",
        |link| {
            if link.url.is_empty() {
                link.identifier.clone()
            } else {
                format!("{} - {}", link.identifier, link.url)
            }
        },
    );
    append_section(
        &mut prompt,
        github_pr_links,
        "GitHub PR",
        "GitHub PRs",
        |link| match pr_url_resolver(link.number) {
            Some(url) => format!("#{} - {}", link.number, url),
            None => format!("#{}", link.number),
        },
    );

    if let Some(user_text) = user_prompt {
        let trimmed = user_text.trim();
        if !trimmed.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(trimmed);
        }
    }

    prompt
}

fn append_section<T>(
    prompt: &mut String,
    items: &[T],
    singular: &str,
    plural: &str,
    format_entry: impl Fn(&T) -> String,
) {
    use std::fmt::Write;
    match items {
        [] => {}
        [single] => {
            let _ = write!(
                prompt,
                "\n\nThis issue has a {}: {}",
                singular,
                format_entry(single)
            );
        }
        many => {
            let _ = write!(prompt, "\n\nThis issue has {}:", plural);
            for item in many {
                let _ = write!(prompt, "\n- {}", format_entry(item));
            }
        }
    }
}

/// Escape a string for use inside single quotes in a shell command.
fn shell_escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DEFAULT: &str = "The source code is in main/.";

    fn linear(identifier: &str, url: &str) -> LinkedLinear {
        LinkedLinear {
            id: format!("uuid-{}", identifier),
            identifier: identifier.to_string(),
            url: url.to_string(),
            imported: false,
        }
    }

    fn pr(number: u32) -> LinkedGithubPr {
        LinkedGithubPr {
            number,
            imported: false,
            import_source: None,
        }
    }

    /// PR URL resolver that always returns None (simulates no `gh` available).
    fn no_pr_url(_: u32) -> Option<String> {
        None
    }

    /// PR URL resolver that always returns a stable test URL.
    fn test_pr_url(number: u32) -> Option<String> {
        Some(format!("https://github.com/test/repo/pull/{}", number))
    }

    // --- append_section ---

    #[test]
    fn append_section_empty_is_noop() {
        let mut out = String::from("base");
        let empty: [u32; 0] = [];
        append_section(&mut out, &empty, "thing", "things", |n| n.to_string());
        assert_eq!(out, "base");
    }

    #[test]
    fn append_section_single_uses_singular_label() {
        let mut out = String::from("base");
        append_section(&mut out, &[42u32], "thing", "things", |n| n.to_string());
        assert_eq!(out, "base\n\nThis issue has a thing: 42");
    }

    #[test]
    fn append_section_multiple_uses_plural_and_bullet_list() {
        let mut out = String::from("base");
        append_section(&mut out, &[1u32, 2, 3], "thing", "things", |n| {
            n.to_string()
        });
        assert_eq!(out, "base\n\nThis issue has things:\n- 1\n- 2\n- 3");
    }

    #[test]
    fn append_section_uses_formatter_closure() {
        let mut out = String::new();
        append_section(&mut out, &[7u32], "PR", "PRs", |n| format!("#{}", n));
        assert_eq!(out, "\n\nThis issue has a PR: #7");
    }

    #[test]
    fn append_section_formatter_applied_to_each_item() {
        let mut out = String::new();
        append_section(&mut out, &[1u32, 2], "PR", "PRs", |n| format!("#{}", n));
        assert_eq!(out, "\n\nThis issue has PRs:\n- #1\n- #2");
    }

    #[test]
    fn append_section_appends_to_existing_content() {
        let mut out = String::from("prefix\n\nmiddle");
        append_section(&mut out, &[1u32], "x", "xs", |n| n.to_string());
        assert_eq!(out, "prefix\n\nmiddle\n\nThis issue has a x: 1");
    }

    #[test]
    fn append_section_works_with_linked_linear() {
        let links = [linear("VIL-1", "https://linear.app/VIL-1")];
        let mut out = String::new();
        append_section(&mut out, &links, "Linear ticket", "Linear tickets", |l| {
            if l.url.is_empty() {
                l.identifier.clone()
            } else {
                format!("{} - {}", l.identifier, l.url)
            }
        });
        assert_eq!(
            out,
            "\n\nThis issue has a Linear ticket: VIL-1 - https://linear.app/VIL-1"
        );
    }

    #[test]
    fn append_section_works_with_linked_github_pr() {
        let prs = [pr(10), pr(11)];
        let mut out = String::new();
        append_section(&mut out, &prs, "GitHub PR", "GitHub PRs", |p| {
            format!("#{}", p.number)
        });
        assert_eq!(out, "\n\nThis issue has GitHub PRs:\n- #10\n- #11");
    }

    // --- build_prompt ---

    #[test]
    fn build_prompt_without_user_prompt() {
        let result = build_prompt(
            "bork-1",
            "Fix auth",
            TEST_DEFAULT,
            None,
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-1: Fix auth. The source code is in main/."
        );
    }

    #[test]
    fn build_prompt_with_user_prompt() {
        let result = build_prompt(
            "bork-2",
            "Add tests",
            TEST_DEFAULT,
            Some("Focus on unit tests"),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-2: Add tests. The source code is in main/.\n\nFocus on unit tests"
        );
    }

    #[test]
    fn build_prompt_with_empty_user_prompt() {
        let result = build_prompt(
            "bork-3",
            "Refactor",
            TEST_DEFAULT,
            Some(""),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-3: Refactor. The source code is in main/."
        );
    }

    #[test]
    fn build_prompt_with_whitespace_only_user_prompt() {
        let result = build_prompt(
            "bork-4",
            "Cleanup",
            TEST_DEFAULT,
            Some("   \n  "),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-4: Cleanup. The source code is in main/."
        );
    }

    #[test]
    fn build_prompt_user_prompt_is_trimmed() {
        let result = build_prompt(
            "bork-5",
            "Feature",
            TEST_DEFAULT,
            Some("  do the thing  "),
            &[],
            &[],
            no_pr_url,
        );
        assert!(result.ends_with("\n\ndo the thing"));
    }

    #[test]
    fn build_prompt_with_real_default_fallback() {
        let result = build_prompt(
            "bork-6",
            "New feature",
            config::DEFAULT_PROMPT_FALLBACK,
            None,
            &[],
            &[],
            no_pr_url,
        );
        assert!(result.starts_with("You are working on bork-6: New feature."));
        assert!(result.contains("source code is in main/"));
        assert!(result.contains("bork issue start"));
    }

    #[test]
    fn build_prompt_with_custom_default_prompt() {
        let result = build_prompt(
            "proj-1",
            "Setup",
            "Read README.md first.",
            Some("Install deps"),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on proj-1: Setup. Read README.md first.\n\nInstall deps"
        );
    }

    #[test]
    fn build_prompt_with_single_linear_link() {
        let links = [linear("VIL-123", "https://linear.app/team/issue/VIL-123")];
        let result = build_prompt(
            "vil-123",
            "Fix auth flow",
            TEST_DEFAULT,
            None,
            &links,
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on vil-123: Fix auth flow. The source code is in main/.\n\nThis issue has a Linear ticket: VIL-123 - https://linear.app/team/issue/VIL-123"
        );
    }

    #[test]
    fn build_prompt_with_linear_link_missing_url() {
        let links = [linear("VIL-123", "")];
        let result = build_prompt(
            "bork-1",
            "Fix bug",
            TEST_DEFAULT,
            None,
            &links,
            &[],
            no_pr_url,
        );
        assert!(result.ends_with("\n\nThis issue has a Linear ticket: VIL-123"));
        assert!(!result.contains("VIL-123 -"));
    }

    #[test]
    fn build_prompt_with_multiple_linear_links() {
        let links = [
            linear("VIL-1", "https://linear.app/team/issue/VIL-1"),
            linear("VIL-2", "https://linear.app/team/issue/VIL-2"),
        ];
        let result = build_prompt(
            "bork-1",
            "Refactor",
            TEST_DEFAULT,
            None,
            &links,
            &[],
            no_pr_url,
        );
        assert!(result.contains(
            "\n\nThis issue has Linear tickets:\n- VIL-1 - https://linear.app/team/issue/VIL-1\n- VIL-2 - https://linear.app/team/issue/VIL-2"
        ));
    }

    #[test]
    fn build_prompt_with_single_github_pr_and_url() {
        let prs = [pr(42)];
        let result = build_prompt(
            "bork-1",
            "Fix bug",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            test_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-1: Fix bug. The source code is in main/.\n\nThis issue has a GitHub PR: #42 - https://github.com/test/repo/pull/42"
        );
    }

    #[test]
    fn build_prompt_with_single_github_pr_without_url() {
        let prs = [pr(42)];
        let result = build_prompt(
            "bork-1",
            "Fix bug",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-1: Fix bug. The source code is in main/.\n\nThis issue has a GitHub PR: #42"
        );
    }

    #[test]
    fn build_prompt_with_multiple_github_prs_and_urls() {
        let prs = [pr(1), pr(2), pr(3)];
        let result = build_prompt(
            "bork-1",
            "Refactor",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            test_pr_url,
        );
        assert!(result.contains(
            "\n\nThis issue has GitHub PRs:\n- #1 - https://github.com/test/repo/pull/1\n- #2 - https://github.com/test/repo/pull/2\n- #3 - https://github.com/test/repo/pull/3"
        ));
    }

    #[test]
    fn build_prompt_with_multiple_github_prs_without_urls() {
        let prs = [pr(1), pr(2), pr(3)];
        let result = build_prompt(
            "bork-1",
            "Refactor",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            no_pr_url,
        );
        assert!(result.contains("\n\nThis issue has GitHub PRs:\n- #1\n- #2\n- #3"));
        assert!(!result.contains("github.com"));
    }

    #[test]
    fn build_prompt_pr_url_resolved_per_link() {
        // Resolver returns Some for #1, None for #2 — exercise both branches in one prompt.
        let prs = [pr(1), pr(2)];
        let resolver = |n: u32| {
            if n == 1 {
                Some("https://github.com/test/repo/pull/1".to_string())
            } else {
                None
            }
        };
        let result = build_prompt("bork-1", "Mixed", TEST_DEFAULT, None, &[], &prs, resolver);
        assert!(result.contains("- #1 - https://github.com/test/repo/pull/1"));
        assert!(result.contains("- #2\n") || result.ends_with("- #2"));
    }

    #[test]
    fn build_prompt_with_both_linear_and_pr() {
        let links = [linear("VIL-123", "https://linear.app/team/issue/VIL-123")];
        let prs = [pr(7)];
        let result = build_prompt(
            "vil-123",
            "Fix auth",
            TEST_DEFAULT,
            None,
            &links,
            &prs,
            test_pr_url,
        );
        assert_eq!(
            result,
            "You are working on vil-123: Fix auth. The source code is in main/.\n\nThis issue has a Linear ticket: VIL-123 - https://linear.app/team/issue/VIL-123\n\nThis issue has a GitHub PR: #7 - https://github.com/test/repo/pull/7"
        );
    }

    #[test]
    fn build_prompt_ordering_with_user_prompt() {
        let links = [linear("VIL-123", "https://linear.app/team/issue/VIL-123")];
        let prs = [pr(7)];
        let result = build_prompt(
            "vil-123",
            "Fix auth",
            TEST_DEFAULT,
            Some("Focus on OAuth"),
            &links,
            &prs,
            test_pr_url,
        );
        assert!(result.contains("The source code is in main/."));
        assert!(result.contains(
            "\n\nThis issue has a Linear ticket: VIL-123 - https://linear.app/team/issue/VIL-123"
        ));
        assert!(result
            .contains("\n\nThis issue has a GitHub PR: #7 - https://github.com/test/repo/pull/7"));
        assert!(result.ends_with("\n\nFocus on OAuth"));
    }

    #[test]
    fn build_prompt_without_links_has_no_integration_lines() {
        let result = build_prompt(
            "bork-7",
            "Add feature",
            TEST_DEFAULT,
            None,
            &[],
            &[],
            no_pr_url,
        );
        assert!(!result.contains("Linear"));
        assert!(!result.contains("GitHub PR"));
    }

    fn test_issue(agent_kind: AgentKind, agent_mode: AgentMode) -> Issue {
        Issue {
            agent_mode,
            ..Issue::new(
                "bork-1",
                "Fix bug",
                crate::types::Column::InProgress,
                agent_kind,
            )
        }
    }

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".to_string(),
            project_root: std::path::PathBuf::from("/tmp/test"),
            agent_kind: AgentKind::OpenCode,
            default_prompt: Some("The source code is in main/.".to_string()),
            review_prompt: None,
            done_session_ttl: 300,
            debug: false,
            agents_allowlist: None,
            agent_launch: std::collections::HashMap::new(),
        }
    }

    /// Test wrapper: invoke build_agent_cmd with a fixed prompt-file path and
    /// return (cmd, pre_assigned_sid, prompt_contents). The prompt-file path is
    /// fixed so tests can also assert on the `"$(cat ...)"` substitution.
    const TEST_PROMPT_PATH: &str = "/tmp/status/prompt-bork-bork-1.txt";

    fn agent_cmd(
        issue: &Issue,
        config: &AppConfig,
        session: &str,
        status_dir: &str,
    ) -> (String, Option<String>, Option<String>) {
        build_agent_cmd(issue, config, session, status_dir, TEST_PROMPT_PATH)
    }

    // --- build_agent_cmd ---

    #[test]
    fn opencode_fresh_plan() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt \"$(cat '/tmp/status/prompt-bork-bork-1.txt')\""));
        assert!(cmd.contains("rm -f '/tmp/status/prompt-bork-bork-1.txt'"));
        assert!(cmd.contains("--agent plan"));
        assert!(cmd.contains("BORK_SESSION='bork-bork-1'"));
        assert!(cmd.contains("BORK_STATUS_DIR='/tmp/status'"));
        assert!(sid.is_none());
    }

    #[test]
    fn opencode_fresh_build() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt"));
        assert!(!cmd.contains("--agent plan"));
    }

    #[test]
    fn fresh_prompt_includes_project_name() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(prompt.unwrap().contains("Bork project: bork"));
    }

    #[test]
    fn fresh_prompt_includes_assigned_worktree() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.worktree = Some("bork-1-fix-bug".to_string());
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        let prompt = prompt.unwrap();
        assert!(prompt.contains("Assigned worktree: bork-1-fix-bug"));
        assert!(prompt.contains("inside that directory"));
    }

    #[test]
    fn opencode_fresh_yolo_treated_as_build() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt"));
        assert!(!cmd.contains("--agent plan"));
        assert!(!cmd.contains("yolo"));
    }

    #[test]
    fn opencode_resume() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Plan);
        issue.session_id = Some("ses_abc123".to_string());
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123'"));
        assert!(cmd.contains("--agent plan"));
        assert!(!cmd.contains("--prompt"));
        assert!(sid.is_none());
    }

    #[test]
    fn opencode_resume_build() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.session_id = Some("ses_abc123".to_string());
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123'"));
        assert!(!cmd.contains("--agent plan"));
    }

    #[test]
    fn claude_fresh_plan() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("claude --name"));
        assert!(cmd.contains("--permission-mode plan"));
        assert!(!cmd.contains("--resume"));
        // Fresh Claude gets a pre-assigned session ID (uuid) only if uuidgen works,
        // but in tests it may or may not be available, so we check the command structure
        if let Some(ref id) = sid {
            assert!(cmd.contains("--session-id"));
            assert!(!id.is_empty());
        }
    }

    #[test]
    fn claude_fresh_build() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("claude --name"));
        assert!(!cmd.contains("--permission-mode plan"));
        assert!(!cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn claude_fresh_yolo() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(!cmd.contains("--permission-mode plan"));
    }

    #[test]
    fn claude_resume() {
        let mut issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        issue.session_id = Some("uuid-123-456".to_string());
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("claude --name"));
        assert!(cmd.contains("--resume 'uuid-123-456'"));
        assert!(cmd.contains("--permission-mode plan"));
        assert!(!cmd.contains("--prompt"));
        assert_eq!(sid, Some("uuid-123-456".to_string()));
    }

    #[test]
    fn claude_resume_yolo() {
        let mut issue = test_issue(AgentKind::Claude, AgentMode::Yolo);
        issue.session_id = Some("uuid-789".to_string());
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--resume 'uuid-789'"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn codex_fresh_plan() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --sandbox workspace-write --ask-for-approval on-request"));
        assert!(prompt
            .unwrap()
            .contains("You are working on bork-1: Fix bug"));
        assert!(sid.is_none());
    }

    #[test]
    fn codex_fresh_build_uses_workspace_write_never() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --sandbox workspace-write --ask-for-approval never"));
        assert!(!cmd.contains("--full-auto"));
        assert!(!cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn codex_fresh_yolo() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(!cmd.contains("--full-auto"));
        assert!(!cmd.contains("workspace-write"));
    }

    #[test]
    fn codex_resume_uses_session_id() {
        let mut issue = test_issue(AgentKind::Codex, AgentMode::Build);
        issue.session_id = Some("019d76ad-9734-77c0-8169-a727a5524013".to_string());
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains(
            "codex resume '019d76ad-9734-77c0-8169-a727a5524013' --sandbox workspace-write --ask-for-approval never"
        ));
        assert!(!cmd.contains("--prompt"));
        assert_eq!(
            sid,
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn fresh_prompt_staged_to_file_not_inlined() {
        // A multiline prompt with characters that break naive shell quoting must
        // be staged verbatim to the file, while the command references it via
        // `"$(cat ...)"` and never inlines the prompt text.
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.prompt = Some("line one\nline `two`\n$VAR and \"quotes\"".to_string());
        let config = test_config();
        let (cmd, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");

        let prompt = prompt.expect("fresh session should stage a prompt file");
        assert!(prompt.contains("line one\nline `two`\n$VAR and \"quotes\""));

        // Command must read the file, not embed the multiline text.
        assert!(cmd.contains("\"$(cat '/tmp/status/prompt-bork-bork-1.txt')\""));
        assert!(!cmd.contains("line `two`"));
        // No literal newlines in the typed command line (send-keys would mangle them).
        assert!(!cmd.contains('\n'));
    }

    #[test]
    fn prompt_file_path_is_scoped_to_session() {
        let path = prompt_file_path(Path::new("/tmp/status"), "bork-bork-7");
        assert_eq!(path, PathBuf::from("/tmp/status/prompt-bork-bork-7.txt"));
    }

    #[test]
    fn pi_fresh_uses_name_and_prompt() {
        let issue = test_issue(AgentKind::Pi, AgentMode::Build);
        let config = test_config();
        let (cmd, sid, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("pi --name 'bork-1: Fix bug'"));
        assert!(prompt
            .unwrap()
            .contains("You are working on bork-1: Fix bug"));
        // Pi has no built-in mode flags.
        assert!(!cmd.contains("--agent plan"));
        assert!(!cmd.contains("--permission-mode"));
        assert!(!cmd.contains("--sandbox"));
        assert!(sid.is_none());
    }

    #[test]
    fn pi_single_mode_ignores_agent_mode() {
        // Pi behaves identically regardless of the stored agent_mode.
        let config = test_config();
        let plan = agent_cmd(
            &test_issue(AgentKind::Pi, AgentMode::Plan),
            &config,
            "bork-bork-1",
            "/tmp/status",
        )
        .0;
        let build = agent_cmd(
            &test_issue(AgentKind::Pi, AgentMode::Build),
            &config,
            "bork-bork-1",
            "/tmp/status",
        )
        .0;
        assert_eq!(plan, build);
    }

    #[test]
    fn pi_resume_uses_session_id() {
        let mut issue = test_issue(AgentKind::Pi, AgentMode::Build);
        issue.session_id = Some("019d76ad-9734-77c0-8169-a727a5524013".to_string());
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("pi --session '019d76ad-9734-77c0-8169-a727a5524013'"));
        assert!(!cmd.contains("--prompt"));
        assert!(!cmd.contains("--name"));
        assert_eq!(
            sid,
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn pi_session_id_parsed_from_filename() {
        assert_eq!(
            parse_pi_session_id_from_filename(
                "2024-12-03T14-00-00_019d76ad-9734-77c0-8169-a727a5524013.jsonl"
            ),
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
        assert_eq!(parse_pi_session_id_from_filename("not-a-session.txt"), None);
        assert_eq!(parse_pi_session_id_from_filename("123_short.jsonl"), None);
    }

    #[test]
    fn cmd_env_prefix() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "my-session", "/path/to/status");
        assert!(
            cmd.starts_with("export BORK_SESSION='my-session' BORK_STATUS_DIR='/path/to/status'")
        );
    }

    fn config_with_launch(
        kind: AgentKind,
        base: &[&str],
        mode_overrides: &[(AgentMode, &[&str])],
    ) -> AppConfig {
        let mut config = test_config();
        let launch = crate::config::AgentLaunchConfig {
            args: base.iter().map(|s| s.to_string()).collect(),
            mode_args: mode_overrides
                .iter()
                .map(|(m, args)| (*m, args.iter().map(|s| s.to_string()).collect()))
                .collect(),
        };
        config.agent_launch.insert(kind, launch);
        config
    }

    #[test]
    fn cmd_escapes_single_quotes_in_session_name() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "it's-a-test", "/tmp/status");
        assert!(cmd.contains("BORK_SESSION='it'\\''s-a-test'"));
    }

    // --- agent_launch / trailing_args integration ---

    #[test]
    fn claude_base_args_are_appended() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = config_with_launch(AgentKind::Claude, &["--verbose"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains(" '--verbose' "));
    }

    #[test]
    fn claude_mode_override_replaces_builtin_flag() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        let config = config_with_launch(
            AgentKind::Claude,
            &[],
            &[(AgentMode::Plan, &["--dangerously-skip-permissions"])],
        );
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("'--dangerously-skip-permissions'"));
        assert!(!cmd.contains("--permission-mode plan"));
    }

    #[test]
    fn claude_empty_mode_override_clears_builtin_flag() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        let config = config_with_launch(AgentKind::Claude, &[], &[(AgentMode::Plan, &[])]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("--permission-mode plan"));
    }

    #[test]
    fn opencode_extra_args_appended_on_resume() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.session_id = Some("ses_abc123".to_string());
        let config = config_with_launch(AgentKind::OpenCode, &["--quiet"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123' '--quiet'"));
    }

    #[test]
    fn codex_mode_override_replaces_sandbox_flags() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Build);
        let config = config_with_launch(
            AgentKind::Codex,
            &[],
            &[(AgentMode::Build, &["--sandbox", "read-only"])],
        );
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("'--sandbox' 'read-only'"));
        assert!(!cmd.contains("workspace-write"));
        assert!(!cmd.contains("--ask-for-approval never"));
    }

    #[test]
    fn launcher_does_not_affect_other_agents() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = config_with_launch(AgentKind::OpenCode, &["--quiet"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("--quiet"));
    }

    #[test]
    fn configured_args_are_individually_shell_escaped() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = config_with_launch(AgentKind::Claude, &["it's a flag"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("'it'\\''s a flag'"));
    }

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape_single_quotes("hello world"), "hello world");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape_single_quotes("it's a test"), "it'\\''s a test");
    }

    #[test]
    fn parse_newest_session_id_finds_first_ses_entry() {
        let output = "ses_abc123   My session title   2024-01-15\nses_def456   Another session   2024-01-14\n";
        assert_eq!(
            parse_newest_session_id(output),
            Some("ses_abc123".to_string())
        );
    }

    #[test]
    fn parse_newest_session_id_returns_none_for_empty_output() {
        assert_eq!(parse_newest_session_id(""), None);
    }

    #[test]
    fn parse_newest_session_id_ignores_non_ses_lines() {
        let output = "No sessions found\n";
        assert_eq!(parse_newest_session_id(output), None);
    }

    #[test]
    fn parse_codex_session_id_from_filename_extracts_uuid() {
        let file_name = "rollout-2026-04-10T11-16-21-019d76ad-9734-77c0-8169-a727a5524013.jsonl";
        assert_eq!(
            parse_codex_session_id_from_filename(file_name),
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn parse_codex_session_id_from_filename_rejects_invalid() {
        let file_name = "rollout-2026-04-10T11-16-21-not-a-uuid.jsonl";
        assert_eq!(parse_codex_session_id_from_filename(file_name), None);
    }

    #[test]
    fn is_uuid_like_validates_expected_shape() {
        assert!(is_uuid_like("019d76ad-9734-77c0-8169-a727a5524013"));
        assert!(!is_uuid_like("019d76ad973477c08169a727a5524013"));
        assert!(!is_uuid_like("not-a-uuid"));
    }
}
