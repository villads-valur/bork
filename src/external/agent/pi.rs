use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::external::hooks;
use crate::types::AgentMode;

use super::{
    is_uuid_like, poll_for_new_session_id, shell_escape_single_quotes, AgentProvider,
    DetectContext, LaunchContext,
};

pub struct Pi;

impl AgentProvider for Pi {
    fn binary(&self) -> &'static str {
        "pi"
    }

    fn display_name(&self) -> &'static str {
        "pi"
    }

    fn parse_aliases(&self) -> &'static [&'static str] {
        &["pi"]
    }

    // Pi has a single mode and no built-in plan/yolo flags. Users can still
    // add per-mode args via `[agent.pi.mode.<mode>]` if desired.
    fn mode_flag(&self, _mode: AgentMode) -> &'static str {
        ""
    }

    fn has_modes(&self) -> bool {
        false
    }

    fn supports_yolo(&self) -> bool {
        false
    }

    fn build_cmd(&self, ctx: &LaunchContext) -> (String, Option<String>, Option<String>) {
        let session_display_name = format!("{}: {}", ctx.issue.id, ctx.issue.title);
        let escaped_name = shell_escape_single_quotes(&session_display_name);

        if let Some(sid) = ctx.current_session {
            // Resume existing session — skip the prompt, history is preserved.
            let escaped_sid = shell_escape_single_quotes(sid);
            let cmd = format!(
                "{} && pi --session '{}'{}",
                ctx.env_prefix, escaped_sid, ctx.trailing,
            );
            (cmd, Some(sid.to_string()), None)
        } else {
            let cmd = format!(
                "{} && pi --name '{}'{} {}{}",
                ctx.env_prefix, escaped_name, ctx.trailing, ctx.prompt_subst, ctx.prompt_cleanup,
            );
            (cmd, None, Some(ctx.build_prompt()))
        }
    }

    fn detect_session_id(&self, ctx: &DetectContext) -> Option<String> {
        detect_session_id(ctx.project_root)
    }

    fn install_hooks(&self) -> anyhow::Result<()> {
        hooks::install_pi_extension()
    }

    fn uninstall_hooks(&self) -> anyhow::Result<()> {
        hooks::uninstall_pi_extension()
    }
}

/// Detect a newly created Pi session UUID by scanning Pi's per-cwd session
/// directory. Pi stores sessions under `<sessions_root>/--<cwd>--/` as
/// `<timestamp>_<uuid>.jsonl`, where `<cwd>` has `/` replaced by `-`.
fn detect_session_id(project_root: &Path) -> Option<String> {
    let sessions_dir = sessions_dir(project_root)?;
    let before = collect_session_ids(&sessions_dir);
    poll_for_new_session_id(&before, || collect_session_ids(&sessions_dir))
}

/// Resolve Pi's session directory for a given working directory.
/// Honors `PI_CODING_AGENT_SESSION_DIR` (flat dir) and `PI_CODING_AGENT_DIR`
/// overrides, falling back to `~/.pi/agent/sessions/--<cwd>--/`.
pub(super) fn sessions_dir(project_root: &Path) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_SESSION_DIR") {
        return Some(PathBuf::from(dir));
    }

    let root = if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
        PathBuf::from(dir)
    } else {
        let home = std::env::var("HOME").ok()?;
        PathBuf::from(home).join(".pi").join("agent")
    };

    let cwd = project_root.to_str()?;
    Some(
        root.join("sessions")
            .join(format!("--{}--", cwd.replace('/', "-"))),
    )
}

/// Collect Pi session UUIDs from a session dir.
pub(super) fn collect_session_ids(sessions_dir: &Path) -> HashSet<String> {
    let mut sessions = HashSet::new();
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
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
    sessions
}

/// Extract the session UUID from a Pi session filename (`<timestamp>_<uuid>.jsonl`).
fn parse_session_id_from_filename(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".jsonl")?;
    let candidate = stem.rsplit('_').next()?;
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
    fn pi_session_id_parsed_from_filename() {
        assert_eq!(
            parse_session_id_from_filename(
                "2024-12-03T14-00-00_019d76ad-9734-77c0-8169-a727a5524013.jsonl"
            ),
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
        assert_eq!(parse_session_id_from_filename("not-a-session.txt"), None);
        assert_eq!(parse_session_id_from_filename("123_short.jsonl"), None);
    }
}
