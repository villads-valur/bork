use std::path::Path;
#[cfg(not(test))]
use std::process::Command;

use crate::types::AgentMode;

use super::{is_uuid_like, shell_escape_single_quotes, AgentProvider, LaunchContext};

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
        if let Some(sid) = ctx.current_session {
            // Resume an existing chat — skip the prompt, history is preserved.
            // `{trailing}` still carries --trust/--mode; a resume that drops
            // --trust hangs on the Workspace Trust gate.
            let escaped_sid = shell_escape_single_quotes(sid);
            let cmd = format!(
                "{} && cursor-agent --resume '{}'{}",
                ctx.env_prefix, escaped_sid, ctx.trailing,
            );
            return (cmd, Some(sid.to_string()), None);
        }

        // Fresh launch. Mint a chat id up front (mirroring Claude's pre-assigned
        // --session-id) so bork can resume the same chat later. cursor-agent has
        // no --name flag, so the chat stays unlabelled.
        let prompt = ctx.build_prompt();
        match mint_chat_id(ctx.project_root) {
            Some(chat_id) => {
                let escaped_id = shell_escape_single_quotes(&chat_id);
                let cmd = format!(
                    "{} && cursor-agent --resume '{}'{} {}{}",
                    ctx.env_prefix, escaped_id, ctx.trailing, ctx.prompt_subst, ctx.prompt_cleanup,
                );
                (cmd, Some(chat_id), Some(prompt))
            }
            None => {
                // Minting failed: launch a bare fresh chat. No id is captured, so
                // the next launch starts fresh (same degraded behaviour as every
                // other harness on a detection miss).
                let cmd = format!(
                    "{} && cursor-agent{} {}{}",
                    ctx.env_prefix, ctx.trailing, ctx.prompt_subst, ctx.prompt_cleanup,
                );
                (cmd, None, Some(prompt))
            }
        }
    }
}

/// Shell out to `cursor-agent create-chat` in `project_root` (the same cwd the
/// agent is later launched in, so the chat keys to the right workspace) and
/// return the minted chat id. `None` on any failure — the caller falls back to a
/// bare fresh launch.
#[cfg(not(test))]
fn mint_chat_id(project_root: &Path) -> Option<String> {
    let output = Command::new("cursor-agent")
        .arg("create-chat")
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_cursor_chat_id(&stdout)
}

/// Test stub: never talks to the `cursor-agent` binary. Returns a fixed id so
/// the fresh-with-id path is deterministic, or `None` when the project root is
/// the mint-failure sentinel, exercising the fresh-without-id fallback.
#[cfg(test)]
fn mint_chat_id(project_root: &Path) -> Option<String> {
    if project_root == Path::new(MINT_FAILURE_ROOT) {
        None
    } else {
        Some(TEST_CHAT_ID.to_string())
    }
}

#[cfg(test)]
const TEST_CHAT_ID: &str = "a506b8cb-b2ea-4b22-b0bb-7c449eb14606";

#[cfg(test)]
const MINT_FAILURE_ROOT: &str = "/tmp/cursor-mint-fails";

/// Parse the chat id from `cursor-agent create-chat` output. The command prints
/// a bare 36-byte UUID (no banner, no trailing newline); trim and validate.
/// Kept pure so it is unit-testable without a subprocess.
fn parse_cursor_chat_id(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if is_uuid_like(trimmed) {
        Some(trimmed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cursor_chat_id_accepts_bare_uuid() {
        assert_eq!(
            parse_cursor_chat_id("a506b8cb-b2ea-4b22-b0bb-7c449eb14606"),
            Some("a506b8cb-b2ea-4b22-b0bb-7c449eb14606".to_string())
        );
    }

    #[test]
    fn parse_cursor_chat_id_trims_surrounding_whitespace() {
        assert_eq!(
            parse_cursor_chat_id("  a506b8cb-b2ea-4b22-b0bb-7c449eb14606\n"),
            Some("a506b8cb-b2ea-4b22-b0bb-7c449eb14606".to_string())
        );
    }

    #[test]
    fn parse_cursor_chat_id_rejects_empty() {
        assert_eq!(parse_cursor_chat_id(""), None);
        assert_eq!(parse_cursor_chat_id("   \n  "), None);
    }

    #[test]
    fn parse_cursor_chat_id_rejects_banner_then_uuid() {
        // A leading banner line means the "UUID" isn't the whole trimmed output,
        // so validation fails — we must not scrape an id out of chatter.
        let out = "Cursor Agent v2026.09.02\na506b8cb-b2ea-4b22-b0bb-7c449eb14606";
        assert_eq!(parse_cursor_chat_id(out), None);
    }

    #[test]
    fn parse_cursor_chat_id_rejects_non_uuid() {
        assert_eq!(parse_cursor_chat_id("not-a-uuid"), None);
    }
}
