use std::fs;
use std::path::Path;

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

    fn has_session(&self, session_id: &str) -> bool {
        let projects_dir = hooks::claude_config_dir().join("projects");
        transcript_exists(&projects_dir, session_id).unwrap_or(true)
    }

    fn install_hooks(&self) -> anyhow::Result<()> {
        hooks::install_claude_hooks()
    }

    fn uninstall_hooks(&self) -> anyhow::Result<()> {
        hooks::uninstall_claude_hooks()
    }
}

/// Whether any project dir holds the transcript. Claude keeps them in
/// `<config dir>/projects/<encoded cwd>/<id>.jsonl`. Searches every dir rather
/// than re-deriving Claude's cwd encoding, which truncates long paths.
/// `None` when the projects dir can't be read, so callers keep resuming.
fn transcript_exists(projects_dir: &Path, session_id: &str) -> Option<bool> {
    let file_name = format!("{session_id}.jsonl");
    let entries = fs::read_dir(projects_dir).ok()?;
    Some(
        entries
            .flatten()
            .any(|entry| entry.path().join(&file_name).is_file()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "cb2f9a52-8df5-4b17-8088-883ed5edcec6";

    #[test]
    fn transcript_found_in_any_project_dir() {
        let root = tempfile::TempDir::new().unwrap();
        fs::create_dir(root.path().join("-Users-x-code-bork")).unwrap();
        fs::create_dir(root.path().join("-Users-x-other")).unwrap();
        fs::write(
            root.path()
                .join("-Users-x-other")
                .join(format!("{SID}.jsonl")),
            "",
        )
        .unwrap();

        assert_eq!(transcript_exists(root.path(), SID), Some(true));
    }

    #[test]
    fn transcript_missing() {
        let root = tempfile::TempDir::new().unwrap();
        fs::create_dir(root.path().join("-Users-x-code-bork")).unwrap();

        assert_eq!(transcript_exists(root.path(), SID), Some(false));
    }

    #[test]
    fn unreadable_projects_dir_is_unknown() {
        let root = tempfile::TempDir::new().unwrap();

        assert_eq!(transcript_exists(&root.path().join("nope"), SID), None);
    }
}
