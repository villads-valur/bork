use crate::types::AgentMode;

use super::{AgentProvider, LaunchContext};

pub struct Cursor;

impl AgentProvider for Cursor {
    fn binary(&self) -> &'static str {
        "cursor-agent"
    }

    fn display_name(&self) -> &'static str {
        "cursor"
    }

    fn parse_aliases(&self) -> &'static [&'static str] {
        &["cursor", "cursor-agent", "cursor_agent"]
    }

    fn mode_flag(&self, mode: AgentMode) -> &'static str {
        // `--trust` is required on every mode: bork creates a fresh worktree per
        // issue, and the first run in an untrusted directory blocks on a
        // Workspace Trust prompt. Requires cursor-agent >= 2026.09.02.
        match mode {
            // Plan is enforced read-only via `--mode plan` (not advisory).
            AgentMode::Plan => "--trust --mode plan",
            // Build is Cursor's default interactive-with-approval mode.
            AgentMode::Build => "--trust",
            // `-f/--force` (never the `--yolo` alias) auto-approves everything;
            // keep it to Yolo so trust doesn't leak Yolo semantics into Build.
            AgentMode::Yolo => "--trust -f",
        }
    }

    fn has_modes(&self) -> bool {
        true
    }

    fn supports_yolo(&self) -> bool {
        true
    }

    fn build_cmd(&self, ctx: &LaunchContext) -> (String, Option<String>, Option<String>) {
        // Fresh launch only in this PR: no resume, no session-id capture
        // (left at the trait defaults). cursor-agent has no --name flag, so
        // unlike Pi/Claude the chat stays unlabelled.
        let cmd = format!(
            "{} && cursor-agent{} {}{}",
            ctx.env_prefix, ctx.trailing, ctx.prompt_subst, ctx.prompt_cleanup,
        );
        (cmd, None, Some(ctx.build_prompt()))
    }
}
