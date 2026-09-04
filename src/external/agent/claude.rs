use crate::external::hooks;
use crate::types::AgentMode;

use super::{generate_uuid, shell_escape_single_quotes, AgentProvider, LaunchContext};

pub struct Claude;

impl AgentProvider for Claude {
    fn binary(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "claude"
    }

    fn parse_aliases(&self) -> &'static [&'static str] {
        &["claude"]
    }

    fn mode_flag(&self, mode: AgentMode) -> &'static str {
        match mode {
            AgentMode::Plan => "--permission-mode plan",
            AgentMode::Yolo => "--dangerously-skip-permissions",
            AgentMode::Build => "",
        }
    }

    fn has_modes(&self) -> bool {
        true
    }

    fn supports_yolo(&self) -> bool {
        true
    }

    fn build_cmd(&self, ctx: &LaunchContext) -> (String, Option<String>, Option<String>) {
        let session_display_name = format!("{}: {}", ctx.issue.id, ctx.issue.title);
        let escaped_name = shell_escape_single_quotes(&session_display_name);

        if let Some(sid) = ctx.current_session {
            // Resume existing session — skip the prompt, history is preserved
            let escaped_sid = shell_escape_single_quotes(sid);
            let cmd = format!(
                "{} && claude --name '{}'{} --resume '{}'",
                ctx.env_prefix, escaped_name, ctx.trailing, escaped_sid,
            );
            (cmd, Some(sid.to_string()), None)
        } else {
            // Fresh session: stage prompt and optionally pre-assign a UUID
            let prompt = ctx.build_prompt();
            let uuid = generate_uuid().unwrap_or_default();
            if uuid.is_empty() {
                let cmd = format!(
                    "{} && claude --name '{}'{} {}{}",
                    ctx.env_prefix,
                    escaped_name,
                    ctx.trailing,
                    ctx.prompt_subst,
                    ctx.prompt_cleanup,
                );
                (cmd, None, Some(prompt))
            } else {
                let escaped_uuid = shell_escape_single_quotes(&uuid);
                let cmd = format!(
                    "{} && claude --name '{}'{} --session-id '{}' {}{}",
                    ctx.env_prefix,
                    escaped_name,
                    ctx.trailing,
                    escaped_uuid,
                    ctx.prompt_subst,
                    ctx.prompt_cleanup,
                );
                (cmd, Some(uuid), Some(prompt))
            }
        }
    }

    fn install_hooks(&self) -> anyhow::Result<()> {
        hooks::install_claude_hooks()
    }

    fn uninstall_hooks(&self) -> anyhow::Result<()> {
        hooks::uninstall_claude_hooks()
    }
}
