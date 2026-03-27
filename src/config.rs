use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::{AgentKind, Issue};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub project_name: String,
    pub project_root: PathBuf,
    pub agent_kind: AgentKind,
    pub default_prompt: Option<String>,
    pub done_session_ttl: u64,
}

pub const DEFAULT_DONE_SESSION_TTL: u64 = 300;

pub const DEFAULT_PROMPT_FALLBACK: &str = "Check AGENTS.md for project context. The source code is in main/. Use the worktree skill to create worktrees for new issues.";

impl Default for AppConfig {
    fn default() -> Self {
        let project_root = find_project_root();
        Self {
            project_name: "bork".to_string(),
            project_root,
            agent_kind: AgentKind::OpenCode,
            default_prompt: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub issues: Vec<Issue>,
}

impl Default for AppState {
    fn default() -> Self {
        Self { issues: Vec::new() }
    }
}

/// Walk up from cwd looking for a `.bork/` directory.
/// This identifies the project container root.
/// Falls back to cwd if not found.
fn find_project_root() -> PathBuf {
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

fn config_dir(project_root: &PathBuf) -> PathBuf {
    project_root.join(".bork")
}

pub fn agent_status_dir(project_root: &PathBuf) -> PathBuf {
    config_dir(project_root).join("agent-status")
}

pub fn ensure_agent_status_dir(project_root: &PathBuf) {
    let dir = agent_status_dir(project_root);
    let _ = fs::create_dir_all(&dir);
}

fn state_path(project_root: &PathBuf) -> PathBuf {
    config_dir(project_root).join("state.json")
}

fn config_path(project_root: &PathBuf) -> PathBuf {
    config_dir(project_root).join("config.toml")
}

pub fn load_config() -> AppConfig {
    let project_root = find_project_root();
    let path = config_path(&project_root);

    if path.exists() {
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(mut config) = toml_parse(&contents) {
                config.project_root = project_root;
                return config;
            }
        }
    }

    AppConfig {
        project_root,
        ..AppConfig::default()
    }
}

pub fn load_state(project_root: &PathBuf) -> AppState {
    let path = state_path(project_root);
    if path.exists() {
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(state) = serde_json::from_str(&contents) {
                return state;
            }
        }
    }
    AppState::default()
}

pub fn save_state(state: &AppState, project_root: &PathBuf) -> anyhow::Result<()> {
    let dir = config_dir(project_root);
    fs::create_dir_all(&dir)?;

    let path = state_path(project_root);
    let json = serde_json::to_string_pretty(state)?;

    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, &path)?;

    Ok(())
}

pub(crate) fn toml_parse(contents: &str) -> Result<AppConfig, String> {
    let mut project_name = None;
    let mut agent_kind = None;
    let mut default_prompt = None;
    let mut done_session_ttl = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            match key {
                "project_name" => project_name = Some(value.to_string()),
                "agent_kind" => {
                    agent_kind = Some(match value {
                        "claude" => AgentKind::Claude,
                        _ => AgentKind::OpenCode,
                    });
                }
                "default_prompt" => default_prompt = Some(value.to_string()),
                "done_session_ttl" => {
                    done_session_ttl = value.parse::<u64>().ok();
                }
                _ => {}
            }
        }
    }

    Ok(AppConfig {
        project_name: project_name.unwrap_or_else(|| "bork".to_string()),
        project_root: PathBuf::from("."),
        agent_kind: agent_kind.unwrap_or(AgentKind::OpenCode),
        default_prompt,
        done_session_ttl: done_session_ttl.unwrap_or(DEFAULT_DONE_SESSION_TTL),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_parse_with_done_session_ttl() {
        let contents = r#"
project_name = "myproject"
agent_kind = "opencode"
done_session_ttl = "600"
"#;
        let config = toml_parse(contents).unwrap();
        assert_eq!(config.done_session_ttl, 600);
    }

    #[test]
    fn toml_parse_without_done_session_ttl_uses_default() {
        let contents = r#"
project_name = "myproject"
agent_kind = "opencode"
"#;
        let config = toml_parse(contents).unwrap();
        assert_eq!(config.done_session_ttl, DEFAULT_DONE_SESSION_TTL);
        assert_eq!(config.done_session_ttl, 300);
    }

    #[test]
    fn toml_parse_basic_fields() {
        let contents = r#"
project_name = "bork"
agent_kind = "claude"
default_prompt = "Do the thing"
"#;
        let config = toml_parse(contents).unwrap();
        assert_eq!(config.project_name, "bork");
        assert_eq!(config.agent_kind, AgentKind::Claude);
        assert_eq!(config.default_prompt, Some("Do the thing".to_string()));
    }

    #[test]
    fn toml_parse_empty_config_uses_defaults() {
        let config = toml_parse("").unwrap();
        assert_eq!(config.project_name, "bork");
        assert_eq!(config.agent_kind, AgentKind::OpenCode);
        assert_eq!(config.default_prompt, None);
        assert_eq!(config.done_session_ttl, DEFAULT_DONE_SESSION_TTL);
    }

    #[test]
    fn toml_parse_ignores_comments_and_blanks() {
        let contents = r#"
# This is a comment
project_name = "test"

# Another comment
agent_kind = "opencode"
"#;
        let config = toml_parse(contents).unwrap();
        assert_eq!(config.project_name, "test");
    }

    #[test]
    fn toml_parse_invalid_ttl_uses_default() {
        let contents = r#"
done_session_ttl = "notanumber"
"#;
        let config = toml_parse(contents).unwrap();
        assert_eq!(config.done_session_ttl, DEFAULT_DONE_SESSION_TTL);
    }
}
