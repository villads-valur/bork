use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::global_config::global_config_dir;
use crate::toml_lite::{self, Table};
use crate::types::{AgentKind, AgentMode, Issue};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub project_name: String,
    pub project_root: PathBuf,
    pub agent_kind: AgentKind,
    pub default_prompt: Option<String>,
    pub review_prompt: Option<String>,
    pub done_session_ttl: u64,
    pub debug: bool,
    /// Auto-create issues from PRs the user has been requested to review.
    pub auto_import_reviews: bool,
    /// Auto-create issues from PRs the user has authored.
    pub auto_import_authored_prs: bool,
    /// Allowed agents for this project, if explicitly configured.
    /// `None` means "no restriction; use whatever is installed".
    pub agents_allowlist: Option<Vec<AgentKind>>,
    /// Per-agent launch overrides. Keyed by `AgentKind`. Use
    /// [`AppConfig::launch_args_for`] to resolve mode-specific args.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_launch: HashMap<AgentKind, AgentLaunchConfig>,
}

/// User-controlled invocation args for a single agent, with optional
/// per-mode overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLaunchConfig {
    /// Args always passed to this agent regardless of mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Per-mode args. When `Some`, these *replace* bork's built-in mode
    /// flags for that mode. An empty `Vec` therefore means "no mode flags".
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mode_args: HashMap<AgentMode, Vec<String>>,
}

impl AppConfig {
    /// Look up the configured launch override for an agent, if any.
    pub fn agent_launch_for(&self, kind: AgentKind) -> Option<&AgentLaunchConfig> {
        self.agent_launch.get(&kind)
    }

    /// Resolve the configured args for `(agent, mode)`.
    ///
    /// Returns `(base_args, mode_args_override)` where:
    /// - `base_args` is `agent.<name>.args`, always applied.
    /// - `mode_args_override` is `Some(args)` when the user configured
    ///   per-mode args; the caller should use these *instead of* bork's
    ///   built-in mode flags. `None` means "use built-in mode flags".
    pub fn launch_args_for(
        &self,
        kind: AgentKind,
        mode: AgentMode,
    ) -> (&[String], Option<&[String]>) {
        let Some(cfg) = self.agent_launch_for(kind) else {
            return (&[], None);
        };
        let mode_args = cfg.mode_args.get(&mode).map(Vec::as_slice);
        (&cfg.args, mode_args)
    }
}

pub const DEFAULT_DONE_SESSION_TTL: u64 = 300;

pub const DEFAULT_PROMPT_FALLBACK: &str = "The source code is in main/. Use `bork issue start \"Title\" --project <name-or-path> --prompt \"Details...\"` to spin off new issues with their own worktrees and agents.";

pub const DEFAULT_REVIEW_PROMPT: &str = "Read the diff, check for correctness, regressions, missing tests, and edge cases. Summarize your findings. Use any code review skills that might be installed. Categorize call outs in High, Medium, Low importance. Add file name, linenumber to each call out.";

impl Default for AppConfig {
    fn default() -> Self {
        let project_root = find_project_root();
        Self {
            project_name: default_project_name(&project_root),
            project_root,
            agent_kind: AgentKind::OpenCode,
            default_prompt: None,
            review_prompt: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
            auto_import_reviews: true,
            auto_import_authored_prs: true,
            agents_allowlist: None,
            agent_launch: HashMap::new(),
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
    pub review_prompt: Option<String>,
    pub done_session_ttl: Option<u64>,
    pub debug: Option<bool>,
    pub auto_import_reviews: Option<bool>,
    pub auto_import_authored_prs: Option<bool>,
    pub agents_allowlist: Option<Vec<AgentKind>>,
    /// Per-agent launch overrides parsed from `[agent.<name>]` sections.
    pub agent_launch: HashMap<AgentKind, PartialAgentLaunch>,
}

/// Partial layer for a single agent's launch config. `None` fields are
/// inherited from the layer below; `Some` fields override.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialAgentLaunch {
    pub args: Option<Vec<String>>,
    pub mode_args: HashMap<AgentMode, Vec<String>>,
}

impl PartialAgentLaunch {
    fn merge(self, other: PartialAgentLaunch) -> PartialAgentLaunch {
        let mut mode_args = self.mode_args;
        for (mode, args) in other.mode_args {
            mode_args.insert(mode, args);
        }
        PartialAgentLaunch {
            args: other.args.or(self.args),
            mode_args,
        }
    }

