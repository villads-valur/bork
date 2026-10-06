use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::external::hooks;
use crate::types::AgentMode;

use super::{
    poll_for_new_session_id, shell_escape_single_quotes, AgentProvider, DetectContext,
    LaunchContext,
};

pub struct OpenCode;

impl AgentProvider for OpenCode {
    fn binary(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "opencode"
    }

    fn parse_aliases(&self) -> &'static [&'static str] {
        &["opencode", "open_code", "open-code"]
    }

    fn mode_flag(&self, mode: AgentMode) -> &'static str {
        match mode {
            // OpenCode has no yolo mode; treat it as Build.
            AgentMode::Plan => "--agent plan",
            AgentMode::Build | AgentMode::Yolo => "",
        }
    }

    fn has_modes(&self) -> bool {
        true
    }

    fn supports_yolo(&self) -> bool {
        false
    }

    fn build_cmd(&self, ctx: &LaunchContext) -> (String, Option<String>, Option<String>) {
        if let Some(sid) = ctx.current_session {
            // Resume existing session — skip --prompt, history is preserved
            let escaped_sid = shell_escape_single_quotes(sid);
            let cmd = format!(
                "{} && opencode --session '{}'{}",
                ctx.env_prefix, escaped_sid, ctx.trailing,
            );
            (cmd, Some(sid.to_string()), None)
        } else {
            let cmd = format!(
                "{} && opencode --prompt {}{}{}",
                ctx.env_prefix, ctx.prompt_subst, ctx.trailing, ctx.prompt_cleanup,
            );
            (cmd, None, Some(ctx.build_prompt()))
        }
    }

    fn snapshot_session_ids(&self, _project_root: &Path) -> HashSet<String> {
        list_session_ids()
    }

    fn detect_session_id(&self, ctx: &DetectContext) -> Option<String> {
        poll_for_new_session_id(ctx.before, list_session_ids)
    }

    fn install_hooks(&self) -> anyhow::Result<()> {
        hooks::install_opencode_plugin()
    }

    fn uninstall_hooks(&self) -> anyhow::Result<()> {
        hooks::uninstall_opencode_plugin()
    }
}

/// Run `opencode session list` and return every session ID found.
/// Session IDs start with "ses_", one per line.
pub(super) fn list_session_ids() -> HashSet<String> {
    let Ok(output) = Command::new("opencode").args(["session", "list"]).output() else {
        return HashSet::new();
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_all_session_ids(&stdout)
}

/// Parse every session ID from `opencode session list` output.
/// Expected format: each line starts with the session ID (ses_xxx).
fn parse_all_session_ids(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let token = line.split_whitespace().next()?;
            if token.starts_with("ses_") {
                Some(token.to_string())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_session_ids_finds_every_ses_entry() {
        let output = "ses_abc123   My session title   2024-01-15\nses_def456   Another session   2024-01-14\n";
        let ids = parse_all_session_ids(output);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("ses_abc123"));
        assert!(ids.contains("ses_def456"));
    }

    #[test]
    fn parse_all_session_ids_empty_for_empty_output() {
        assert!(parse_all_session_ids("").is_empty());
    }

    #[test]
    fn parse_all_session_ids_ignores_non_ses_lines() {
        let output = "No sessions found\n";
        assert!(parse_all_session_ids(output).is_empty());
    }
}
