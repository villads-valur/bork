use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::external::hooks;
use crate::types::AgentMode;

use super::{
    is_uuid_like, poll_for_new_session_id, shell_escape_single_quotes, AgentProvider,
    DetectContext, LaunchContext,
};

pub struct Codex;

impl AgentProvider for Codex {
    fn binary(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "codex"
    }

    fn parse_aliases(&self) -> &'static [&'static str] {
        &["codex"]
    }

    fn mode_flag(&self, mode: AgentMode) -> &'static str {
        match mode {
            // `--full-auto` is deprecated upstream; use explicit sandbox + approval flags.
            AgentMode::Plan => "--sandbox workspace-write --ask-for-approval on-request",
            AgentMode::Build => "--sandbox workspace-write --ask-for-approval never",
            AgentMode::Yolo => "--dangerously-bypass-approvals-and-sandbox",
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
            let escaped_sid = shell_escape_single_quotes(sid);
            let cmd = format!(
                "{} && codex resume '{}'{}",
                ctx.env_prefix, escaped_sid, ctx.trailing
            );
            (cmd, Some(sid.to_string()), None)
        } else {
            let cmd = format!(
                "{} && codex{} {}{}",
                ctx.env_prefix, ctx.trailing, ctx.prompt_subst, ctx.prompt_cleanup,
            );
            (cmd, None, Some(ctx.build_prompt()))
        }
    }

    fn detect_session_id(&self, _ctx: &DetectContext) -> Option<String> {
        detect_session_id()
    }

    fn install_hooks(&self) -> anyhow::Result<()> {
        hooks::install_codex_hooks()
    }

    fn uninstall_hooks(&self) -> anyhow::Result<()> {
        hooks::uninstall_codex_hooks()
    }
}

pub(super) fn sessions_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Detect a newly created Codex session UUID by scanning ~/.codex/sessions.
fn detect_session_id() -> Option<String> {
    let sessions_root = sessions_root()?;
    let before = collect_session_ids(&sessions_root);
    poll_for_new_session_id(&before, || collect_session_ids(&sessions_root))
}

/// Collect all Codex session IDs.
pub(super) fn collect_session_ids(sessions_root: &Path) -> HashSet<String> {
    let mut sessions = HashSet::new();
    let mut stack = vec![sessions_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(session_id) = parse_session_id_from_filename(file_name) else {
                continue;
            };
            sessions.insert(session_id);
        }
    }

    sessions
}

fn parse_session_id_from_filename(file_name: &str) -> Option<String> {
    let stem = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    if stem.len() < 36 {
        return None;
    }
    let candidate = &stem[stem.len() - 36..];
    if is_uuid_like(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codex_session_id_from_filename_extracts_uuid() {
        let file_name = "rollout-2026-04-10T11-16-21-019d76ad-9734-77c0-8169-a727a5524013.jsonl";
        assert_eq!(
            parse_session_id_from_filename(file_name),
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn parse_codex_session_id_from_filename_rejects_invalid() {
        let file_name = "rollout-2026-04-10T11-16-21-not-a-uuid.jsonl";
        assert_eq!(parse_session_id_from_filename(file_name), None);
    }
}
