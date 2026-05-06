use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::global_config::global_config_dir;
use crate::toml_lite::{self, Table};
use crate::types::{AgentKind, Issue};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub project_name: String,
    pub project_root: PathBuf,
    pub agent_kind: AgentKind,
    pub default_prompt: Option<String>,
    pub done_session_ttl: u64,
    pub debug: bool,
    /// Allowed agents for this project, if explicitly configured.
    /// `None` means "no restriction; use whatever is installed".
    pub agents_allowlist: Option<Vec<AgentKind>>,
}

pub const DEFAULT_DONE_SESSION_TTL: u64 = 300;

pub const DEFAULT_PROMPT_FALLBACK: &str = "The source code is in main/. Use `bork worktree <issue-id> <slug>` to create worktrees for new issues.";

impl Default for AppConfig {
    fn default() -> Self {
        let project_root = find_project_root();
        Self {
            project_name: default_project_name(&project_root),
            project_root,
            agent_kind: AgentKind::OpenCode,
            default_prompt: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
            agents_allowlist: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    pub issues: Vec<Issue>,
}

/// Walk up from cwd looking for a `.bork/` directory.
/// This identifies the project container root.
/// Falls back to cwd if not found.
pub fn find_project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.as_path();

    loop {
        if dir.join(".bork").is_dir() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }

    cwd
}

fn config_dir(project_root: &Path) -> PathBuf {
    project_root.join(".bork")
}

pub fn agent_status_dir(project_root: &Path) -> PathBuf {
    config_dir(project_root).join("agent-status")
}

pub fn ensure_agent_status_dir(project_root: &Path) {
    let dir = agent_status_dir(project_root);
    let _ = fs::create_dir_all(&dir);
}

fn state_path(project_root: &Path) -> PathBuf {
    config_dir(project_root).join("state.json")
}

fn config_path(project_root: &Path) -> PathBuf {
    config_dir(project_root).join("config.toml")
}

pub fn global_config_path() -> PathBuf {
    global_config_dir().join("config.toml")
}

/// Path of the legacy `agents.toml` file. Kept only so we can warn the user
/// once on startup; the file is no longer parsed.
pub fn legacy_agents_config_path() -> PathBuf {
    global_config_dir().join("agents.toml")
}

/// A partial config, where every field is optional. Used as the layer type
/// for the global file and the project file before merging.
#[derive(Debug, Clone, Default)]
pub struct PartialConfig {
    pub project_name: Option<String>,
    pub agent_kind: Option<AgentKind>,
    pub default_prompt: Option<String>,
    pub done_session_ttl: Option<u64>,
    pub debug: Option<bool>,
    pub agents_allowlist: Option<Vec<AgentKind>>,
}

impl PartialConfig {
    /// Merge `other` on top of `self`. Any field set in `other` wins.
    fn merge(self, other: PartialConfig) -> PartialConfig {
        PartialConfig {
            project_name: other.project_name.or(self.project_name),
            agent_kind: other.agent_kind.or(self.agent_kind),
            default_prompt: other.default_prompt.or(self.default_prompt),
            done_session_ttl: other.done_session_ttl.or(self.done_session_ttl),
            debug: other.debug.or(self.debug),
            agents_allowlist: other.agents_allowlist.or(self.agents_allowlist),
        }
    }
}

pub fn load_config() -> AppConfig {
    let project_root = find_project_root();
    load_config_from(&project_root)
}

/// Load and merge global + project config layers, then materialize an
/// `AppConfig`. Missing files are treated as empty layers.
pub fn load_config_from(project_root: &Path) -> AppConfig {
    let merged =
        read_partial(&global_config_path()).merge(read_partial(&config_path(project_root)));
    materialize(merged, project_root)
}

/// Resolve a merged `PartialConfig` into a concrete `AppConfig`, applying
/// built-in defaults for any field still unset.
fn materialize(merged: PartialConfig, project_root: &Path) -> AppConfig {
    let project_name = merged
        .project_name
        .unwrap_or_else(|| default_project_name(project_root));

    AppConfig {
        project_name,
        project_root: project_root.to_path_buf(),
        agent_kind: merged.agent_kind.unwrap_or(AgentKind::OpenCode),
        default_prompt: merged.default_prompt,
        done_session_ttl: merged.done_session_ttl.unwrap_or(DEFAULT_DONE_SESSION_TTL),
        debug: merged.debug.unwrap_or(false),
        agents_allowlist: merged.agents_allowlist,
    }
}

fn default_project_name(project_root: &Path) -> String {
    project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string()
}

/// Load only the global config layer. Used by `agent_config` to seed agent
/// resolution before any project is selected.
pub fn load_global_partial() -> PartialConfig {
    read_partial(&global_config_path())
}

fn read_partial(path: &Path) -> PartialConfig {
    if !path.exists() {
        return PartialConfig::default();
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return PartialConfig::default();
    };
    parse_partial(&contents)
}

pub(crate) fn parse_partial(contents: &str) -> PartialConfig {
    let table = toml_lite::parse(contents);
    partial_from_table(&table)
}

fn partial_from_table(table: &Table) -> PartialConfig {
    let project_name = table
        .get("project_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Accept both `agent_kind` (project-flavoured) and `default_agent`
    // (global-flavoured). They mean the same thing.
    let agent_kind = table
        .get("agent_kind")
        .or_else(|| table.get("default_agent"))
        .and_then(|v| v.as_str())
        .and_then(AgentKind::parse);

    let default_prompt = table
        .get("default_prompt")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let done_session_ttl = table.get("done_session_ttl").and_then(|v| v.as_u64());
    let debug = table.get("debug").and_then(|v| v.as_bool());

    let agents_allowlist = table.get("agents").and_then(|v| v.as_list()).map(|items| {
        items
            .iter()
            .filter_map(|s| AgentKind::parse(s.as_str()))
            .collect::<Vec<_>>()
    });

    PartialConfig {
        project_name,
        agent_kind,
        default_prompt,
        done_session_ttl,
        debug,
        agents_allowlist,
    }
}

pub fn load_state(project_root: &Path) -> AppState {
    let path = state_path(project_root);
    let Ok(contents) = fs::read_to_string(&path) else {
        return AppState::default();
    };
    let Ok(mut state) = serde_json::from_str::<AppState>(&contents) else {
        return AppState::default();
    };
    for issue in &mut state.issues {
        issue.migrate_legacy_fields();
    }
    state
}

pub fn state_mtime(project_root: &Path) -> Option<SystemTime> {
    fs::metadata(state_path(project_root)).ok()?.modified().ok()
}

pub fn save_state(state: &AppState, project_root: &Path) -> anyhow::Result<()> {
    let dir = config_dir(project_root);
    fs::create_dir_all(&dir)?;

    let path = state_path(project_root);
    let json = serde_json::to_string_pretty(state)?;

    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, &path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merge_to_app(global: &str, project: &str) -> AppConfig {
        let merged = parse_partial(global).merge(parse_partial(project));
        materialize(merged, Path::new("."))
    }

    #[test]
    fn parse_partial_with_done_session_ttl() {
        let p = parse_partial(
            r#"
project_name = "myproject"
agent_kind = "opencode"
done_session_ttl = "600"
"#,
        );
        assert_eq!(p.done_session_ttl, Some(600));
    }

    #[test]
    fn parse_partial_without_done_session_ttl() {
        let p = parse_partial(
            r#"
project_name = "myproject"
agent_kind = "opencode"
"#,
        );
        assert_eq!(p.done_session_ttl, None);
    }

    #[test]
    fn parse_partial_basic_fields() {
        let p = parse_partial(
            r#"
project_name = "bork"
agent_kind = "claude"
default_prompt = "Do the thing"
"#,
        );
        assert_eq!(p.project_name.as_deref(), Some("bork"));
        assert_eq!(p.agent_kind, Some(AgentKind::Claude));
        assert_eq!(p.default_prompt.as_deref(), Some("Do the thing"));
    }

    #[test]
    fn parse_partial_default_agent_alias() {
        let p = parse_partial(r#"default_agent = "claude""#);
        assert_eq!(p.agent_kind, Some(AgentKind::Claude));
    }

    #[test]
    fn parse_partial_codex_agent_kind() {
        let p = parse_partial(r#"agent_kind = "codex""#);
        assert_eq!(p.agent_kind, Some(AgentKind::Codex));
    }

    #[test]
    fn parse_partial_empty_yields_no_values() {
        let p = parse_partial("");
        assert!(p.project_name.is_none());
        assert!(p.agent_kind.is_none());
        assert!(p.default_prompt.is_none());
        assert!(p.done_session_ttl.is_none());
        assert!(p.debug.is_none());
        assert!(p.agents_allowlist.is_none());
    }

    #[test]
    fn parse_partial_ignores_comments_and_blanks() {
        let p = parse_partial(
            r#"
# This is a comment
project_name = "test"

# Another comment
agent_kind = "opencode"
"#,
        );
        assert_eq!(p.project_name.as_deref(), Some("test"));
    }

    #[test]
    fn parse_partial_invalid_ttl_is_none() {
        let p = parse_partial(r#"done_session_ttl = "notanumber""#);
        assert_eq!(p.done_session_ttl, None);
    }

    #[test]
    fn parse_partial_debug_true() {
        let p = parse_partial("debug = true");
        assert_eq!(p.debug, Some(true));
    }

    #[test]
    fn parse_partial_debug_quoted_true() {
        let p = parse_partial(r#"debug = "true""#);
        assert_eq!(p.debug, Some(true));
    }

    #[test]
    fn parse_partial_agents_allowlist_array() {
        let p = parse_partial(r#"agents = ["claude", "opencode"]"#);
        assert_eq!(
            p.agents_allowlist,
            Some(vec![AgentKind::Claude, AgentKind::OpenCode])
        );
    }

    #[test]
    fn parse_partial_agents_allowlist_skips_unknown() {
        let p = parse_partial(r#"agents = ["claude", "bogus", "opencode"]"#);
        assert_eq!(
            p.agents_allowlist,
            Some(vec![AgentKind::Claude, AgentKind::OpenCode])
        );
    }

    #[test]
    fn merge_project_overrides_global() {
        let cfg = merge_to_app(
            r#"
default_agent = "claude"
done_session_ttl = 600
"#,
            r#"
project_name = "bork"
agent_kind = "opencode"
"#,
        );
        assert_eq!(cfg.project_name, "bork");
        assert_eq!(cfg.agent_kind, AgentKind::OpenCode);
        assert_eq!(cfg.done_session_ttl, 600);
    }

    #[test]
    fn merge_global_provides_defaults() {
        let cfg = merge_to_app(
            r#"
default_agent = "claude"
done_session_ttl = 900
debug = true
"#,
            r#"project_name = "bork""#,
        );
        assert_eq!(cfg.agent_kind, AgentKind::Claude);
        assert_eq!(cfg.done_session_ttl, 900);
        assert!(cfg.debug);
    }

    #[test]
    fn merge_project_agents_overrides_global_agents() {
        let cfg = merge_to_app(
            r#"agents = ["claude", "opencode", "codex"]"#,
            r#"agents = ["opencode"]"#,
        );
        assert_eq!(cfg.agents_allowlist, Some(vec![AgentKind::OpenCode]));
    }

    #[test]
    fn merge_empty_layers_uses_builtins() {
        let cfg = merge_to_app("", "");
        assert_eq!(cfg.agent_kind, AgentKind::OpenCode);
        assert_eq!(cfg.done_session_ttl, DEFAULT_DONE_SESSION_TTL);
        assert!(!cfg.debug);
        assert!(cfg.agents_allowlist.is_none());
    }
}
