use crate::config::AppConfig;
use crate::error::AppError;
use crate::external::tmux;
use crate::types::{AgentKind, Issue};

/// Launch an agent session for an issue.
/// Creates a tmux session with two windows:
///   1. The agent (opencode/claude)
///   2. A bare terminal
/// Returns the tmux session name.
pub fn launch_session(issue: &Issue, config: &AppConfig) -> Result<String, AppError> {
    let session_name = issue.session_name();
    let cwd = &config.project_root;

    if tmux::session_exists(&session_name) {
        return Ok(session_name);
    }

    tmux::create_session(&session_name, cwd)?;

    let agent_cmd = match issue.agent_kind {
        AgentKind::OpenCode => "opencode",
        AgentKind::Claude => "claude",
    };
    tmux::send_keys(&session_name, agent_cmd)?;

    // Second window: bare terminal for ad-hoc commands
    tmux::create_window(&session_name, "terminal", cwd)?;

    Ok(session_name)
}
