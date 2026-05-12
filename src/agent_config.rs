use std::path::Path;
use std::process::{Command, Stdio};

use crate::config;
use crate::types::AgentKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSelection {
    pub available: Vec<AgentKind>,
    pub default_agent: Option<AgentKind>,
}

/// Resolve the available agent set + default from the layered config.
///
/// `project_root` is optional so this can run before a project has been
/// chosen (e.g. cold startup with no `.bork/`). When provided, the project's
/// allowlist and default override the global one.
pub fn resolve_agent_selection(project_root: Option<&Path>) -> AgentSelection {
    let prefs = load_layered_prefs(project_root);
    let installed: Vec<AgentKind> = AgentKind::ALL
        .into_iter()
        .filter(|kind| command_exists(kind.command()))
        .collect();
    resolve_with_installed(prefs, &installed)
}

#[derive(Debug, Clone, Default)]
struct AgentPreferences {
    enabled: Option<Vec<AgentKind>>,
    default_agent: Option<AgentKind>,
}

/// Read the layered config and project the agent-relevant fields out of it.
/// `load_config_from` already handles the global + project merge, so we just
/// pick what we need; for the no-project case we read the global layer alone.
fn load_layered_prefs(project_root: Option<&Path>) -> AgentPreferences {
    if let Some(root) = project_root {
        let merged = config::load_config_from(root);
        return AgentPreferences {
            enabled: merged.agents_allowlist,
            default_agent: Some(merged.agent_kind),
        };
    }

    let global = config::load_global_partial();
    AgentPreferences {
        enabled: global.agents_allowlist,
        default_agent: global.agent_kind,
    }
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

/// Print a one-line warning to stderr if the user still has a legacy
/// `~/.config/bork/agents.toml`. The file is no longer read; bork-119 was a
/// hard cutover.
pub fn warn_if_legacy_agents_file() {
    let path = config::legacy_agents_config_path();
    if path.exists() {
        eprintln!(
            "warning: {} is no longer read. Move its contents to {} (keys: agents, default_agent).",
            path.display(),
            config::global_config_path().display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn resolve_with_installed_honors_explicit_default() {
        let prefs = AgentPreferences {
            enabled: Some(vec![AgentKind::OpenCode, AgentKind::Claude]),
            default_agent: Some(AgentKind::Claude),
        };
        let selection = resolve_with_installed(prefs, &[AgentKind::OpenCode, AgentKind::Claude]);
        assert_eq!(selection.default_agent, Some(AgentKind::Claude));
    }

    #[test]
    fn resolve_with_installed_falls_back_when_default_uninstalled() {
        let prefs = AgentPreferences {
            enabled: Some(vec![AgentKind::OpenCode, AgentKind::Claude]),
            default_agent: Some(AgentKind::Claude),
        };
        let selection = resolve_with_installed(prefs, &[AgentKind::OpenCode]);
        assert_eq!(selection.default_agent, Some(AgentKind::OpenCode));
    }
}
