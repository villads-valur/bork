use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::{self, AppConfig};
use crate::error::AppError;
use crate::external::process::{self, BORK_SESSION_VAR};
use crate::external::{github, tmux};
use crate::types::{AgentKind, AgentMode, Issue, IssueKind, LinkedGithubPr, LinkedLinear};

/// Launch an agent session for an issue.
/// Creates a tmux session with two windows:
///   1. The agent (opencode/claude/codex/pi) launched at the project root with issue context
///   2. A bare terminal
///
/// Exports BORK_SESSION and BORK_STATUS_DIR so hooks/plugins can write status files.
/// Returns (tmux_session_name, agent_session_id).
/// The agent_session_id is the agent's internal session ID for resuming conversations:
///   - Claude: UUID pre-assigned via --session-id
///   - OpenCode: ses_xxx detected by polling `opencode session list` after launch
///   - Codex: UUID detected from newest ~/.codex/sessions rollout transcript
pub fn launch_session(
    issue: &Issue,
    config: &AppConfig,
) -> Result<(String, Option<String>, bool), AppError> {
    let session_name = issue.session_name(&config.project_name);
    let cwd = &config.project_root;

    if tmux::session_exists(&session_name) {
        return Ok((
            session_name.clone(),
            issue.current_session_id().map(str::to_string),
            false,
        ));
    }

    tmux::create_session(&session_name, cwd)?;

    let status_dir = config::agent_status_dir(&config.project_root);
    let status_dir_str = status_dir.to_str().unwrap_or("");

    let prompt_path = prompt_file_path(&status_dir, &session_name);
    let prompt_path_str = prompt_path.to_str().unwrap_or("");

    let (agent_cmd, pre_assigned_session_id, prompt_contents) = build_agent_cmd(
        issue,
        config,
        &session_name,
        status_dir_str,
        prompt_path_str,
    );

    // Multiline prompts can't be typed directly: tmux send-keys collapses
    // embedded newlines. Instead we write the prompt to a file and the launch
    // command reads it back via `"$(cat ...)"`, which preserves it verbatim.
    if let Some(contents) = &prompt_contents {
        write_prompt_file(&prompt_path, contents)?;
    }

    // Fresh sessions with a worktree run the configured setup script inside
    // the worktree first; `&&` ensures the agent only starts if it succeeds
    // and its output stays visible in the agent window.
    let setup = setup_prefix(issue, config);
    let setup_ran = setup.is_some();
    let agent_cmd = match setup {
        Some(prefix) => format!("{} && {}", prefix, agent_cmd),
        None => agent_cmd,
    };

    // Snapshot opencode's visible sessions before the agent starts, so the
    // post-launch detector only ever adopts a genuinely new id — the newest
    // global session could belong to any concurrent opencode run. (OpenCode
    // never pre-assigns an id, so agent kind alone gates the snapshot.)
    let opencode_before = if issue.agent_kind == AgentKind::OpenCode {
        list_opencode_session_ids()
    } else {
        HashSet::new()
    };

    tmux::send_keys(&session_name, &agent_cmd)?;

    // Second window: bare terminal for ad-hoc commands
    tmux::create_window(&session_name, "terminal", cwd)?;

    // For OpenCode/Codex, detect session IDs after launch
    let agent_session_id = match pre_assigned_session_id {
        Some(id) => Some(id),
        None => match issue.agent_kind {
            AgentKind::OpenCode => detect_opencode_session_id(&opencode_before),
            AgentKind::Claude => None,
            AgentKind::Codex => detect_codex_session_id(),
            AgentKind::Pi => detect_pi_session_id(&config.project_root),
        },
    };

    Ok((session_name, agent_session_id, setup_ran))
}

/// Kill an issue's tmux session and remove its transient hook/prompt files.
///
/// Returns whether a live tmux session was killed. An already-absent session
/// is a successful no-op.
pub fn terminate_session(project_root: &Path, session_name: &str) -> Result<bool, AppError> {
    let process_sessions = capture_process_sessions(session_name)?;
    let killed = kill_tmux_session(session_name)?;

    let status_dir = config::agent_status_dir(project_root);
    let _ = fs::remove_file(status_dir.join(format!("{session_name}.json")));
    let _ = fs::remove_file(prompt_file_path(&status_dir, session_name));

    // Killing tmux does not guarantee that detached descendants exited.
    process::kill_session_survivors(session_name, &process_sessions);

    Ok(killed)
}

#[cfg(not(test))]
fn capture_process_sessions(session_name: &str) -> Result<Vec<i32>, AppError> {
    let pane_pids = tmux::pane_pids(session_name)?;
    Ok(process::process_session_ids(&pane_pids))
}

#[cfg(test)]
fn capture_process_sessions(_session_name: &str) -> Result<Vec<i32>, AppError> {
    Ok(Vec::new())
}

/// `tmux::kill_session`, stubbed under test so the suite never talks to a
/// live tmux server.
#[cfg(not(test))]
fn kill_tmux_session(session_name: &str) -> Result<bool, AppError> {
    tmux::kill_session(session_name)
}

#[cfg(test)]
fn kill_tmux_session(_session_name: &str) -> Result<bool, AppError> {
    Ok(false)
}

/// Build the setup-script prefix for a launch command, if applicable.
/// Skipped once `setup_ran` is set — see `Issue::attach_worktree` for the
/// once-per-worktree rule. The script itself is user-authored shell and is
/// inserted verbatim; the worktree dir is escaped. The tmux session's cwd is
/// the project root, so the relative worktree dir resolves correctly.
fn setup_prefix(issue: &Issue, config: &AppConfig) -> Option<String> {
    if issue.setup_ran {
        return None;
    }
    let worktree = issue.worktree.as_deref()?;
    let script = config.setup_script.as_deref()?;
    Some(format!(
        "(cd '{}' && {})",
        shell_escape_single_quotes(worktree),
        script
    ))
}

/// Path for an issue's staged prompt file, scoped to its session name so
/// concurrent launches don't collide.
fn prompt_file_path(status_dir: &Path, session_name: &str) -> PathBuf {
    status_dir.join(format!("prompt-{session_name}.txt"))
}

/// Write the prompt to disk (creating the parent dir if needed) so the launch
/// command can read it back verbatim.
fn write_prompt_file(path: &Path, contents: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    fs::write(path, contents).map_err(AppError::Io)
}

