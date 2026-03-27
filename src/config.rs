use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::{AgentKind, Issue};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub project_name: String,
    pub project_root: PathBuf,
    pub agent_kind: AgentKind,
}

impl Default for AppConfig {
    fn default() -> Self {
        let project_root = find_project_root();
        Self {
            project_name: "bork".to_string(),
            project_root,
            agent_kind: AgentKind::OpenCode,
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

fn toml_parse(contents: &str) -> Result<AppConfig, String> {
    let mut project_name = None;
    let mut agent_kind = None;

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
                _ => {}
            }
        }
    }

    Ok(AppConfig {
        project_name: project_name.unwrap_or_else(|| "bork".to_string()),
        project_root: PathBuf::from("."),
        agent_kind: agent_kind.unwrap_or(AgentKind::OpenCode),
    })
}
