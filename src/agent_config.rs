use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::global_config::global_config_dir;
use crate::types::AgentKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSelection {
    pub available: Vec<AgentKind>,
    pub default_agent: Option<AgentKind>,
}

#[derive(Debug, Clone, Default)]
struct AgentPreferences {
    enabled: Option<Vec<AgentKind>>,
    default_agent: Option<AgentKind>,
}

pub fn agent_config_path() -> PathBuf {
    global_config_dir().join("agents.toml")
}

pub fn resolve_agent_selection() -> AgentSelection {
    let prefs = load_agent_preferences();
    let installed: Vec<AgentKind> = AgentKind::ALL
        .into_iter()
        .filter(|kind| command_exists(kind.command()))
        .collect();
    resolve_with_installed(prefs, &installed)
}

fn load_agent_preferences() -> AgentPreferences {
    let path = agent_config_path();
    let Ok(contents) = fs::read_to_string(path) else {
        return AgentPreferences::default();
    };
    parse_agent_preferences(&contents)
}

fn parse_agent_preferences(contents: &str) -> AgentPreferences {
    let mut enabled = None;
    let mut default_agent = None;

    for raw in contents.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "agents" => enabled = Some(parse_agent_list(value)),
            "default_agent" | "default" => default_agent = parse_agent_value(value),
            _ => {}
        }
    }

    AgentPreferences {
        enabled,
        default_agent,
    }
}

fn parse_agent_value(value: &str) -> Option<AgentKind> {
    AgentKind::parse(trim_value_token(value))
}

fn parse_agent_list(value: &str) -> Vec<AgentKind> {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);

    let mut agents = Vec::new();
    for item in inner.split(',') {
        let Some(kind) = AgentKind::parse(trim_value_token(item)) else {
            continue;
        };
        if !agents.contains(&kind) {
            agents.push(kind);
        }
    }
    agents
}

fn trim_value_token(value: &str) -> &str {
    value.trim().trim_matches('"').trim_matches('\'')
}

fn command_exists(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn resolve_with_installed(prefs: AgentPreferences, installed: &[AgentKind]) -> AgentSelection {
    let requested = prefs
        .enabled
        .unwrap_or_else(|| AgentKind::ALL.into_iter().collect());
    let available: Vec<AgentKind> = requested
        .into_iter()
        .filter(|kind| installed.contains(kind))
        .collect();
    let default_agent = prefs
        .default_agent
        .filter(|kind| available.contains(kind))
        .or_else(|| available.first().copied());

    AgentSelection {
        available,
        default_agent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_preferences_defaults_when_empty() {
        let prefs = parse_agent_preferences("");
        assert_eq!(prefs.enabled, None);
        assert_eq!(prefs.default_agent, None);
    }

    #[test]
    fn parse_agent_preferences_with_csv_agents_and_default() {
        let prefs = parse_agent_preferences(
            r#"
agents = "claude, opencode"
default_agent = "claude"
"#,
        );

        assert_eq!(
            prefs.enabled,
            Some(vec![AgentKind::Claude, AgentKind::OpenCode])
        );
        assert_eq!(prefs.default_agent, Some(AgentKind::Claude));
    }

    #[test]
    fn parse_agent_preferences_with_array_syntax() {
        let prefs = parse_agent_preferences(
            r#"
agents = ["opencode", "claude"]
default = "opencode"
"#,
        );

        assert_eq!(
            prefs.enabled,
            Some(vec![AgentKind::OpenCode, AgentKind::Claude])
        );
        assert_eq!(prefs.default_agent, Some(AgentKind::OpenCode));
    }

    #[test]
    fn parse_agent_preferences_supports_codex() {
        let prefs = parse_agent_preferences(
            r#"
agents = ["codex", "claude"]
default_agent = "codex"
"#,
        );

        assert_eq!(
            prefs.enabled,
            Some(vec![AgentKind::Codex, AgentKind::Claude])
        );
        assert_eq!(prefs.default_agent, Some(AgentKind::Codex));
    }

    #[test]
    fn resolve_with_installed_filters_unavailable_agents() {
        let prefs = AgentPreferences {
            enabled: Some(vec![AgentKind::Claude, AgentKind::OpenCode]),
            default_agent: Some(AgentKind::Claude),
        };

        let selection = resolve_with_installed(prefs, &[AgentKind::OpenCode]);
        assert_eq!(selection.available, vec![AgentKind::OpenCode]);
        assert_eq!(selection.default_agent, Some(AgentKind::OpenCode));
    }

    #[test]
    fn resolve_with_installed_uses_builtin_defaults() {
        let prefs = AgentPreferences::default();
        let selection = resolve_with_installed(prefs, &[AgentKind::Claude]);
        assert_eq!(selection.available, vec![AgentKind::Claude]);
        assert_eq!(selection.default_agent, Some(AgentKind::Claude));
    }
}