/// Build the agent launch command and return
/// (command, pre_assigned_session_id, prompt_contents).
/// For Claude, pre-assigns a UUID and returns it. For OpenCode, returns None (ID detected post-launch).
/// `prompt_contents` is `Some` for fresh sessions (staged to a file by the
/// caller) and `None` for resume sessions, which carry no prompt.
fn build_agent_cmd(
    issue: &Issue,
    config: &AppConfig,
    session_name: &str,
    status_dir_str: &str,
    prompt_path_str: &str,
) -> (String, Option<String>, Option<String>) {
    let env_prefix = format!(
        "export {BORK_SESSION_VAR}='{}' BORK_STATUS_DIR='{}' BORK_ISSUE_ID='{}'",
        shell_escape_single_quotes(session_name),
        shell_escape_single_quotes(status_dir_str),
        shell_escape_single_quotes(&issue.id),
    );

    // Builds the full issue prompt text. Lazy: only invoked for fresh sessions,
    // since resume paths skip the prompt entirely. Returned verbatim (no shell
    // escaping) because it's staged to a file and read back via `"$(cat ...)"`.
    let build_prompt_contents = || {
        let default_prompt = if issue.kind == IssueKind::Orchestrator {
            config
                .orchestrator_prompt
                .as_deref()
                .unwrap_or(config::DEFAULT_ORCHESTRATOR_PROMPT)
        } else {
            config
                .default_prompt
                .as_deref()
                .unwrap_or(config::DEFAULT_PROMPT_FALLBACK)
        };
        let main_worktree = config.project_root.join("main");
        let mut prompt = build_prompt(
            &issue.id,
            &issue.title,
            default_prompt,
            issue.prompt.as_deref(),
            &issue.linear_links,
            &issue.github_pr_links,
            |number| github::pr_url(&main_worktree, number),
        );
        prompt.push_str("\n\nBork project: ");
        prompt.push_str(&config.project_name);
        if let Some(worktree) = issue.worktree.as_deref() {
            prompt.push_str("\n\nAssigned worktree: ");
            prompt.push_str(worktree);
            prompt.push_str(". Do all work for this issue inside that directory.");
        }
        if issue.kind == IssueKind::Orchestrator {
            prompt.push_str(&format!(
                "\n\nMaintain your plan in plans/{}/planning.md, relative to the project root (the directory containing main/).",
                issue.id
            ));
        }
        prompt
    };

    // Shell snippet that expands to the prompt at runtime by reading the staged
    // file, plus a cleanup suffix that removes the file afterwards. Using a file
    // sidesteps tmux send-keys mangling newlines in the typed command line.
    let escaped_prompt_path = shell_escape_single_quotes(prompt_path_str);
    let prompt_subst = format!("\"$(cat '{}')\"", escaped_prompt_path);
    let prompt_cleanup = format!("; rm -f '{}'", escaped_prompt_path);

    // Built-in mode flags. These are replaced when the user configured
    // per-mode args under `[agent.<name>.mode.<mode>]`.
    let builtin_mode_flag = match issue.agent_kind {
        AgentKind::OpenCode => match issue.agent_mode {
            // OpenCode has no yolo mode; treat it as Build.
            AgentMode::Plan => "--agent plan",
            AgentMode::Build | AgentMode::Yolo => "",
        },
        AgentKind::Claude => match issue.agent_mode {
            AgentMode::Plan => "--permission-mode plan",
            AgentMode::Yolo => "--dangerously-skip-permissions",
            AgentMode::Build => "",
        },
        AgentKind::Codex => match issue.agent_mode {
            // `--full-auto` is deprecated upstream; use explicit sandbox + approval flags.
            AgentMode::Plan => "--sandbox workspace-write --ask-for-approval on-request",
            AgentMode::Build => "--sandbox workspace-write --ask-for-approval never",
            AgentMode::Yolo => "--dangerously-bypass-approvals-and-sandbox",
        },
        // Pi has a single mode and no built-in plan/yolo flags. Users can still
        // add per-mode args via `[agent.pi.mode.<mode>]` if desired.
        AgentKind::Pi => "",
    };

    let trailing = trailing_args(
        config,
        issue.agent_kind,
        issue.agent_mode,
        builtin_mode_flag,
    );

    let current_session = issue.current_session_id();

    match issue.agent_kind {
        AgentKind::OpenCode => {
            if let Some(sid) = current_session {
                // Resume existing session — skip --prompt, history is preserved
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && opencode --session '{}'{}",
                    env_prefix, escaped_sid, trailing,
                );
                (cmd, None, None)
            } else {
                let cmd = format!(
                    "{} && opencode --prompt {}{}{}",
                    env_prefix, prompt_subst, trailing, prompt_cleanup,
                );
                (cmd, None, Some(build_prompt_contents()))
            }
        }
        AgentKind::Claude => {
            let session_display_name = format!("{}: {}", issue.id, issue.title);
            let escaped_name = shell_escape_single_quotes(&session_display_name);

            if let Some(sid) = current_session {
                // Resume existing session — skip the prompt, history is preserved
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && claude --name '{}'{} --resume '{}'",
                    env_prefix, escaped_name, trailing, escaped_sid,
                );
                (cmd, Some(sid.to_string()), None)
            } else {
                // Fresh session: stage prompt and optionally pre-assign a UUID
                let prompt = build_prompt_contents();
                let uuid = generate_uuid().unwrap_or_default();
                if uuid.is_empty() {
                    let cmd = format!(
                        "{} && claude --name '{}'{} {}{}",
                        env_prefix, escaped_name, trailing, prompt_subst, prompt_cleanup,
                    );
                    (cmd, None, Some(prompt))
                } else {
                    let escaped_uuid = shell_escape_single_quotes(&uuid);
                    let cmd = format!(
                        "{} && claude --name '{}'{} --session-id '{}' {}{}",
                        env_prefix,
                        escaped_name,
                        trailing,
                        escaped_uuid,
                        prompt_subst,
                        prompt_cleanup,
                    );
                    (cmd, Some(uuid), Some(prompt))
                }
            }
        }
        AgentKind::Codex => {
            if let Some(sid) = current_session {
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && codex resume '{}'{}",
                    env_prefix, escaped_sid, trailing
                );
                (cmd, Some(sid.to_string()), None)
            } else {
                let cmd = format!(
                    "{} && codex{} {}{}",
                    env_prefix, trailing, prompt_subst, prompt_cleanup,
                );
                (cmd, None, Some(build_prompt_contents()))
            }
        }
        AgentKind::Pi => {
            let session_display_name = format!("{}: {}", issue.id, issue.title);
            let escaped_name = shell_escape_single_quotes(&session_display_name);

            if let Some(sid) = current_session {
                // Resume existing session — skip the prompt, history is preserved.
                let escaped_sid = shell_escape_single_quotes(sid);
                let cmd = format!(
                    "{} && pi --session '{}'{}",
                    env_prefix, escaped_sid, trailing,
                );
                (cmd, Some(sid.to_string()), None)
            } else {
                let cmd = format!(
                    "{} && pi --name '{}'{} {}{}",
                    env_prefix, escaped_name, trailing, prompt_subst, prompt_cleanup,
                );
                (cmd, None, Some(build_prompt_contents()))
            }
        }
    }
}