    fn materialize(self) -> AgentLaunchConfig {
        AgentLaunchConfig {
            args: self.args.unwrap_or_default(),
            mode_args: self.mode_args,
        }
    }

    fn is_empty(&self) -> bool {
        self.args.is_none() && self.mode_args.is_empty()
    }
}

impl PartialConfig {
    /// Merge `other` on top of `self`. Any field set in `other` wins.
    fn merge(self, other: PartialConfig) -> PartialConfig {
        let mut agent_launch = self.agent_launch;
        for (kind, layer) in other.agent_launch {
            let merged = agent_launch.remove(&kind).unwrap_or_default().merge(layer);
            agent_launch.insert(kind, merged);
        }
        PartialConfig {
            project_name: other.project_name.or(self.project_name),
            agent_kind: other.agent_kind.or(self.agent_kind),
            default_prompt: other.default_prompt.or(self.default_prompt),
            review_prompt: other.review_prompt.or(self.review_prompt),
            done_session_ttl: other.done_session_ttl.or(self.done_session_ttl),
            debug: other.debug.or(self.debug),
            auto_import_reviews: other.auto_import_reviews.or(self.auto_import_reviews),
            auto_import_authored_prs: other
                .auto_import_authored_prs
                .or(self.auto_import_authored_prs),
            agents_allowlist: other.agents_allowlist.or(self.agents_allowlist),
            agent_launch,
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

    let agent_launch = merged
        .agent_launch
        .into_iter()
        .filter(|(_, layer)| !layer.is_empty())
        .map(|(kind, layer)| (kind, layer.materialize()))
        .collect();

    AppConfig {
        project_name,
        project_root: project_root.to_path_buf(),
        agent_kind: merged.agent_kind.unwrap_or(AgentKind::OpenCode),
        default_prompt: merged.default_prompt,
        review_prompt: merged.review_prompt,
        done_session_ttl: merged.done_session_ttl.unwrap_or(DEFAULT_DONE_SESSION_TTL),
        debug: merged.debug.unwrap_or(false),
        auto_import_reviews: merged.auto_import_reviews.unwrap_or(true),
        auto_import_authored_prs: merged.auto_import_authored_prs.unwrap_or(true),
        agents_allowlist: merged.agents_allowlist,
        agent_launch,
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

    let review_prompt = table
        .get("review_prompt")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let done_session_ttl = table.get("done_session_ttl").and_then(|v| v.as_u64());
    let debug = table.get("debug").and_then(|v| v.as_bool());
    let auto_import_reviews = table.get("auto_import_reviews").and_then(|v| v.as_bool());
    let auto_import_authored_prs = table
        .get("auto_import_authored_prs")
        .and_then(|v| v.as_bool());

    let agents_allowlist = table.get("agents").and_then(|v| v.as_list()).map(|items| {
        items
            .iter()
            .filter_map(|s| AgentKind::parse(s.as_str()))
            .collect::<Vec<_>>()
    });

    let agent_launch = collect_agent_launch(table);

    PartialConfig {
        project_name,
        agent_kind,
        default_prompt,
        review_prompt,
        done_session_ttl,
        debug,
        auto_import_reviews,
        auto_import_authored_prs,
        agents_allowlist,
        agent_launch,
    }
}

/// Scan dotted keys of the form `agent.<name>.args` and
/// `agent.<name>.mode.<mode>.args` and bucket them per agent.
fn collect_agent_launch(table: &Table) -> HashMap<AgentKind, PartialAgentLaunch> {
    let mut out: HashMap<AgentKind, PartialAgentLaunch> = HashMap::new();
    for (key, value) in table {
        let Some(rest) = key.strip_prefix("agent.") else {
            continue;
        };
        let mut parts = rest.split('.');
        let Some(agent_name) = parts.next() else {
            continue;
        };
        let Some(kind) = AgentKind::parse(agent_name) else {
            continue;
        };
        let Some(items) = value.as_list() else {
            continue;
        };
        let args: Vec<String> = items.to_vec();

        let entry = out.entry(kind).or_default();
        match (parts.next(), parts.next(), parts.next()) {
            (Some("args"), None, None) => {
                entry.args = Some(args);
            }
            (Some("mode"), Some(mode_name), Some("args")) if parts.next().is_none() => {
                if let Some(mode) = AgentMode::parse(mode_name) {
                    entry.mode_args.insert(mode, args);
                }
            }
            _ => {}
        }
    }
    out
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

/// Modification time of a project's `.bork/config.toml`, if it exists.
/// Used by the TUI to pick up `bork config set` edits without a restart.
pub fn config_mtime(project_root: &Path) -> Option<SystemTime> {
    fs::metadata(config_path(project_root))
        .ok()?
        .modified()
        .ok()
}

/// Set a single top-level scalar `key = value` in a config file, in place.
///
/// If the key already exists at the top level it is replaced; otherwise it is
/// inserted before the first `[section]` header (or appended if there is none).
/// Comments and unknown lines are preserved. `global` selects the global config
/// file (`~/.config/bork/config.toml`) instead of the project file.
pub fn set_config_value(
    project_root: &Path,
    global: bool,
    key: &str,
    value: &str,
) -> anyhow::Result<PathBuf> {
    let path = if global {
        global_config_path()
    } else {
        config_path(project_root)
    };

    let existing = fs::read_to_string(&path).unwrap_or_default();
    let new_line = format!("{} = {}", key, value);
    let updated = upsert_toml_line(&existing, key, &new_line);

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp_path, updated)?;
    fs::rename(&tmp_path, &path)?;

    Ok(path)
}

/// Replace an existing top-level `key = ...` line with `new_line`, or insert it
/// before the first section header. Only top-level (pre-first-section) keys are
/// matched so we never accidentally edit a key inside a `[section]`.
fn upsert_toml_line(contents: &str, key: &str, new_line: &str) -> String {
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
    let mut in_section = false;
    let mut insert_at = lines.len();
    let mut found = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            if !in_section {
                insert_at = i;
            }
            in_section = true;
            continue;
        }
        if in_section {
            continue;
        }
        if line_key(trimmed) == Some(key) {
            found = true;
            insert_at = i;
            break;
        }
    }

    if found {
        lines[insert_at] = new_line.to_string();
    } else {
        lines.insert(insert_at, new_line.to_string());
    }

    let mut out = lines.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Extract the bare key from a `key = value` line, ignoring comments and blanks.
fn line_key(trimmed: &str) -> Option<&str> {
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (k, _) = trimmed.split_once('=')?;
    Some(k.trim())
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
review_prompt = "Review the thing"
"#,
        );
        assert_eq!(p.project_name.as_deref(), Some("bork"));
        assert_eq!(p.agent_kind, Some(AgentKind::Claude));
        assert_eq!(p.default_prompt.as_deref(), Some("Do the thing"));
        assert_eq!(p.review_prompt.as_deref(), Some("Review the thing"));
    }

