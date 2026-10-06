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
        build_cmd_with_minter(ctx, mint_chat_id)
    }
}

fn build_cmd_with_minter(
    ctx: &LaunchContext,
    mint: impl FnOnce(&Path) -> Option<String>,
) -> (String, Option<String>, Option<String>) {
    if let Some(sid) = ctx.current_session {
        // Preserve history without re-sending the prompt; keep --trust on resume.
        let escaped_sid = shell_escape_single_quotes(sid);
        let cmd = format!(
            "{} && cursor-agent --resume '{}'{}",
            ctx.env_prefix, escaped_sid, ctx.trailing,
        );
        return (cmd, Some(sid.to_string()), None);
    }

    let prompt = ctx.build_prompt();
    match mint(ctx.project_root) {
        Some(chat_id) => {
            let escaped_id = shell_escape_single_quotes(&chat_id);
            let cmd = format!(
                "{} && cursor-agent --resume '{}'{} {}{}",
                ctx.env_prefix, escaped_id, ctx.trailing, ctx.prompt_subst, ctx.prompt_cleanup,
            );
            (cmd, Some(chat_id), Some(prompt))
        }
        None => {
            // Without a captured id, the next launch also starts fresh.
            let cmd = format!(
                "{} && cursor-agent{} {}{}",
                ctx.env_prefix, ctx.trailing, ctx.prompt_subst, ctx.prompt_cleanup,
            );
            (cmd, None, Some(prompt))
        }
    }
}

/// Mint in the agent's launch cwd so the chat belongs to the same workspace.
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

// Cross-provider tests must not invoke a locally installed agent.
#[cfg(test)]
fn mint_chat_id(_project_root: &Path) -> Option<String> {
    None
}

/// `create-chat` prints a bare UUID; reject banners or other extra output.
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

    const CHAT_ID: &str = "a506b8cb-b2ea-4b22-b0bb-7c449eb14606";

    fn command(
        current_session: Option<&str>,
        mint: impl FnOnce(&Path) -> Option<String>,
    ) -> (String, Option<String>, Option<String>) {
        let issue = crate::types::Issue::new(
            "bork-1",
            "Test",
            crate::types::Column::InProgress,
            crate::types::AgentKind::Cursor,
        );
        let ctx = LaunchContext {
            issue: &issue,
            project_root: Path::new("/project-root"),
            env_prefix: "export BORK_SESSION='test'",
            trailing: " --trust --mode plan",
            prompt_subst: "\"$(cat '/prompt')\"",
            prompt_cleanup: "; rm -f '/prompt'",
            current_session,
            build_prompt: &|| {
                assert!(current_session.is_none(), "resume must not build a prompt");
                "First message".to_string()
            },
        };
        build_cmd_with_minter(&ctx, mint)
    }

    #[test]
    fn cursor_fresh_with_chat_id() {
        let (cmd, sid, prompt) = command(None, |cwd| {
            assert_eq!(cwd, Path::new("/project-root"));
            Some(CHAT_ID.to_string())
        });
        assert!(cmd.contains(&format!("--resume '{CHAT_ID}' --trust --mode plan")));
        assert!(cmd.contains("\"$(cat '/prompt')\"; rm -f '/prompt'"));
        assert_eq!(sid.as_deref(), Some(CHAT_ID));
        assert_eq!(prompt.as_deref(), Some("First message"));
    }

    #[test]
    fn cursor_fresh_without_chat_id() {
        let (cmd, sid, prompt) = command(None, |_| None);
        assert!(cmd.contains("cursor-agent --trust --mode plan \"$(cat '/prompt')\""));
        assert!(!cmd.contains("--resume"));
        assert_eq!(sid, None);
        assert_eq!(prompt.as_deref(), Some("First message"));
    }

    #[test]
    fn cursor_resume_omits_prompt() {
        let (cmd, sid, prompt) = command(Some(CHAT_ID), |_| panic!("resume must not mint"));
        assert!(cmd.ends_with(&format!("--resume '{CHAT_ID}' --trust --mode plan")));
        assert!(!cmd.contains("$(cat"));
        assert_eq!(sid.as_deref(), Some(CHAT_ID));
        assert_eq!(prompt, None);
    }

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
