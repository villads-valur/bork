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

    let prompt = issue
        .prompt
        .clone()
        .unwrap_or_else(|| format!("Working on {}: {}", issue.id, issue.title));
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

/// Escape a string for use inside single quotes in a shell command.
fn shell_escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}
