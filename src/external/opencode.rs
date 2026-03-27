use crate::config::{self, AppConfig};
use crate::error::AppError;
use crate::external::tmux;
use crate::types::{AgentKind, AgentMode, Issue};

/// Launch an agent session for an issue.
/// Creates a tmux session with two windows:
///   1. The agent (opencode/claude) launched at the project root with issue context
///   2. A bare terminal
/// Exports BORK_SESSION and BORK_STATUS_DIR so hooks/plugins can write status files.
/// Returns the tmux session name.
pub fn launch_session(issue: &Issue, config: &AppConfig) -> Result<String, AppError> {
    let session_name = issue.session_name();
    let cwd = &config.project_root;

    if tmux::session_exists(&session_name) {
        return Ok(session_name);
    }

    tmux::create_session(&session_name, cwd)?;

    let default_prompt = config
        .default_prompt
        .as_deref()
        .unwrap_or(config::DEFAULT_PROMPT_FALLBACK);

    let prompt = build_prompt(
        &issue.id,
        &issue.title,
        default_prompt,
        issue.prompt.as_deref(),
    );
    let escaped_prompt = shell_escape_single_quotes(&prompt);

    let session_display_name = format!("{}: {}", issue.id, issue.title);
    let escaped_name = shell_escape_single_quotes(&session_display_name);

    let status_dir = config::agent_status_dir(&config.project_root);
    let status_dir_str = status_dir.to_str().unwrap_or("");

    let agent_cmd = match issue.agent_kind {
        AgentKind::OpenCode => {
            // opencode does not support --name
            let mode_flag = match issue.agent_mode {
                AgentMode::Plan => " --agent plan",
                AgentMode::Build => "",
            };
            format!(
                "export BORK_SESSION='{}' BORK_STATUS_DIR='{}' && opencode --prompt '{}'{}",
                shell_escape_single_quotes(&session_name),
                shell_escape_single_quotes(status_dir_str),
                escaped_prompt,
                mode_flag,
            )
        }
        AgentKind::Claude => {
            let mode_flag = match issue.agent_mode {
                AgentMode::Plan => " --permission-mode plan",
                AgentMode::Build => "",
            };
            format!(
                "export BORK_SESSION='{}' BORK_STATUS_DIR='{}' && claude --name '{}'{} '{}'",
                shell_escape_single_quotes(&session_name),
                shell_escape_single_quotes(status_dir_str),
                escaped_name,
                mode_flag,
                escaped_prompt,
            )
        }
    };
    tmux::send_keys(&session_name, &agent_cmd)?;

    // Second window: bare terminal for ad-hoc commands
    tmux::create_window(&session_name, "terminal", cwd)?;

    Ok(session_name)
}

/// Build the full prompt sent to the agent.
/// Always starts with issue context and the default prompt, then appends the
/// user's custom prompt (if any) after a blank line.
fn build_prompt(id: &str, title: &str, default_prompt: &str, user_prompt: Option<&str>) -> String {
    let mut prompt = format!("You are working on {}: {}. {}", id, title, default_prompt);

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

    const TEST_DEFAULT: &str = "Check AGENTS.md for project context.";

    #[test]
    fn build_prompt_without_user_prompt() {
        let result = build_prompt("bork-1", "Fix auth", TEST_DEFAULT, None);
        assert_eq!(
            result,
            "You are working on bork-1: Fix auth. Check AGENTS.md for project context."
        );
    }

    #[test]
    fn build_prompt_with_user_prompt() {
        let result = build_prompt(
            "bork-2",
            "Add tests",
            TEST_DEFAULT,
            Some("Focus on unit tests"),
        );
        assert_eq!(
            result,
            "You are working on bork-2: Add tests. Check AGENTS.md for project context.\n\nFocus on unit tests"
        );
    }

    #[test]
    fn build_prompt_with_empty_user_prompt() {
        let result = build_prompt("bork-3", "Refactor", TEST_DEFAULT, Some(""));
        assert_eq!(
            result,
            "You are working on bork-3: Refactor. Check AGENTS.md for project context."
        );
    }

    #[test]
    fn build_prompt_with_whitespace_only_user_prompt() {
        let result = build_prompt("bork-4", "Cleanup", TEST_DEFAULT, Some("   \n  "));
        assert_eq!(
            result,
            "You are working on bork-4: Cleanup. Check AGENTS.md for project context."
        );
    }

    #[test]
    fn build_prompt_user_prompt_is_trimmed() {
        let result = build_prompt("bork-5", "Feature", TEST_DEFAULT, Some("  do the thing  "));
        assert!(result.ends_with("\n\ndo the thing"));
    }

    #[test]
    fn build_prompt_with_real_default_fallback() {
        let result = build_prompt(
            "bork-6",
            "New feature",
            config::DEFAULT_PROMPT_FALLBACK,
            None,
        );
        assert!(result.starts_with("You are working on bork-6: New feature."));
        assert!(result.contains("Check AGENTS.md for project context"));
        assert!(result.contains("bork worktree"));
    }

    #[test]
    fn build_prompt_with_custom_default_prompt() {
        let result = build_prompt(
            "proj-1",
            "Setup",
            "Read README.md first.",
            Some("Install deps"),
        );
        assert_eq!(
            result,
            "You are working on proj-1: Setup. Read README.md first.\n\nInstall deps"
        );
    }

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape_single_quotes("hello world"), "hello world");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape_single_quotes("it's a test"), "it'\\''s a test");
    }
}
