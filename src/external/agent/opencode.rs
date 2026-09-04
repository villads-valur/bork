use std::collections::HashSet;
use std::process::Command;

use crate::external::hooks;
use crate::types::AgentMode;

use super::{shell_escape_single_quotes, AgentProvider, DetectContext, LaunchContext};

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
            (cmd, None, None)
        } else {
            let cmd = format!(
                "{} && opencode --prompt {}{}{}",
                ctx.env_prefix, ctx.prompt_subst, ctx.trailing, ctx.prompt_cleanup,
            );
            (cmd, None, Some(ctx.build_prompt()))
        }
    }

    fn detect_session_id(&self, ctx: &DetectContext) -> Option<String> {
        detect_session_id(ctx.before)
    }

    fn install_hooks(&self) -> anyhow::Result<()> {
        hooks::install_opencode_plugin()
    }

    fn uninstall_hooks(&self) -> anyhow::Result<()> {
        hooks::uninstall_opencode_plugin()
    }
}

/// Poll `opencode session list` until an id appears that wasn't in the
/// pre-launch snapshot. Returns it if found within ~5 seconds, otherwise
/// None — the newest global session could belong to any concurrent
/// opencode run, so only a genuinely new id is trusted.
fn detect_session_id(before: &HashSet<String>) -> Option<String> {
    // Give OpenCode a moment to create its session before polling
    std::thread::sleep(std::time::Duration::from_millis(800));

    for _ in 0..9 {
        if let Some(sid) = list_session_ids()
            .into_iter()
            .find(|id| !before.contains(id))
        {
            return Some(sid);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    None
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
