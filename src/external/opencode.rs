use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{self, AppConfig};
use crate::error::AppError;
use crate::external::tmux;
use crate::types::{AgentKind, AgentMode, Issue};

/// Launch an agent session for an issue.
/// Creates a tmux session with two windows:
///   1. The agent (opencode/claude/codex) launched at the project root with issue context
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

    let (agent_cmd, pre_assigned_session_id) =
        build_agent_cmd(issue, config, &session_name, status_dir_str);

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
        },
    };

    Ok((session_name, agent_session_id))
}

/// Build the agent launch command and return (command, pre_assigned_session_id).
/// For Claude, pre-assigns a UUID and returns it. For OpenCode, returns None (ID detected post-launch).
/// If the issue already has a session_id, builds a resume command instead.
fn build_agent_cmd(
    issue: &Issue,
    config: &AppConfig,
    session_name: &str,
    status_dir_str: &str,
) -> (String, Option<String>) {
    let env_prefix = format!(
        "export BORK_SESSION='{}' BORK_STATUS_DIR='{}'",
        shell_escape_single_quotes(session_name),
        shell_escape_single_quotes(status_dir_str),
    );

    match issue.agent_kind {
        AgentKind::OpenCode => {
            if let Some(ref sid) = issue.session_id {
                // Resume existing OpenCode session — skip --prompt, history is preserved
                let escaped_sid = shell_escape_single_quotes(sid);
                // Yolo is Claude-only; treat as Build for OpenCode
                let mode_flag = match issue.agent_mode {
                    AgentMode::Plan => " --agent plan",
                    AgentMode::Build | AgentMode::Yolo => "",
                };
                let cmd = format!(
                    "{} && opencode --session '{}'{}",
                    env_prefix, escaped_sid, mode_flag,
                );
                (cmd, None)
            } else {
                let default_prompt = config
                    .default_prompt
                    .as_deref()
                    .unwrap_or(config::DEFAULT_PROMPT_FALLBACK);
                let prompt = build_prompt(
                    &issue.id,
                    &issue.title,
                    default_prompt,
                    issue.prompt.as_deref(),
                    issue.linear_url.as_deref(),
                );
                let escaped_prompt = shell_escape_single_quotes(&prompt);
                // Yolo is Claude-only; treat as Build for OpenCode
                let mode_flag = match issue.agent_mode {
                    AgentMode::Plan => " --agent plan",
                    AgentMode::Build | AgentMode::Yolo => "",
                };
                let cmd = format!(
                    "{} && opencode --prompt '{}'{}",
                    env_prefix, escaped_prompt, mode_flag,
                );
                (cmd, None)
            }
        }
        AgentKind::Claude => {
            let session_display_name = format!("{}: {}", issue.id, issue.title);
            let escaped_name = shell_escape_single_quotes(&session_display_name);
            let mode_flag = match issue.agent_mode {
                AgentMode::Plan => " --permission-mode plan",
                AgentMode::Yolo => " --dangerously-skip-permissions",
                AgentMode::Build => "",
            };

            if let Some(ref sid) = issue.session_id {
                // Resume existing Claude session — skip the prompt, history is preserved
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && claude --name '{}'{} --resume '{}'",
                    env_prefix, escaped_name, mode_flag, escaped_sid,
                );
                (cmd, Some(sid.clone()))
            } else {
                // Fresh Claude session: build prompt and optionally pre-assign a UUID
                let default_prompt = config
                    .default_prompt
                    .as_deref()
                    .unwrap_or(config::DEFAULT_PROMPT_FALLBACK);
                let prompt = build_prompt(
                    &issue.id,
                    &issue.title,
                    default_prompt,
                    issue.prompt.as_deref(),
                    issue.linear_url.as_deref(),
                );
                let escaped_prompt = shell_escape_single_quotes(&prompt);

                let uuid = generate_uuid().unwrap_or_default();
                if uuid.is_empty() {
                    let cmd = format!(
                        "{} && claude --name '{}'{} '{}'",
                        env_prefix, escaped_name, mode_flag, escaped_prompt,
                    );
                    (cmd, None)
                } else {
                    let escaped_uuid = shell_escape_single_quotes(&uuid);
                    let cmd = format!(
                        "{} && claude --name '{}'{} --session-id '{}' '{}'",
                        env_prefix, escaped_name, mode_flag, escaped_uuid, escaped_prompt,
                    );
                    (cmd, Some(uuid))
                }
            }
        }
        AgentKind::Codex => {
            let mode_flag = match issue.agent_mode {
                AgentMode::Plan => " --sandbox read-only --ask-for-approval untrusted",
                AgentMode::Build => " --full-auto",
                AgentMode::Yolo => " --dangerously-bypass-approvals-and-sandbox",
            };

            if let Some(ref sid) = issue.session_id {
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && codex resume '{}'{}",
                    env_prefix, escaped_sid, mode_flag
                );
                (cmd, Some(sid.clone()))
            } else {
                let default_prompt = config
                    .default_prompt
                    .as_deref()
                    .unwrap_or(config::DEFAULT_PROMPT_FALLBACK);
                let prompt = build_prompt(
                    &issue.id,
                    &issue.title,
                    default_prompt,
                    issue.prompt.as_deref(),
                    issue.linear_url.as_deref(),
                );
                let escaped_prompt = shell_escape_single_quotes(&prompt);
                let cmd = format!("{} && codex{} '{}'", env_prefix, mode_flag, escaped_prompt);
                (cmd, None)
            }
        }
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
        for (id, _) in &after {
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
/// includes a Linear ticket URL, then appends the user's custom prompt (if
/// any) after a blank line.
fn build_prompt(
    id: &str,
    title: &str,
    default_prompt: &str,
    user_prompt: Option<&str>,
    linear_url: Option<&str>,
) -> String {
    let mut prompt = format!("You are working on {}: {}. {}", id, title, default_prompt);

    if let Some(url) = linear_url {
        prompt.push_str("\n\nThis issue has a Linear ticket: ");
        prompt.push_str(url);
    }

    if let Some(user_text) = user_prompt {
        let trimmed = user_text.trim();
        if !trimmed.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(trimmed);
        }
    }

    prompt
}

