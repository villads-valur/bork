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
        Self {
            project_name: "bork".to_string(),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
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

pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("bork")
}

fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    if path.exists() {
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(config) = toml_parse(&contents) {
                return config;
            }
        }
    }
    let config = AppConfig::default();
    let _ = save_config(&config);
    config
}

pub fn load_state() -> AppState {
    let path = state_path();
    if path.exists() {
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(state) = serde_json::from_str(&contents) {
                return state;
            }
        }
    }
    AppState::default()
}

pub fn save_state(state: &AppState) -> anyhow::Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;

    let path = state_path();
    let json = serde_json::to_string_pretty(state)?;

    // Atomic write: write to temp file, then rename
    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, &path)?;

    Ok(())
}

fn save_config(config: &AppConfig) -> anyhow::Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;

    let path = config_path();
    let toml = format!(
        "project_name = {:?}\nproject_root = {:?}\nagent_kind = {:?}\n",
        config.project_name,
        config.project_root.display(),
        format!("{}", config.agent_kind),
    );

    fs::write(&path, toml)?;
    Ok(())
}

// Minimal TOML parsing (just key = "value" lines) to avoid adding a toml crate
fn toml_parse(contents: &str) -> Result<AppConfig, String> {
    let mut project_name = None;
    let mut project_root = None;
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
                "project_root" => project_root = Some(PathBuf::from(value)),
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
        project_root: project_root
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        agent_kind: agent_kind.unwrap_or(AgentKind::OpenCode),
    })
}