/// Build the trailing args string (always starts with a leading space when
/// non-empty) that gets appended to each agent invocation.
///
/// Resolution:
/// - Base args from `[agent.<name>].args` are always appended.
/// - Per-mode args from `[agent.<name>.mode.<mode>].args` replace the
///   built-in mode flags. Set to `[]` to suppress mode flags entirely.
/// - When no per-mode override is configured, bork's built-in mode flags
///   are used.
///
/// All configured args are individually shell-escaped so values containing
/// spaces or quotes are passed through safely.
fn trailing_args(
    config: &AppConfig,
    kind: AgentKind,
    mode: AgentMode,
    builtin_mode_flag: &str,
) -> String {
    let (base_args, mode_args_override) = config.launch_args_for(kind, mode);

    let mut parts: Vec<String> = Vec::new();
    match mode_args_override {
        Some(args) => {
            for arg in args {
                parts.push(format!("'{}'", shell_escape_single_quotes(arg)));
            }
        }
        None => {
            let trimmed = builtin_mode_flag.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    for arg in base_args {
        parts.push(format!("'{}'", shell_escape_single_quotes(arg)));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

/// Generate a UUID using the system `uuidgen` command.
fn generate_uuid() -> Option<String> {
    Command::new("uuidgen")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
}

/// Poll `opencode session list` until an id appears that wasn't in the
/// pre-launch snapshot. Returns it if found within ~5 seconds, otherwise
/// None — the newest global session could belong to any concurrent
/// opencode run, so only a genuinely new id is trusted.
fn detect_opencode_session_id(before: &HashSet<String>) -> Option<String> {
    // Give OpenCode a moment to create its session before polling
    std::thread::sleep(Duration::from_millis(800));

    for _ in 0..9 {
        if let Some(sid) = list_opencode_session_ids()
            .into_iter()
            .find(|id| !before.contains(id))
        {
            return Some(sid);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Attributes legacy (pre-map) session ids to the agent that minted them,
/// snapshotting the enumerable transcript stores at most once — collection
/// walks `~/.codex/sessions` and the Pi session dir, so it only happens if a
/// legacy UUID actually needs attributing.
pub struct LegacySessionStores {
    project_root: PathBuf,
    stores: Option<(HashSet<String>, HashSet<String>)>,
}

impl LegacySessionStores {
    pub fn new(project_root: &Path) -> Self {
        LegacySessionStores {
            project_root: project_root.to_path_buf(),
            stores: None,
        }
    }

    /// Attribute a legacy session id to the agent that minted it. OpenCode
    /// ids are "ses_"-prefixed; the UUID agents are told apart via their
    /// on-disk transcript stores — an id found there is re-keyed even when
    /// the issue pointed at a different agent (the pre-map bug left such
    /// mismatches behind). Claude has no cheap index, so it is assumed only
    /// when the issue already pointed at Claude. `None` = drop the id.
    pub fn attribute(&mut self, sid: &str, current: AgentKind) -> Option<AgentKind> {
        if sid.starts_with("ses_") {
            return Some(AgentKind::OpenCode);
        }
        if !is_uuid_like(sid) {
            return None;
        }
        let (codex_ids, pi_ids) = self.stores.get_or_insert_with(|| {
            (
                codex_sessions_root()
                    .map(|root| collect_codex_session_ids(&root))
                    .unwrap_or_default(),
                pi_sessions_dir(&self.project_root)
                    .map(|dir| collect_pi_session_ids(&dir))
                    .unwrap_or_default(),
            )
        });
        attribute_uuid_against_stores(sid, current, codex_ids, pi_ids)
    }
}

fn attribute_uuid_against_stores(
    sid: &str,
    current: AgentKind,
    codex_ids: &HashSet<String>,
    pi_ids: &HashSet<String>,
) -> Option<AgentKind> {
    if codex_ids.contains(sid) {
        return Some(AgentKind::Codex);
    }
    if pi_ids.contains(sid) {
        return Some(AgentKind::Pi);
    }
    // Not in an enumerable store: trust Claude ownership only when the issue
    // already pointed at Claude. Under Codex/Pi the id is evicted or foreign,
    // and under OpenCode a UUID can't be attributed at all — resuming a
    // foreign id is worse than starting fresh once.
    if current == AgentKind::Claude {
        Some(AgentKind::Claude)
    } else {
        None
    }
}

fn codex_sessions_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Sleep 800ms for the agent to create its session, then poll ~5s for an id
/// the pre-launch snapshot lacks. Only a genuinely new id is trusted: the
/// globally newest one could be any unrelated conversation (concurrent runs,
/// or a launch that failed or was killed mid-flight), and returning None
/// just means the next launch starts fresh instead of resuming a foreign
/// session.
fn poll_for_new_session_id(
    before: &HashSet<String>,
    snapshot: impl Fn() -> HashSet<String>,
) -> Option<String> {
    std::thread::sleep(Duration::from_millis(800));

    for _ in 0..9 {
        if let Some(id) = snapshot().into_iter().find(|id| !before.contains(id)) {
            return Some(id);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Detect a newly created Codex session UUID by scanning ~/.codex/sessions.
fn detect_codex_session_id() -> Option<String> {
    let sessions_root = codex_sessions_root()?;
    let before = collect_codex_session_ids(&sessions_root);
    poll_for_new_session_id(&before, || collect_codex_session_ids(&sessions_root))
}

/// Collect all Codex session IDs.
fn collect_codex_session_ids(sessions_root: &Path) -> HashSet<String> {
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
            let Some(session_id) = parse_codex_session_id_from_filename(file_name) else {
                continue;
            };
            sessions.insert(session_id);
        }
    }

    sessions
}

fn parse_codex_session_id_from_filename(file_name: &str) -> Option<String> {
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

fn is_uuid_like(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.chars().enumerate().all(|(i, ch)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            ch == '-'
        } else {
            ch.is_ascii_hexdigit()
        }
    })
}

/// Detect a newly created Pi session UUID by scanning Pi's per-cwd session
/// directory. Pi stores sessions under `<sessions_root>/--<cwd>--/` as
/// `<timestamp>_<uuid>.jsonl`, where `<cwd>` has `/` replaced by `-`.
fn detect_pi_session_id(project_root: &Path) -> Option<String> {
    let sessions_dir = pi_sessions_dir(project_root)?;
    let before = collect_pi_session_ids(&sessions_dir);
    poll_for_new_session_id(&before, || collect_pi_session_ids(&sessions_dir))
}

/// Resolve Pi's session directory for a given working directory.
/// Honors `PI_CODING_AGENT_SESSION_DIR` (flat dir) and `PI_CODING_AGENT_DIR`
/// overrides, falling back to `~/.pi/agent/sessions/--<cwd>--/`.
fn pi_sessions_dir(project_root: &Path) -> Option<PathBuf> {
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
fn collect_pi_session_ids(sessions_dir: &Path) -> HashSet<String> {
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
        let Some(session_id) = parse_pi_session_id_from_filename(file_name) else {
            continue;
        };
        sessions.insert(session_id);
    }
    sessions
}

/// Extract the session UUID from a Pi session filename (`<timestamp>_<uuid>.jsonl`).
fn parse_pi_session_id_from_filename(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".jsonl")?;
    let candidate = stem.rsplit('_').next()?;
    if is_uuid_like(candidate) {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// Run `opencode session list` and return every session ID found.
/// Session IDs start with "ses_", one per line.
fn list_opencode_session_ids() -> HashSet<String> {
    let Ok(output) = Command::new("opencode").args(["session", "list"]).output() else {
        return HashSet::new();
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_all_session_ids(&stdout)
}

/// Parse every session ID from `opencode session list` output.
/// Expected format: each line starts with the session ID (ses_xxx).
fn parse_all_session_ids(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let token = line.split_whitespace().next()?;
            if token.starts_with("ses_") {
                Some(token.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Build the full prompt sent to the agent.
/// Always starts with issue context and the default prompt, then optionally
/// includes Linear tickets and GitHub PRs linked to the issue, then appends
/// the user's custom prompt (if any) after a blank line.
///
/// `pr_url_resolver` is called once per linked PR to fetch its canonical URL.
/// If it returns `None` (e.g. `gh` not available, non-GitHub remote), the PR
/// is rendered as `#{number}` with no URL.
fn build_prompt(
    id: &str,
    title: &str,
    default_prompt: &str,
    user_prompt: Option<&str>,
    linear_links: &[LinkedLinear],
    github_pr_links: &[LinkedGithubPr],
    pr_url_resolver: impl Fn(u32) -> Option<String>,
) -> String {
    let mut prompt = format!("You are working on {}: {}. {}", id, title, default_prompt);

    append_section(
        &mut prompt,
        linear_links,
        "Linear ticket",
        "Linear tickets",
        |link| {
            if link.url.is_empty() {
                link.identifier.clone()
            } else {
                format!("{} - {}", link.identifier, link.url)
            }
        },
    );
    append_section(
        &mut prompt,
        github_pr_links,
        "GitHub PR",
        "GitHub PRs",
        |link| match pr_url_resolver(link.number) {
            Some(url) => format!("#{} - {}", link.number, url),
            None => format!("#{}", link.number),
        },
    );

    if let Some(user_text) = user_prompt {
        let trimmed = user_text.trim();
        if !trimmed.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(trimmed);
        }
    }

    prompt
}

fn append_section<T>(
    prompt: &mut String,
    items: &[T],
    singular: &str,
    plural: &str,
    format_entry: impl Fn(&T) -> String,
) {
    use std::fmt::Write;
    match items {
        [] => {}
        [single] => {
            let _ = write!(
                prompt,
                "\n\nThis issue has a {}: {}",
                singular,
                format_entry(single)
            );
        }
        many => {
            let _ = write!(prompt, "\n\nThis issue has {}:", plural);
            for item in many {
                let _ = write!(prompt, "\n- {}", format_entry(item));
            }
        }
    }
}

/// Escape a string for use inside single quotes in a shell command.
fn shell_escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DEFAULT: &str = "The source code is in main/.";

    fn linear(identifier: &str, url: &str) -> LinkedLinear {
        LinkedLinear {
            id: format!("uuid-{}", identifier),
            identifier: identifier.to_string(),
            url: url.to_string(),
            imported: false,
        }
    }

    fn pr(number: u32) -> LinkedGithubPr {
        LinkedGithubPr {
            number,
            imported: false,
            import_source: None,
        }
    }

    /// PR URL resolver that always returns None (simulates no `gh` available).
    fn no_pr_url(_: u32) -> Option<String> {
        None
    }

    /// PR URL resolver that always returns a stable test URL.
    fn test_pr_url(number: u32) -> Option<String> {
        Some(format!("https://github.com/test/repo/pull/{}", number))
    }

    // --- append_section ---

    #[test]
    fn append_section_empty_is_noop() {
        let mut out = String::from("base");
        let empty: [u32; 0] = [];
        append_section(&mut out, &empty, "thing", "things", |n| n.to_string());
        assert_eq!(out, "base");
    }

    #[test]
    fn append_section_single_uses_singular_label() {
        let mut out = String::from("base");
        append_section(&mut out, &[42u32], "thing", "things", |n| n.to_string());
        assert_eq!(out, "base\n\nThis issue has a thing: 42");
    }

    #[test]
    fn append_section_multiple_uses_plural_and_bullet_list() {
        let mut out = String::from("base");
        append_section(&mut out, &[1u32, 2, 3], "thing", "things", |n| {
            n.to_string()
        });
        assert_eq!(out, "base\n\nThis issue has things:\n- 1\n- 2\n- 3");
    }

    #[test]
    fn append_section_uses_formatter_closure() {
        let mut out = String::new();
        append_section(&mut out, &[7u32], "PR", "PRs", |n| format!("#{}", n));
        assert_eq!(out, "\n\nThis issue has a PR: #7");
    }

    #[test]
    fn append_section_formatter_applied_to_each_item() {
        let mut out = String::new();
        append_section(&mut out, &[1u32, 2], "PR", "PRs", |n| format!("#{}", n));
        assert_eq!(out, "\n\nThis issue has PRs:\n- #1\n- #2");
    }

    #[test]
    fn append_section_appends_to_existing_content() {
        let mut out = String::from("prefix\n\nmiddle");
        append_section(&mut out, &[1u32], "x", "xs", |n| n.to_string());
        assert_eq!(out, "prefix\n\nmiddle\n\nThis issue has a x: 1");
    }

    #[test]
    fn append_section_works_with_linked_linear() {
        let links = [linear("VIL-1", "https://linear.app/VIL-1")];
        let mut out = String::new();
        append_section(&mut out, &links, "Linear ticket", "Linear tickets", |l| {
            if l.url.is_empty() {
                l.identifier.clone()
            } else {
                format!("{} - {}", l.identifier, l.url)
            }
        });
        assert_eq!(
            out,
            "\n\nThis issue has a Linear ticket: VIL-1 - https://linear.app/VIL-1"
        );
    }

    #[test]
    fn append_section_works_with_linked_github_pr() {
        let prs = [pr(10), pr(11)];
        let mut out = String::new();
        append_section(&mut out, &prs, "GitHub PR", "GitHub PRs", |p| {
            format!("#{}", p.number)
        });
        assert_eq!(out, "\n\nThis issue has GitHub PRs:\n- #10\n- #11");
    }

    // --- build_prompt ---

    #[test]
    fn build_prompt_without_user_prompt() {
        let result = build_prompt(
            "bork-1",
            "Fix auth",
            TEST_DEFAULT,
            None,
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-1: Fix auth. The source code is in main/."
        );
    }

    #[test]
    fn build_prompt_with_user_prompt() {
        let result = build_prompt(
            "bork-2",
            "Add tests",
            TEST_DEFAULT,
            Some("Focus on unit tests"),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-2: Add tests. The source code is in main/.\n\nFocus on unit tests"
        );
    }

    #[test]
    fn build_prompt_with_empty_user_prompt() {
        let result = build_prompt(
            "bork-3",
            "Refactor",
            TEST_DEFAULT,
            Some(""),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-3: Refactor. The source code is in main/."
        );
    }

    #[test]
    fn build_prompt_with_whitespace_only_user_prompt() {
        let result = build_prompt(
            "bork-4",
            "Cleanup",
            TEST_DEFAULT,
            Some("   \n  "),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-4: Cleanup. The source code is in main/."
        );
    }

    #[test]
    fn build_prompt_user_prompt_is_trimmed() {
        let result = build_prompt(
            "bork-5",
            "Feature",
            TEST_DEFAULT,
            Some("  do the thing  "),
            &[],
            &[],
            no_pr_url,
        );
        assert!(result.ends_with("\n\ndo the thing"));
    }

    #[test]
    fn build_prompt_with_real_default_fallback() {
        let result = build_prompt(
            "bork-6",
            "New feature",
            config::DEFAULT_PROMPT_FALLBACK,
            None,
            &[],
            &[],
            no_pr_url,
        );
        assert!(result.starts_with("You are working on bork-6: New feature."));
        assert!(result.contains("source code is in main/"));
        assert!(result.contains("bork issue start"));
    }

    #[test]
    fn build_prompt_with_custom_default_prompt() {
        let result = build_prompt(
            "proj-1",
            "Setup",
            "Read README.md first.",
            Some("Install deps"),
            &[],
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on proj-1: Setup. Read README.md first.\n\nInstall deps"
        );
    }

    #[test]
    fn build_prompt_with_single_linear_link() {
        let links = [linear("VIL-123", "https://linear.app/team/issue/VIL-123")];
        let result = build_prompt(
            "vil-123",
            "Fix auth flow",
            TEST_DEFAULT,
            None,
            &links,
            &[],
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on vil-123: Fix auth flow. The source code is in main/.\n\nThis issue has a Linear ticket: VIL-123 - https://linear.app/team/issue/VIL-123"
        );
    }

    #[test]
    fn build_prompt_with_linear_link_missing_url() {
        let links = [linear("VIL-123", "")];
        let result = build_prompt(
            "bork-1",
            "Fix bug",
            TEST_DEFAULT,
            None,
            &links,
            &[],
            no_pr_url,
        );
        assert!(result.ends_with("\n\nThis issue has a Linear ticket: VIL-123"));
        assert!(!result.contains("VIL-123 -"));
    }

    #[test]
    fn build_prompt_with_multiple_linear_links() {
        let links = [
            linear("VIL-1", "https://linear.app/team/issue/VIL-1"),
            linear("VIL-2", "https://linear.app/team/issue/VIL-2"),
        ];
        let result = build_prompt(
            "bork-1",
            "Refactor",
            TEST_DEFAULT,
            None,
            &links,
            &[],
            no_pr_url,
        );
        assert!(result.contains(
            "\n\nThis issue has Linear tickets:\n- VIL-1 - https://linear.app/team/issue/VIL-1\n- VIL-2 - https://linear.app/team/issue/VIL-2"
        ));
    }

    #[test]
    fn build_prompt_with_single_github_pr_and_url() {
        let prs = [pr(42)];
        let result = build_prompt(
            "bork-1",
            "Fix bug",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            test_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-1: Fix bug. The source code is in main/.\n\nThis issue has a GitHub PR: #42 - https://github.com/test/repo/pull/42"
        );
    }

    #[test]
    fn build_prompt_with_single_github_pr_without_url() {
        let prs = [pr(42)];
        let result = build_prompt(
            "bork-1",
            "Fix bug",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            no_pr_url,
        );
        assert_eq!(
            result,
            "You are working on bork-1: Fix bug. The source code is in main/.\n\nThis issue has a GitHub PR: #42"
        );
    }

    #[test]
    fn build_prompt_with_multiple_github_prs_and_urls() {
        let prs = [pr(1), pr(2), pr(3)];
        let result = build_prompt(
            "bork-1",
            "Refactor",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            test_pr_url,
        );
        assert!(result.contains(
            "\n\nThis issue has GitHub PRs:\n- #1 - https://github.com/test/repo/pull/1\n- #2 - https://github.com/test/repo/pull/2\n- #3 - https://github.com/test/repo/pull/3"
        ));
    }

    #[test]
    fn build_prompt_with_multiple_github_prs_without_urls() {
        let prs = [pr(1), pr(2), pr(3)];
        let result = build_prompt(
            "bork-1",
            "Refactor",
            TEST_DEFAULT,
            None,
            &[],
            &prs,
            no_pr_url,
        );
        assert!(result.contains("\n\nThis issue has GitHub PRs:\n- #1\n- #2\n- #3"));
        assert!(!result.contains("github.com"));
    }

    #[test]
    fn build_prompt_pr_url_resolved_per_link() {
        // Resolver returns Some for #1, None for #2 — exercise both branches in one prompt.
        let prs = [pr(1), pr(2)];
        let resolver = |n: u32| {
            if n == 1 {
                Some("https://github.com/test/repo/pull/1".to_string())
            } else {
                None
            }
        };
        let result = build_prompt("bork-1", "Mixed", TEST_DEFAULT, None, &[], &prs, resolver);
        assert!(result.contains("- #1 - https://github.com/test/repo/pull/1"));
        assert!(result.contains("- #2\n") || result.ends_with("- #2"));
    }

    #[test]
    fn build_prompt_with_both_linear_and_pr() {
        let links = [linear("VIL-123", "https://linear.app/team/issue/VIL-123")];
        let prs = [pr(7)];
        let result = build_prompt(
            "vil-123",
            "Fix auth",
            TEST_DEFAULT,
            None,
            &links,
            &prs,
            test_pr_url,
        );
        assert_eq!(
            result,
            "You are working on vil-123: Fix auth. The source code is in main/.\n\nThis issue has a Linear ticket: VIL-123 - https://linear.app/team/issue/VIL-123\n\nThis issue has a GitHub PR: #7 - https://github.com/test/repo/pull/7"
        );
    }

    #[test]
    fn build_prompt_ordering_with_user_prompt() {
        let links = [linear("VIL-123", "https://linear.app/team/issue/VIL-123")];
        let prs = [pr(7)];
        let result = build_prompt(
            "vil-123",
            "Fix auth",
            TEST_DEFAULT,
            Some("Focus on OAuth"),
            &links,
            &prs,
            test_pr_url,
        );
        assert!(result.contains("The source code is in main/."));
        assert!(result.contains(
            "\n\nThis issue has a Linear ticket: VIL-123 - https://linear.app/team/issue/VIL-123"
        ));
        assert!(result
            .contains("\n\nThis issue has a GitHub PR: #7 - https://github.com/test/repo/pull/7"));
        assert!(result.ends_with("\n\nFocus on OAuth"));
    }

    #[test]
    fn build_prompt_without_links_has_no_integration_lines() {
        let result = build_prompt(
            "bork-7",
            "Add feature",
            TEST_DEFAULT,
            None,
            &[],
            &[],
            no_pr_url,
        );
        assert!(!result.contains("Linear"));
        assert!(!result.contains("GitHub PR"));
    }

    fn test_issue(agent_kind: AgentKind, agent_mode: AgentMode) -> Issue {
        Issue {
            agent_mode,
            ..Issue::new(
                "bork-1",
                "Fix bug",
                crate::types::Column::InProgress,
                agent_kind,
            )
        }
    }

    /// `test_issue` with a session stored for its own agent, ready to resume.
    fn resumable_issue(agent_kind: AgentKind, agent_mode: AgentMode, sid: &str) -> Issue {
        let mut issue = test_issue(agent_kind, agent_mode);
        issue.sessions.insert(agent_kind, sid.to_string());
        issue
    }

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".to_string(),
            project_root: std::path::PathBuf::from("/tmp/test"),
            agent_kind: AgentKind::OpenCode,
            agent_mode: AgentMode::Plan,
            default_prompt: Some("The source code is in main/.".to_string()),
            review_prompt: None,
            orchestrator_prompt: None,
            setup_script: None,
            teardown_script: None,
            done_session_ttl: 300,
            debug: false,
            auto_import_reviews: true,
            auto_import_authored_prs: true,
            agents_allowlist: None,
            prune_threshold: crate::config::DEFAULT_PRUNE_THRESHOLD,
            auto_prune_check_interval: crate::config::DEFAULT_AUTO_PRUNE_CHECK_INTERVAL,
            agent_launch: std::collections::HashMap::new(),
        }
    }

    /// Test wrapper: invoke build_agent_cmd with a fixed prompt-file path and
    /// return (cmd, pre_assigned_sid, prompt_contents). The prompt-file path is
    /// fixed so tests can also assert on the `"$(cat ...)"` substitution.
    const TEST_PROMPT_PATH: &str = "/tmp/status/prompt-bork-bork-1.txt";

    fn agent_cmd(
        issue: &Issue,
        config: &AppConfig,
        session: &str,
        status_dir: &str,
    ) -> (String, Option<String>, Option<String>) {
        build_agent_cmd(issue, config, session, status_dir, TEST_PROMPT_PATH)
    }

    // --- build_agent_cmd ---

    #[test]
    fn opencode_fresh_plan() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt \"$(cat '/tmp/status/prompt-bork-bork-1.txt')\""));
        assert!(cmd.contains("rm -f '/tmp/status/prompt-bork-bork-1.txt'"));
        assert!(cmd.contains("--agent plan"));
        assert!(cmd.contains("BORK_SESSION='bork-bork-1'"));
        assert!(cmd.contains("BORK_STATUS_DIR='/tmp/status'"));
        assert!(sid.is_none());
    }

    #[test]
    fn opencode_fresh_build() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt"));
        assert!(!cmd.contains("--agent plan"));
    }

    #[test]
    fn fresh_prompt_includes_project_name() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(prompt.unwrap().contains("Bork project: bork"));
    }

    #[test]
    fn fresh_prompt_includes_assigned_worktree() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.worktree = Some("bork-1-fix-bug".to_string());
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        let prompt = prompt.unwrap();
        assert!(prompt.contains("Assigned worktree: bork-1-fix-bug"));
        assert!(prompt.contains("inside that directory"));
    }

    #[test]
    fn orchestrator_prompt_uses_orchestrator_template() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.kind = crate::types::IssueKind::Orchestrator;
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        let prompt = prompt.unwrap();
        assert!(prompt.contains("orchestrator agent"));
        assert!(prompt.contains("bork issue start"));
        assert!(prompt.contains("plans/bork-1/planning.md"));
        assert!(!prompt.contains("Assigned worktree"));
        // The regular default prompt is not used for orchestrators.
        assert!(!prompt.contains("The source code is in main/."));
    }

    #[test]
    fn orchestrator_prompt_respects_config_override() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.kind = crate::types::IssueKind::Orchestrator;
        let mut config = test_config();
        config.orchestrator_prompt = Some("Custom orchestration rules".to_string());
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        let prompt = prompt.unwrap();
        assert!(prompt.contains("Custom orchestration rules"));
        assert!(prompt.contains("plans/bork-1/planning.md"));
    }

    #[test]
    fn agentic_prompt_does_not_use_orchestrator_template() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        let prompt = prompt.unwrap();
        assert!(prompt.contains("The source code is in main/."));
        assert!(!prompt.contains("planning.md"));
    }

    #[test]
    fn opencode_fresh_yolo_treated_as_build() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --prompt"));
        assert!(!cmd.contains("--agent plan"));
        assert!(!cmd.contains("yolo"));
    }

    #[test]
    fn opencode_resume() {
        let issue = resumable_issue(AgentKind::OpenCode, AgentMode::Plan, "ses_abc123");
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123'"));
        assert!(cmd.contains("--agent plan"));
        assert!(!cmd.contains("--prompt"));
        assert!(sid.is_none());
    }

    #[test]
    fn opencode_resume_build() {
        let issue = resumable_issue(AgentKind::OpenCode, AgentMode::Build, "ses_abc123");
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123'"));
        assert!(!cmd.contains("--agent plan"));
    }

    #[test]
    fn claude_fresh_plan() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("claude --name"));
        assert!(cmd.contains("--permission-mode plan"));
        assert!(!cmd.contains("--resume"));
        // Fresh Claude gets a pre-assigned session ID (uuid) only if uuidgen works,
        // but in tests it may or may not be available, so we check the command structure
        if let Some(ref id) = sid {
            assert!(cmd.contains("--session-id"));
            assert!(!id.is_empty());
        }
    }

    #[test]
    fn claude_fresh_build() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("claude --name"));
        assert!(!cmd.contains("--permission-mode plan"));
        assert!(!cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn claude_fresh_yolo() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--dangerously-skip-permissions"));
        assert!(!cmd.contains("--permission-mode plan"));
    }

    #[test]
    fn claude_resume() {
        let issue = resumable_issue(AgentKind::Claude, AgentMode::Plan, "uuid-123-456");
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("claude --name"));
        assert!(cmd.contains("--resume 'uuid-123-456'"));
        assert!(cmd.contains("--permission-mode plan"));
        assert!(!cmd.contains("--prompt"));
        assert_eq!(sid, Some("uuid-123-456".to_string()));
    }

    #[test]
    fn claude_resume_yolo() {
        let issue = resumable_issue(AgentKind::Claude, AgentMode::Yolo, "uuid-789");
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--resume 'uuid-789'"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn codex_fresh_plan() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Plan);
        let config = test_config();
        let (cmd, sid, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --sandbox workspace-write --ask-for-approval on-request"));
        assert!(prompt
            .unwrap()
            .contains("You are working on bork-1: Fix bug"));
        assert!(sid.is_none());
    }

    #[test]
    fn codex_fresh_build_uses_workspace_write_never() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --sandbox workspace-write --ask-for-approval never"));
        assert!(!cmd.contains("--full-auto"));
        assert!(!cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn codex_fresh_yolo() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(!cmd.contains("--full-auto"));
        assert!(!cmd.contains("workspace-write"));
    }

    #[test]
    fn codex_resume_uses_session_id() {
        let issue = resumable_issue(
            AgentKind::Codex,
            AgentMode::Build,
            "019d76ad-9734-77c0-8169-a727a5524013",
        );
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains(
            "codex resume '019d76ad-9734-77c0-8169-a727a5524013' --sandbox workspace-write --ask-for-approval never"
        ));
        assert!(!cmd.contains("--prompt"));
        assert_eq!(
            sid,
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn fresh_prompt_staged_to_file_not_inlined() {
        // A multiline prompt with characters that break naive shell quoting must
        // be staged verbatim to the file, while the command references it via
        // `"$(cat ...)"` and never inlines the prompt text.
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.prompt = Some("line one\nline `two`\n$VAR and \"quotes\"".to_string());
        let config = test_config();
        let (cmd, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");

        let prompt = prompt.expect("fresh session should stage a prompt file");
        assert!(prompt.contains("line one\nline `two`\n$VAR and \"quotes\""));

        // Command must read the file, not embed the multiline text.
        assert!(cmd.contains("\"$(cat '/tmp/status/prompt-bork-bork-1.txt')\""));
        assert!(!cmd.contains("line `two`"));
        // No literal newlines in the typed command line (send-keys would mangle them).
        assert!(!cmd.contains('\n'));
    }

    #[test]
    fn prompt_file_path_is_scoped_to_session() {
        let path = prompt_file_path(Path::new("/tmp/status"), "bork-bork-7");
        assert_eq!(path, PathBuf::from("/tmp/status/prompt-bork-bork-7.txt"));
    }

    #[test]
    fn pi_fresh_uses_name_and_prompt() {
        let issue = test_issue(AgentKind::Pi, AgentMode::Build);
        let config = test_config();
        let (cmd, sid, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("pi --name 'bork-1: Fix bug'"));
        assert!(prompt
            .unwrap()
            .contains("You are working on bork-1: Fix bug"));
        // Pi has no built-in mode flags.
        assert!(!cmd.contains("--agent plan"));
        assert!(!cmd.contains("--permission-mode"));
        assert!(!cmd.contains("--sandbox"));
        assert!(sid.is_none());
    }

    #[test]
    fn pi_single_mode_ignores_agent_mode() {
        // Pi behaves identically regardless of the stored agent_mode.
        let config = test_config();
        let plan = agent_cmd(
            &test_issue(AgentKind::Pi, AgentMode::Plan),
            &config,
            "bork-bork-1",
            "/tmp/status",
        )
        .0;
        let build = agent_cmd(
            &test_issue(AgentKind::Pi, AgentMode::Build),
            &config,
            "bork-bork-1",
            "/tmp/status",
        )
        .0;
        assert_eq!(plan, build);
    }

    #[test]
    fn pi_resume_uses_session_id() {
        let issue = resumable_issue(
            AgentKind::Pi,
            AgentMode::Build,
            "019d76ad-9734-77c0-8169-a727a5524013",
        );
        let config = test_config();
        let (cmd, sid, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("pi --session '019d76ad-9734-77c0-8169-a727a5524013'"));
        assert!(!cmd.contains("--prompt"));
        assert!(!cmd.contains("--name"));
        assert_eq!(
            sid,
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn pi_session_id_parsed_from_filename() {
        assert_eq!(
            parse_pi_session_id_from_filename(
                "2024-12-03T14-00-00_019d76ad-9734-77c0-8169-a727a5524013.jsonl"
            ),
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
        assert_eq!(parse_pi_session_id_from_filename("not-a-session.txt"), None);
        assert_eq!(parse_pi_session_id_from_filename("123_short.jsonl"), None);
    }

    // --- per-agent sessions ---

    #[test]
    fn agent_switch_starts_fresh_ignoring_other_agents_sessions() {
        // A session minted by opencode must never reach claude's resume flag
        // (the historical bork-147 bug: `claude --resume ses_xxx`).
        let mut issue = test_issue(AgentKind::Claude, AgentMode::Build);
        issue
            .sessions
            .insert(AgentKind::OpenCode, "ses_abc123".to_string());
        let config = test_config();
        let (cmd, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("--resume"));
        assert!(!cmd.contains("ses_abc123"));
        // Fresh launch for the new agent carries the full prompt.
        assert!(prompt.is_some());
    }

    #[test]
    fn agent_switch_back_resumes_own_session() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue
            .sessions
            .insert(AgentKind::OpenCode, "ses_abc123".to_string());
        issue
            .sessions
            .insert(AgentKind::Claude, "uuid-123-456".to_string());
        let config = test_config();
        let (cmd, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123'"));
        assert!(!cmd.contains("uuid-123-456"));
        assert!(prompt.is_none());
    }

    // --- setup_prefix ---

    #[test]
    fn setup_prefix_for_fresh_session_with_worktree() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.worktree = Some("bork-1-fix-bug".to_string());
        let mut config = test_config();
        config.setup_script = Some("npm install".to_string());
        assert_eq!(
            setup_prefix(&issue, &config),
            Some("(cd 'bork-1-fix-bug' && npm install)".to_string())
        );
    }

    #[test]
    fn setup_prefix_skipped_after_setup_ran() {
        // Setup ran on the first launch in this worktree; resumes and agent
        // switches within the same worktree must not re-run it.
        let mut issue = resumable_issue(AgentKind::OpenCode, AgentMode::Build, "ses_abc123");
        issue.worktree = Some("bork-1-fix-bug".to_string());
        issue.setup_ran = true;
        let mut config = test_config();
        config.setup_script = Some("npm install".to_string());
        assert_eq!(setup_prefix(&issue, &config), None);
    }

    #[test]
    fn setup_prefix_runs_in_fresh_worktree_despite_recorded_sessions() {
        // Recorded session ids outlive the worktree they were created in:
        // after a prune + re-attach (attach_worktree clears setup_ran), the
        // fresh checkout needs the setup script even though sessions exist.
        let mut issue = resumable_issue(AgentKind::OpenCode, AgentMode::Build, "ses_abc123");
        issue.attach_worktree("bork-1-fix-bug".to_string());
        let mut config = test_config();
        config.setup_script = Some("npm install".to_string());
        assert_eq!(
            setup_prefix(&issue, &config),
            Some("(cd 'bork-1-fix-bug' && npm install)".to_string())
        );
    }

    #[test]
    fn setup_prefix_skipped_without_worktree() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let mut config = test_config();
        config.setup_script = Some("npm install".to_string());
        assert_eq!(setup_prefix(&issue, &config), None);
    }

    #[test]
    fn setup_prefix_skipped_for_orchestrator() {
        // Orchestrators never have a worktree (set_kind clears it,
        // auto_assign and `bork worktree` skip them), so setup_script
        // must not run for their launches.
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.kind = crate::types::IssueKind::Orchestrator;
        issue.worktree = None;
        let mut config = test_config();
        config.setup_script = Some("npm install".to_string());
        assert_eq!(setup_prefix(&issue, &config), None);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("npm install"));
    }

    #[test]
    fn setup_prefix_skipped_without_config() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.worktree = Some("bork-1-fix-bug".to_string());
        let config = test_config();
        assert_eq!(setup_prefix(&issue, &config), None);
    }

    #[test]
    fn setup_prefix_escapes_worktree_dir() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.worktree = Some("it's-a-dir".to_string());
        let mut config = test_config();
        config.setup_script = Some("bin/setup".to_string());
        assert_eq!(
            setup_prefix(&issue, &config),
            Some("(cd 'it'\\''s-a-dir' && bin/setup)".to_string())
        );
    }

    #[test]
    fn cmd_env_prefix() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "my-session", "/path/to/status");
        assert!(
            cmd.starts_with("export BORK_SESSION='my-session' BORK_STATUS_DIR='/path/to/status'")
        );
    }

    fn config_with_launch(
        kind: AgentKind,
        base: &[&str],
        mode_overrides: &[(AgentMode, &[&str])],
    ) -> AppConfig {
        let mut config = test_config();
        let launch = crate::config::AgentLaunchConfig {
            args: base.iter().map(|s| s.to_string()).collect(),
            mode_args: mode_overrides
                .iter()
                .map(|(m, args)| (*m, args.iter().map(|s| s.to_string()).collect()))
                .collect(),
        };
        config.agent_launch.insert(kind, launch);
        config
    }

    #[test]
    fn cmd_escapes_single_quotes_in_session_name() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "it's-a-test", "/tmp/status");
        assert!(cmd.contains("BORK_SESSION='it'\\''s-a-test'"));
    }

    // --- agent_launch / trailing_args integration ---

    #[test]
    fn claude_base_args_are_appended() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = config_with_launch(AgentKind::Claude, &["--verbose"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains(" '--verbose' "));
    }

    #[test]
    fn claude_mode_override_replaces_builtin_flag() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        let config = config_with_launch(
            AgentKind::Claude,
            &[],
            &[(AgentMode::Plan, &["--dangerously-skip-permissions"])],
        );
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("'--dangerously-skip-permissions'"));
        assert!(!cmd.contains("--permission-mode plan"));
    }

    #[test]
    fn claude_empty_mode_override_clears_builtin_flag() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Plan);
        let config = config_with_launch(AgentKind::Claude, &[], &[(AgentMode::Plan, &[])]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("--permission-mode plan"));
    }

    #[test]
    fn opencode_extra_args_appended_on_resume() {
        let issue = resumable_issue(AgentKind::OpenCode, AgentMode::Build, "ses_abc123");
        let config = config_with_launch(AgentKind::OpenCode, &["--quiet"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123' '--quiet'"));
    }

    #[test]
    fn codex_mode_override_replaces_sandbox_flags() {
        let issue = test_issue(AgentKind::Codex, AgentMode::Build);
        let config = config_with_launch(
            AgentKind::Codex,
            &[],
            &[(AgentMode::Build, &["--sandbox", "read-only"])],
        );
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("'--sandbox' 'read-only'"));
        assert!(!cmd.contains("workspace-write"));
        assert!(!cmd.contains("--ask-for-approval never"));
    }

    #[test]
    fn launcher_does_not_affect_other_agents() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = config_with_launch(AgentKind::OpenCode, &["--quiet"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("--quiet"));
    }

    #[test]
    fn configured_args_are_individually_shell_escaped() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = config_with_launch(AgentKind::Claude, &["it's a flag"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("'it'\\''s a flag'"));
    }

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape_single_quotes("hello world"), "hello world");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape_single_quotes("it's a test"), "it'\\''s a test");
    }

    #[test]
    fn parse_all_session_ids_finds_every_ses_entry() {
        let output = "ses_abc123   My session title   2024-01-15\nses_def456   Another session   2024-01-14\n";
        let ids = parse_all_session_ids(output);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("ses_abc123"));
        assert!(ids.contains("ses_def456"));
    }

    #[test]
    fn parse_all_session_ids_empty_for_empty_output() {
        assert!(parse_all_session_ids("").is_empty());
    }

    #[test]
    fn parse_all_session_ids_ignores_non_ses_lines() {
        let output = "No sessions found\n";
        assert!(parse_all_session_ids(output).is_empty());
    }

    // --- legacy session attribution ---

    const LEGACY_UUID: &str = "019d76ad-9734-77c0-8169-a727a5524013";

    /// Attribution of LEGACY_UUID with each store either holding it or empty.
    fn owner_of(current: AgentKind, in_codex: bool, in_pi: bool) -> Option<AgentKind> {
        let store = |present: bool| -> HashSet<String> {
            if present {
                HashSet::from([LEGACY_UUID.to_string()])
            } else {
                HashSet::new()
            }
        };
        attribute_uuid_against_stores(LEGACY_UUID, current, &store(in_codex), &store(in_pi))
    }

    #[test]
    fn legacy_uuid_attribution_follows_stores_then_claude() {
        // The pre-map bug could leave another agent's id under any current
        // agent; the transcript stores decide, claude is the only fallback.
        assert_eq!(
            owner_of(AgentKind::Claude, true, false),
            Some(AgentKind::Codex)
        );
        assert_eq!(owner_of(AgentKind::Codex, false, true), Some(AgentKind::Pi));
        assert_eq!(
            owner_of(AgentKind::Claude, false, false),
            Some(AgentKind::Claude)
        );
        for current in [AgentKind::OpenCode, AgentKind::Codex, AgentKind::Pi] {
            assert_eq!(
                owner_of(current, false, false),
                None,
                "unattributable id must drop under {current}"
            );
        }
    }

    #[test]
    fn legacy_attribution_shape_checks_precede_store_lookups() {
        // ses_ ids and non-uuid garbage resolve without touching the stores
        // (a nonexistent project root would otherwise collect empty sets).
        let mut stores = LegacySessionStores::new(Path::new("/nonexistent"));
        assert_eq!(
            stores.attribute("ses_abc123", AgentKind::Claude),
            Some(AgentKind::OpenCode)
        );
        assert_eq!(stores.attribute("not-a-uuid", AgentKind::Claude), None);
    }

    #[test]
    fn parse_codex_session_id_from_filename_extracts_uuid() {
        let file_name = "rollout-2026-04-10T11-16-21-019d76ad-9734-77c0-8169-a727a5524013.jsonl";
        assert_eq!(
            parse_codex_session_id_from_filename(file_name),
            Some("019d76ad-9734-77c0-8169-a727a5524013".to_string())
        );
    }

    #[test]
    fn parse_codex_session_id_from_filename_rejects_invalid() {
        let file_name = "rollout-2026-04-10T11-16-21-not-a-uuid.jsonl";
        assert_eq!(parse_codex_session_id_from_filename(file_name), None);
    }

    #[test]
    fn is_uuid_like_validates_expected_shape() {
        assert!(is_uuid_like("019d76ad-9734-77c0-8169-a727a5524013"));
        assert!(!is_uuid_like("019d76ad973477c08169a727a5524013"));
        assert!(!is_uuid_like("not-a-uuid"));
    }
}