    #[test]
    fn parse_partial_review_prompt() {
        let p = parse_partial(r#"review_prompt = "Look closely""#);
        assert_eq!(p.review_prompt.as_deref(), Some("Look closely"));
    }

    #[test]
    fn merge_project_review_prompt_overrides_global() {
        let cfg = merge_to_app(
            r#"review_prompt = "from global""#,
            r#"review_prompt = "from project""#,
        );
        assert_eq!(cfg.review_prompt.as_deref(), Some("from project"));
    }

    #[test]
    fn merge_global_review_prompt_used_when_project_unset() {
        let cfg = merge_to_app(r#"review_prompt = "from global""#, "");
        assert_eq!(cfg.review_prompt.as_deref(), Some("from global"));
    }

    #[test]
    fn merge_empty_layers_leave_review_prompt_unset() {
        let cfg = merge_to_app("", "");
        assert!(cfg.review_prompt.is_none());
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
        assert!(p.review_prompt.is_none());
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
    fn parse_partial_auto_import_flags() {
        let p = parse_partial(
            r#"
auto_import_reviews = false
auto_import_authored_prs = true
"#,
        );
        assert_eq!(p.auto_import_reviews, Some(false));
        assert_eq!(p.auto_import_authored_prs, Some(true));
    }

    #[test]
    fn auto_import_flags_default_to_true() {
        let cfg = merge_to_app("", "");
        assert!(cfg.auto_import_reviews);
        assert!(cfg.auto_import_authored_prs);
    }

    #[test]
    fn auto_import_reviews_project_overrides_global() {
        let cfg = merge_to_app(
            r#"auto_import_reviews = true"#,
            r#"auto_import_reviews = false"#,
        );
        assert!(!cfg.auto_import_reviews);
        // The unset flag still defaults on.
        assert!(cfg.auto_import_authored_prs);
    }

    #[test]
    fn upsert_replaces_existing_top_level_key() {
        let out = upsert_toml_line(
            "project_name = \"bork\"\nauto_import_reviews = true\n",
            "auto_import_reviews",
            "auto_import_reviews = false",
        );
        assert_eq!(
            out,
            "project_name = \"bork\"\nauto_import_reviews = false\n"
        );
    }

    #[test]
    fn upsert_inserts_before_first_section() {
        let out = upsert_toml_line(
            "project_name = \"bork\"\n\n[agent.claude]\nargs = [\"--foo\"]\n",
            "auto_import_reviews",
            "auto_import_reviews = false",
        );
        assert_eq!(
            out,
            "project_name = \"bork\"\n\nauto_import_reviews = false\n[agent.claude]\nargs = [\"--foo\"]\n"
        );
    }

    #[test]
    fn upsert_appends_when_key_absent_and_no_section() {
        let out = upsert_toml_line("project_name = \"bork\"\n", "debug", "debug = true");
        assert_eq!(out, "project_name = \"bork\"\ndebug = true\n");
    }

    #[test]
    fn upsert_does_not_touch_same_key_inside_section() {
        // A key named like ours but nested in a section must not be matched.
        let out = upsert_toml_line(
            "project_name = \"bork\"\n[agent.claude]\ndebug = false\n",
            "debug",
            "debug = true",
        );
        assert_eq!(
            out,
            "project_name = \"bork\"\ndebug = true\n[agent.claude]\ndebug = false\n"
        );
    }

    #[test]
    fn upsert_preserves_comments() {
        let out = upsert_toml_line(
            "# top comment\nproject_name = \"bork\"\n",
            "auto_import_reviews",
            "auto_import_reviews = false",
        );
        assert_eq!(
            out,
            "# top comment\nproject_name = \"bork\"\nauto_import_reviews = false\n"
        );
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
        assert!(cfg.agent_launch.is_empty());
    }

    // --- agent_launch parsing ---

    #[test]
    fn parse_partial_agent_base_args_from_section() {
        let p = parse_partial(
            r#"
[agent.claude]
args = ["--dangerously-skip-permissions"]
"#,
        );
        let claude = p.agent_launch.get(&AgentKind::Claude).unwrap();
        assert_eq!(
            claude.args.as_deref(),
            Some(&["--dangerously-skip-permissions".to_string()][..])
        );
        assert!(claude.mode_args.is_empty());
    }

    #[test]
    fn parse_partial_agent_mode_args_replace_builtins() {
        let p = parse_partial(
            r#"
[agent.claude.mode.build]
args = ["--dangerously-skip-permissions"]
"#,
        );
        let claude = p.agent_launch.get(&AgentKind::Claude).unwrap();
        assert!(claude.args.is_none());
        assert_eq!(
            claude.mode_args.get(&AgentMode::Build).map(Vec::as_slice),
            Some(&["--dangerously-skip-permissions".to_string()][..])
        );
    }

    #[test]
    fn parse_partial_supports_dotted_keys() {
        let p = parse_partial(r#"agent.codex.mode.yolo.args = ["--bypass"]"#);
        let codex = p.agent_launch.get(&AgentKind::Codex).unwrap();
        assert_eq!(
            codex.mode_args.get(&AgentMode::Yolo).map(Vec::as_slice),
            Some(&["--bypass".to_string()][..])
        );
    }

    #[test]
    fn parse_partial_ignores_unknown_agent_or_mode() {
        let p = parse_partial(
            r#"
[agent.bogus]
args = ["--x"]
[agent.claude.mode.wat]
args = ["--y"]
"#,
        );
        // Unknown agent dropped entirely.
        assert!(!p.agent_launch.contains_key(&AgentKind::OpenCode));
        // Unknown mode names produce no mode_args entry for Claude.
        let claude = p.agent_launch.get(&AgentKind::Claude);
        assert!(
            claude.is_none_or(|c| c.mode_args.is_empty()),
            "unknown mode names should not register",
        );
    }

    #[test]
    fn merge_project_agent_launch_overrides_global() {
        let cfg = merge_to_app(
            r#"
[agent.claude]
args = ["--from-global"]
"#,
            r#"
[agent.claude]
args = ["--from-project"]
"#,
        );
        let claude = cfg.agent_launch.get(&AgentKind::Claude).unwrap();
        assert_eq!(claude.args, vec!["--from-project".to_string()]);
    }

    #[test]
    fn merge_layers_keep_disjoint_agent_launch_keys() {
        let cfg = merge_to_app(
            r#"
[agent.claude]
args = ["--global-claude"]
"#,
            r#"
[agent.codex]
args = ["--project-codex"]
"#,
        );
        let claude = cfg.agent_launch.get(&AgentKind::Claude).unwrap();
        assert_eq!(claude.args, vec!["--global-claude".to_string()]);
        let codex = cfg.agent_launch.get(&AgentKind::Codex).unwrap();
        assert_eq!(codex.args, vec!["--project-codex".to_string()]);
    }

    #[test]
    fn merge_project_clears_mode_args_with_empty_array() {
        let cfg = merge_to_app(
            r#"
[agent.claude.mode.plan]
args = ["--permission-mode", "plan"]
"#,
            r#"
[agent.claude.mode.plan]
args = []
"#,
        );
        let claude = cfg.agent_launch.get(&AgentKind::Claude).unwrap();
        let plan_args = claude.mode_args.get(&AgentMode::Plan).unwrap();
        assert!(plan_args.is_empty());
    }

    #[test]
    fn launch_args_for_returns_base_and_mode_override() {
        let cfg = merge_to_app(
            "",
            r#"
[agent.claude]
args = ["--extra"]
[agent.claude.mode.build]
args = ["--dangerously-skip-permissions"]
"#,
        );
        let (base, mode_args) = cfg.launch_args_for(AgentKind::Claude, AgentMode::Build);
        assert_eq!(base, &["--extra".to_string()]);
        assert_eq!(
            mode_args,
            Some(&["--dangerously-skip-permissions".to_string()][..])
        );
    }

    #[test]
    fn launch_args_for_returns_none_when_unconfigured() {
        let cfg = merge_to_app("", "");
        let (base, mode_args) = cfg.launch_args_for(AgentKind::Claude, AgentMode::Plan);
        assert!(base.is_empty());
        assert!(mode_args.is_none());
    }
}