/// Escape a string for use inside single quotes in a shell command.
fn shell_escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DEFAULT: &str = "The source code is in main/.";

    #[test]
    fn build_prompt_without_user_prompt() {
        let result = build_prompt("bork-1", "Fix auth", TEST_DEFAULT, None, None);
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
            None,
        );
        assert_eq!(
            result,
            "You are working on bork-2: Add tests. The source code is in main/.\n\nFocus on unit tests"
        );
    }

    #[test]
    fn build_prompt_with_empty_user_prompt() {
        let result = build_prompt("bork-3", "Refactor", TEST_DEFAULT, Some(""), None);
        assert_eq!(
            result,
            "You are working on bork-3: Refactor. The source code is in main/."
        );
    }

    #[test]
    fn build_prompt_with_whitespace_only_user_prompt() {
        let result = build_prompt("bork-4", "Cleanup", TEST_DEFAULT, Some("   \n  "), None);
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
            None,
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
            None,
        );
        assert!(result.starts_with("You are working on bork-6: New feature."));
        assert!(result.contains("source code is in main/"));
        assert!(result.contains("bork worktree"));
    }

    #[test]
    fn build_prompt_with_custom_default_prompt() {
        let result = build_prompt(
            "proj-1",
            "Setup",
            "Read README.md first.",
            Some("Install deps"),
            None,
        );
        assert_eq!(
            result,
            "You are working on proj-1: Setup. Read README.md first.\n\nInstall deps"
        );
    }

    #[test]
    fn build_prompt_with_linear_url() {
        let result = build_prompt(
            "vil-123",
            "Fix auth flow",
            TEST_DEFAULT,
            None,
            Some("https://linear.app/team/issue/VIL-123"),
        );
        assert_eq!(
            result,
            "You are working on vil-123: Fix auth flow. The source code is in main/.\n\nThis issue has a Linear ticket: https://linear.app/team/issue/VIL-123"
        );
    }

    #[test]
    fn build_prompt_with_linear_url_and_user_prompt() {
        let result = build_prompt(
            "vil-123",
            "Fix auth flow",
            TEST_DEFAULT,
            Some("Focus on OAuth"),
            Some("https://linear.app/team/issue/VIL-123"),
        );
        assert!(result.contains("The source code is in main/."));
        assert!(result
            .contains("\n\nThis issue has a Linear ticket: https://linear.app/team/issue/VIL-123"));
        assert!(result.ends_with("\n\nFocus on OAuth"));
    }

    #[test]
    fn build_prompt_without_linear_url_no_linear_line() {
        let result = build_prompt("bork-7", "Add feature", TEST_DEFAULT, None, None);
        assert!(!result.contains("Linear"));
    }

    fn test_issue(agent_kind: AgentKind, agent_mode: AgentMode) -> Issue {
        Issue {
            id: "bork-1".to_string(),
            title: "Fix bug".to_string(),
            kind: crate::types::IssueKind::Agentic,
            column: crate::types::Column::InProgress,
            agent_kind,
            agent_mode,
            prompt: None,
            worktree: None,
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
        }
    }

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".to_string(),
            project_root: std::path::PathBuf::from("/tmp/test"),
            agent_kind: AgentKind::OpenCode,
            default_prompt: Some("The source code is in main/.".to_string()),
            done_session_ttl: 300,
            debug: false,
        }
    }

    // --- build_agent_cmd ---

    #[test]
    fn opencode_fresh_plan() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt"));
        assert!(cmd.contains("--agent plan"));
        assert!(cmd.contains("BORK_SESSION='bork-bork-1'"));
        assert!(cmd.contains("BORK_STATUS_DIR='/tmp/status'"));
        assert!(sid.is_none());
    }

    #[test]
    fn opencode_fresh_build() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt"));
        assert!(!cmd.contains("--agent plan"));
    }

    #[test]
    fn opencode_fresh_yolo_treated_as_build() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt"));
        assert!(!cmd.contains("--agent plan"));
        assert!(!cmd.contains("yolo"));
    }

    #[test]
    fn opencode_resume() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Plan);
        issue.session_id = Some("ses_abc123".to_string());
        let config = test_config();
        let (cmd, sid) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
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
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123'"));
        assert!(!cmd.contains("--agent plan"));
    }

    #[test]
    fn claude_fresh_plan() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
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
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("claude --name"));
        assert!(!cmd.contains("--permission-mode plan"));
        assert!(!cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn claude_fresh_yolo() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(!cmd.contains("--permission-mode plan"));
    }

    #[test]
    fn claude_resume() {
        let mut issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        issue.session_id = Some("uuid-123-456".to_string());
        let config = test_config();
        let (cmd, sid) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
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
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--resume 'uuid-789'"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn codex_fresh_plan() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --sandbox read-only --ask-for-approval untrusted"));
        assert!(cmd.contains("You are working on bork-1: Fix bug"));
        assert!(sid.is_none());
    }

    #[test]
    fn codex_fresh_build_uses_full_auto() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Build);
        let config = test_config();
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --full-auto"));
        assert!(!cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn codex_fresh_yolo() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(!cmd.contains("--full-auto"));
    }

    #[test]
    fn codex_resume_uses_session_id() {
        let mut issue = test_issue(AgentKind::Codex, AgentMode::Build);
        issue.session_id = Some("019d76ad-9734-77c0-8169-a727a5524013".to_string());
        let config = test_config();
        let (cmd, sid) = build_agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex resume '019d76ad-9734-77c0-8169-a727a5524013' --full-auto"));
        assert!(!cmd.contains("--prompt"));
        assert_eq!(
            sid,
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn cmd_env_prefix() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _) = build_agent_cmd(&issue, &config, "my-session", "/path/to/status");
        assert!(
            cmd.starts_with("export BORK_SESSION='my-session' BORK_STATUS_DIR='/path/to/status'")
        );
    }

    #[test]
    fn cmd_escapes_single_quotes_in_session_name() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _) = build_agent_cmd(&issue, &config, "it's-a-test", "/tmp/status");
        assert!(cmd.contains("BORK_SESSION='it'\\''s-a-test'"));
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
