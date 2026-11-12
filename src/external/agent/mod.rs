use std::collections::HashSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::{self, AppConfig};
use crate::error::AppError;
use crate::external::process::{self, BORK_ISSUE_ID_VAR, BORK_SESSION_VAR};
use crate::external::{github, tmux};
use crate::types::{
    AgentKind, AgentMode, Issue, IssueKind, LinkedGithubPr, LinkedLinear, PrImportSource,
};

mod claude;
mod codex;
mod cursor;
mod handoff;
mod opencode;
mod pi;

/// Per-provider surface behind the agent registry. One implementor per
/// harness; `provider()` maps `AgentKind` to the matching `&'static dyn`.
///
/// Trait objects (not an enum of structs) keep the dispatch a single
/// compiler-checked match while letting `AgentKind::ALL` iterate providers for
/// help text, the yolo capability, and hooks. The surface is uniform across
/// harnesses with no generics or lifetimes, so trait objects stay boring here.
pub trait AgentProvider {
    /// The exec / PATH-probe name (e.g. Cursor is `cursor-agent`).
    fn binary(&self) -> &'static str;

    /// Display / config token (equals `binary()` for every current harness).
    fn display_name(&self) -> &'static str;

    /// Accepted `AgentKind::parse` aliases (lowercased).
    fn parse_aliases(&self) -> &'static [&'static str];

    /// Built-in mode flag for `mode`, replaced by any `[agent.<name>.mode.<m>]`
    /// override. Empty string means "no built-in flag".
    fn mode_flag(&self, mode: AgentMode) -> &'static str;

    /// Whether this agent exposes bork-managed plan/build/yolo modes.
    fn has_modes(&self) -> bool;

    /// Whether the dialog offers a yolo pip / the mode cycle includes Yolo.
    fn supports_yolo(&self) -> bool;

    /// Whether the launch command takes `--model <name>`, on fresh launches
    /// and resumes alike.
    fn supports_model(&self) -> bool {
        true
    }

    /// Build the launch/resume command. Returns
    /// (command, pre_assigned_session_id, prompt_contents), matching what
    /// `build_agent_cmd` historically returned.
    fn build_cmd(&self, ctx: &LaunchContext) -> (String, Option<String>, Option<String>);

    /// Capture existing IDs before sending a fresh launch to tmux.
    fn snapshot_session_ids(&self, _project_root: &Path) -> HashSet<String> {
        HashSet::new()
    }

    /// Detect the session id created by a fresh launch, if this harness needs
    /// post-launch detection (OpenCode/Codex/Pi). Claude pre-assigns instead.
    fn detect_session_id(&self, _ctx: &DetectContext) -> Option<String> {
        None
    }

    /// Whether the agent can still resume `session_id`. Harnesses that can't
    /// tell (or whose storage can't be read) return `true` so the stored
    /// session is kept.
    fn has_session(&self, _project_root: &Path, _session_id: &str) -> bool {
        true
    }

    /// Install hooks (plugin/extension/settings) for this harness. Providers
    /// with no status plumbing return `Ok(())` without touching disk.
    fn install_hooks(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Remove hooks for this harness.
    fn uninstall_hooks(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Everything a provider needs to assemble its launch/resume command, computed
/// once by `build_agent_cmd`, including the cwd for pre-launch session creation.
pub struct LaunchContext<'a> {
    pub issue: &'a Issue,
    /// The tmux launch cwd, also used when minting workspace-scoped chat ids.
    pub project_root: &'a Path,
    /// `export BORK_SESSION=... BORK_STATUS_DIR=... BORK_ISSUE_ID=...`
    pub env_prefix: &'a str,
    /// Trailing args (leading space when non-empty).
    pub trailing: &'a str,
    /// `"$(cat '<prompt-path>')"` — expands to the staged prompt at runtime.
    pub prompt_subst: &'a str,
    /// `; rm -f '<prompt-path>'` — removes the staged file after launch.
    pub prompt_cleanup: &'a str,
    /// The current agent's stored session id, if resuming.
    pub current_session: Option<&'a str>,
    /// Lazily builds the full issue prompt (only for fresh sessions).
    build_prompt: &'a dyn Fn() -> String,
}

impl LaunchContext<'_> {
    pub fn build_prompt(&self) -> String {
        (self.build_prompt)()
    }
}

/// Context for post-launch session-id detection.
pub struct DetectContext<'a> {
    pub project_root: &'a Path,
    /// Session ids visible before launch, excluded from post-launch detection.
    pub before: &'a HashSet<String>,
}

/// Whether any direct subdir of `root` contains `name`. For harnesses that
/// group sessions by a workspace key bork doesn't derive itself. `None` when
/// any part of the search can't be read and `name` wasn't found, so callers
/// keep the session.
fn exists_in_any_subdir(root: &Path, name: &str) -> Option<bool> {
    let mut unreadable = false;
    for entry in fs::read_dir(root).ok()? {
        let Ok(entry) = entry else {
            unreadable = true;
            continue;
        };
        // Skip stray files like `.DS_Store`: probing below them fails with
        // NotADirectory, which would read as unreadable storage.
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match path.join(name).try_exists() {
            Ok(true) => return Some(true),
            Ok(false) => {}
            Err(_) => unreadable = true,
        }
    }
    (!unreadable).then_some(false)
}

/// Map an `AgentKind` to its provider. The one dispatch point — a match here is
/// compiler-checked, so a new `AgentKind` variant fails to compile until wired.
pub fn provider(kind: AgentKind) -> &'static dyn AgentProvider {
    match kind {
        AgentKind::OpenCode => &opencode::OpenCode,
        AgentKind::Claude => &claude::Claude,
        AgentKind::Codex => &codex::Codex,
        AgentKind::Pi => &pi::Pi,
        AgentKind::Cursor => &cursor::Cursor,
    }
}

/// The exec / PATH-probe name for `kind` (e.g. Cursor's `cursor-agent`).
pub fn binary(kind: AgentKind) -> &'static str {
    provider(kind).binary()
}

/// Display / config token for `kind`.
pub fn display_name(kind: AgentKind) -> &'static str {
    provider(kind).display_name()
}

/// Accepted `AgentKind::parse` aliases for `kind`.
pub fn parse_aliases(kind: AgentKind) -> &'static [&'static str] {
    provider(kind).parse_aliases()
}

/// Whether `kind` exposes bork-managed plan/build/yolo modes.
pub fn has_modes(kind: AgentKind) -> bool {
    provider(kind).has_modes()
}

/// Comma-separated display names of every registered agent, in `AgentKind::ALL`
/// order. Single source of truth for CLI help / error text so a new harness
/// can't leave a stale hardcoded list behind.
pub fn display_names_csv() -> String {
    AgentKind::ALL
        .iter()
        .map(|kind| display_name(*kind))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the given agent offers a yolo mode (drives the dialog yolo pip and
/// the mode-cycle behaviour).
pub fn supports_yolo(kind: AgentKind) -> bool {
    provider(kind).supports_yolo()
}

/// Whether `kind` accepts a per-issue `--model`.
pub fn supports_model(kind: AgentKind) -> bool {
    provider(kind).supports_model()
}

pub struct LaunchOutcome {
    pub session_name: String,
    /// The agent's own session id, used to resume the conversation later:
    ///   - Claude: UUID pre-assigned via --session-id
    ///   - OpenCode: ses_xxx detected by polling `opencode session list` after launch
    ///   - Codex: UUID detected from newest ~/.codex/sessions rollout transcript
    pub agent_session_id: Option<String>,
    /// The launch included the one-time worktree setup script.
    pub setup_ran: bool,
    /// The stored session was gone, so this launch started a new one.
    pub replaced_lost_session: bool,
}

/// Launch an agent session for an issue.
/// Creates a tmux session with two windows:
///   1. The agent launched at the project root with issue context
///   2. A bare terminal
///
/// The pane only gets a short `bork issue launch <id>` line; that command
/// (`exec_launch`) runs setup, builds the agent command and execs it. The
/// issue is staged for it in a handoff dir because this copy may hold edits
/// the TUI hasn't flushed to state.json yet. Session-id detection stays here
/// so bulk launches, which run one after another, can't pick up each
/// other's ids.
pub fn launch_session(issue: &Issue, config: &AppConfig) -> Result<LaunchOutcome, AppError> {
    let session_name = issue.session_name(&config.project_name);
    let cwd = &config.project_root;

    if tmux::session_exists(&session_name) {
        return Ok(LaunchOutcome {
            session_name,
            agent_session_id: issue.current_session_id().map(str::to_string),
            setup_ran: false,
            replaced_lost_session: false,
        });
    }

    let status_dir = config::agent_status_dir(&config.project_root);
    let handoff = handoff::Handoff::prepare(&status_dir, issue)?;
    let launch_cmd = launch_command(&issue.id)?;

    // Snapshot resumes too: the launch command starts fresh when the stored
    // session turns out to be gone.
    let provider = provider(issue.agent_kind);
    let before = provider.snapshot_session_ids(&config.project_root);

    tmux::create_session(&session_name, cwd)?;
    let started = start_in_session(&session_name, cwd, &launch_cmd, &handoff);
    let (session_id, setup_ran, replaced_lost_session) = match started {
        Ok(started) => started,
        Err(err) => {
            let pane_output = tmux::capture_pane_history(&session_name, FAILED_PANE_LINES);
            return Err(abandon_launch(
                &config.project_root,
                &issue.id,
                &session_name,
                err,
                &pane_output,
            ));
        }
    };

    // For OpenCode/Codex/Pi, detect session IDs after launch.
    let agent_session_id = session_id.or_else(|| {
        provider.detect_session_id(&DetectContext {
            project_root: &config.project_root,
            before: &before,
        })
    });

    Ok(LaunchOutcome {
        session_name,
        agent_session_id,
        setup_ran,
        replaced_lost_session,
    })
}

/// How much of a failed launch's pane goes into `.bork/errors.log`.
const FAILED_PANE_LINES: usize = 200;

/// Type the launch command into the new session, add the terminal window and
/// wait for the command's report. Returns `(session_id, setup_ran,
/// replaced_lost_session)` once the agent is about to start.
fn start_in_session(
    session_name: &str,
    cwd: &Path,
    launch_cmd: &str,
    handoff: &handoff::Handoff,
) -> Result<(Option<String>, bool, bool), AppError> {
    tmux::send_keys(session_name, launch_cmd)?;

    // Second window: bare terminal for ad-hoc commands
    tmux::create_window(session_name, "terminal", cwd)?;

    match handoff.wait(|| tmux::is_pane_alive(session_name))? {
        handoff::Report::Started {
            session_id,
            setup_ran,
            replaced_lost_session,
        } => Ok((session_id, setup_ran, replaced_lost_session)),
        handoff::Report::Failed { error } => Err(AppError::Launch(error)),
    }
}

/// Clean up after a launch that failed once its tmux session existed. The
/// pane is left at a plain shell, and `launch_session` would only attach to
/// it next time, so kill the session to let the next launch start clean.
/// What the pane showed (e.g. setup script errors) goes to `.bork/errors.log`
/// first, since killing the session throws it away.
fn abandon_launch(
    project_root: &Path,
    issue_id: &str,
    session_name: &str,
    err: AppError,
    pane_output: &str,
) -> AppError {
    config::append_error_log(
        project_root,
        &failed_launch_log(issue_id, &err, pane_output),
    );
    match terminate_session(project_root, session_name) {
        Ok(_) => AppError::Launch(format!("{err} (pane output in .bork/errors.log)")),
        Err(kill_err) => AppError::Launch(format!(
            "{err} (and its tmux session '{session_name}' could not be closed: {kill_err}, close it before relaunching)"
        )),
    }
}

fn failed_launch_log(issue_id: &str, err: &AppError, pane_output: &str) -> Vec<String> {
    let mut lines = vec![format!("Failed to launch {issue_id}: {err}")];
    if pane_output.trim().is_empty() {
        lines.push("  (agent pane was empty)".to_string());
        return lines;
    }
    lines.push("  agent pane output:".to_string());
    lines.extend(pane_output.lines().map(|line| format!("  | {line}")));
    lines
}

/// The line typed into the agent pane. Uses this binary's own path so PATH
/// and a `bork update` mid-session can't run a different bork.
fn launch_command(issue_id: &str) -> Result<String, AppError> {
    let exe = std::env::current_exe()?;
    Ok(format!(
        "'{}' issue launch '{}'",
        shell_escape_single_quotes(&exe.to_string_lossy()),
        shell_escape_single_quotes(issue_id)
    ))
}

/// What a launch did before exec'ing the agent.
#[derive(Debug)]
struct Prepared {
    agent_cmd: String,
    session_id: Option<String>,
    setup_ran: bool,
    replaced_lost_session: bool,
}

/// Body of `bork issue launch <id>`, run inside the agent pane: run the
/// worktree setup script if it's due, build the agent command and exec it.
/// Reports back to a waiting `launch_session`; when run by hand, records the
/// result in state.json itself. Only returns on failure.
pub fn exec_launch(project_root: &Path, issue_id: &str) -> anyhow::Result<()> {
    let config = config::load_config_from(project_root);
    let status_dir = config::agent_status_dir(project_root);

    let (claim, issue) = match handoff::Claim::take(&status_dir, issue_id) {
        Some((claim, issue)) => (Some(claim), issue),
        None => {
            let state = config::load_state(project_root);
            let issue = state
                .issues
                .into_iter()
                .find(|i| i.id.eq_ignore_ascii_case(issue_id))
                .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;
            (None, issue)
        }
    };

    // The agent runs through `/bin/sh -c`, so a missing binary would only
    // surface as the shell's "command not found" after reporting success.
    let prepared = match ensure_agent_installed(&issue, crate::agent_config::command_exists)
        .and_then(|()| prepare_launch(&issue, &config, &status_dir))
    {
        Ok(prepared) => prepared,
        Err(e) => {
            if let Some(claim) = &claim {
                let _ = claim.report(&handoff::Report::Failed {
                    error: e.to_string(),
                });
            }
            return Err(e.into());
        }
    };

    match &claim {
        Some(claim) => claim.report(&handoff::Report::Started {
            session_id: prepared.session_id,
            setup_ran: prepared.setup_ran,
            replaced_lost_session: prepared.replaced_lost_session,
        })?,
        None => crate::ops::record_launch(
            project_root,
            &issue,
            prepared.session_id,
            prepared.setup_ran,
            prepared.replaced_lost_session,
        )?,
    }

    let err = Command::new("/bin/sh")
        .arg("-c")
        .arg(&prepared.agent_cmd)
        .current_dir(project_root)
        .exec();
    Err(anyhow::anyhow!(
        "failed to start {}: {err}",
        binary(issue.agent_kind)
    ))
}

fn prepare_launch(
    issue: &Issue,
    config: &AppConfig,
    status_dir: &Path,
) -> Result<Prepared, AppError> {
    let session_name = issue.session_name(&config.project_name);

    let setup = setup_prefix(issue, config);
    if let Some(prefix) = &setup {
        run_setup(prefix, &config.project_root)?;
    }

    // A stored session the agent no longer has (e.g. Claude cleaned up the
    // transcript) would fail to resume, so launch fresh instead.
    let provider = provider(issue.agent_kind);
    let resume_session = issue
        .current_session_id()
        .filter(|sid| provider.has_session(&config.project_root, sid));
    let replaced_lost_session = issue.current_session_id().is_some() && resume_session.is_none();

    let status_dir_str = status_dir.to_str().unwrap_or("");
    let prompt_path = prompt_file_path(status_dir, &session_name);
    let prompt_path_str = prompt_path.to_str().unwrap_or("");

    let (agent_cmd, session_id, prompt_contents) = build_agent_cmd(
        issue,
        resume_session,
        config,
        &session_name,
        status_dir_str,
        prompt_path_str,
    );

    // The agent command reads the prompt back via `"$(cat ...)"`, which keeps
    // a multiline prompt intact as a single argument.
    if let Some(contents) = &prompt_contents {
        write_prompt_file(&prompt_path, contents)?;
    }

    Ok(Prepared {
        agent_cmd,
        session_id,
        setup_ran: setup.is_some(),
        replaced_lost_session,
    })
}

fn ensure_agent_installed(
    issue: &Issue,
    command_exists: impl Fn(&str) -> bool,
) -> Result<(), AppError> {
    let binary = binary(issue.agent_kind);
    if command_exists(binary) {
        return Ok(());
    }
    Err(AppError::Launch(format!(
        "{binary} is not installed (not found on PATH). Pick another agent with `bork issue update {} --agent <name>`",
        issue.id
    )))
}

/// Run the setup script in the user's shell with the pane's stdio, so its
/// output shows above the agent. A non-zero exit blocks the launch.
fn run_setup(prefix: &str, project_root: &Path) -> Result<(), AppError> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "sh".to_string());
    let status = Command::new(shell)
        .arg("-c")
        .arg(prefix)
        .current_dir(project_root)
        .status()?;
    setup_result(status)
}

fn setup_result(status: std::process::ExitStatus) -> Result<(), AppError> {
    if status.success() {
        return Ok(());
    }
    let reason = match status.code() {
        Some(code) => format!("script exited with status {code}"),
        None => "script was interrupted".to_string(),
    };
    Err(AppError::Setup(reason))
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
/// Claude pre-assigns fresh IDs; resume commands return the stored ID.
/// `prompt_contents` is `Some` for fresh sessions (staged to a file by the
/// caller) and `None` for resume sessions, which carry no prompt.
fn build_agent_cmd(
    issue: &Issue,
    resume_session: Option<&str>,
    config: &AppConfig,
    session_name: &str,
    status_dir_str: &str,
    prompt_path_str: &str,
) -> (String, Option<String>, Option<String>) {
    let env_prefix = format!(
        "export {BORK_SESSION_VAR}='{}' BORK_STATUS_DIR='{}' {BORK_ISSUE_ID_VAR}='{}'",
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
        let is_review = issue.primary_pr_import_source() == Some(PrImportSource::ReviewRequested);
        if issue.kind == IssueKind::Agentic && !is_review {
            prompt.push_str(&format!(
                "\n\nWhen you open a pull request for this work, link it to this issue with `bork integration attach-pr {} <pr-number>`.",
                issue.id
            ));
        }
        if issue.kind == IssueKind::Orchestrator {
            prompt.push_str(&format!(
                "\n\nMaintain your plan in plans/{}/planning.md, relative to the project root (the directory containing main/).",
                issue.id
            ));
        }
        if issue.kind == IssueKind::Orchestrator && issue.has_linear() {
            prompt.push_str(LINEAR_TICKET_UPDATES);
        }
        if issue.kind.links_linear_projects() && !issue.linear_projects.is_empty() {
            append_section(
                &mut prompt,
                &issue.linear_projects,
                "Linear project",
                "Linear projects",
                |project| format!("{} - {}", project.name, project.url),
            );
            prompt.push_str(LINEAR_PROJECT_CONTEXT);
        }
        prompt
    };

    // Shell snippet that expands to the prompt at runtime by reading the staged
    // file, plus a cleanup suffix that removes the file afterwards. Using a file
    // keeps the multiline prompt out of the command's shell quoting.
    let escaped_prompt_path = shell_escape_single_quotes(prompt_path_str);
    let prompt_subst = format!("\"$(cat '{}')\"", escaped_prompt_path);
    let prompt_cleanup = format!("; rm -f '{}'", escaped_prompt_path);

    let provider = provider(issue.agent_kind);

    // Built-in mode flags. These are replaced when the user configured
    // per-mode args under `[agent.<name>.mode.<mode>]`.
    let builtin_mode_flag = provider.mode_flag(issue.agent_mode);

    let model = issue.model.as_deref().filter(|_| provider.supports_model());
    let trailing = trailing_args(
        config,
        issue.agent_kind,
        issue.agent_mode,
        builtin_mode_flag,
        model,
    );

    let ctx = LaunchContext {
        issue,
        project_root: &config.project_root,
        env_prefix: &env_prefix,
        trailing: &trailing,
        prompt_subst: &prompt_subst,
        prompt_cleanup: &prompt_cleanup,
        current_session: resume_session,
        build_prompt: &build_prompt_contents,
    };

    provider.build_cmd(&ctx)
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
/// - The issue's `model` goes last as `--model '<m>'`, and any `--model` /
///   `-m` in the configured args is dropped (codex rejects a repeated flag).
///
/// All configured args are individually shell-escaped so values containing
/// spaces or quotes are passed through safely.
fn trailing_args(
    config: &AppConfig,
    kind: AgentKind,
    mode: AgentMode,
    builtin_mode_flag: &str,
    model: Option<&str>,
) -> String {
    let (base_args, mode_args_override) = config.launch_args_for(kind, mode);
    let configured = |args: &[String]| -> Vec<String> {
        let args = match model {
            Some(_) => strip_model_args(args),
            None => args.to_vec(),
        };
        args.iter()
            .map(|arg| format!("'{}'", shell_escape_single_quotes(arg)))
            .collect()
    };

    let mut parts: Vec<String> = Vec::new();
    match mode_args_override {
        Some(args) => parts.extend(configured(args)),
        None => {
            let trimmed = builtin_mode_flag.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    parts.extend(configured(base_args));
    if let Some(model) = model {
        parts.push(format!("--model '{}'", shell_escape_single_quotes(model)));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

/// Drop `--model <m>`, `-m <m>` and `--model=<m>` from configured args so the
/// issue's model is the only one passed.
fn strip_model_args(args: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--model" || arg == "-m" {
            iter.next();
        } else if !arg.starts_with("--model=") {
            kept.push(arg.clone());
        }
    }
    kept
}

/// Generate a UUID using the system `uuidgen` command.
pub(crate) fn generate_uuid() -> Option<String> {
    Command::new("uuidgen")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
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
                codex::sessions_root()
                    .map(|root| codex::collect_session_ids(&root))
                    .unwrap_or_default(),
                pi::sessions_dir(&self.project_root)
                    .map(|dir| pi::collect_session_ids(&dir))
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

/// Sleep 800ms for the agent to create its session, then poll ~5s for an id
/// the pre-launch snapshot lacks. Only a genuinely new id is trusted: the
/// globally newest one could be any unrelated conversation (concurrent runs,
/// or a launch that failed or was killed mid-flight), and returning None
/// just means the next launch starts fresh instead of resuming a foreign
/// session.
pub(crate) fn poll_for_new_session_id(
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

pub(crate) fn is_uuid_like(value: &str) -> bool {
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

const LINEAR_TICKET_UPDATES: &str =
    "\n\nKeep the linked Linear tickets updated as milestones complete.";

const LINEAR_PROJECT_CONTEXT: &str = "\n\nFor context on the goal, read each linked Linear project's description, documents, milestones, and tickets, using the Linear MCP or the `linear` CLI. \
If an issue you start covers one of its tickets, pass `--linear <TICKET-ID>` to `bork issue start` to link it.";

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
pub(crate) fn shell_escape_single_quotes(s: &str) -> String {
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

    // --- ensure_agent_installed ---

    #[test]
    fn launch_fails_when_the_agent_binary_is_missing() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Plan);

        let err = ensure_agent_installed(&issue, |_| false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "opencode is not installed (not found on PATH). Pick another agent with `bork issue update bork-1 --agent <name>`"
        );
    }

    #[test]
    fn launch_checks_the_provider_binary_not_the_display_name() {
        let issue = test_issue(AgentKind::Cursor, AgentMode::Plan);

        assert!(ensure_agent_installed(&issue, |cmd| cmd == "cursor-agent").is_ok());
        assert!(ensure_agent_installed(&issue, |cmd| cmd == "cursor").is_err());
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

    // --- shared helpers used across providers ---

    #[test]
    fn shell_escape_no_quotes() {
        assert_eq!(shell_escape_single_quotes("hello world"), "hello world");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape_single_quotes("it's a test"), "it'\\''s a test");
    }

    #[test]
    fn is_uuid_like_validates_expected_shape() {
        assert!(is_uuid_like("019d76ad-9734-77c0-8169-a727a5524013"));
        assert!(!is_uuid_like("019d76ad973477c08169a727a5524013"));
        assert!(!is_uuid_like("not-a-uuid"));
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

    // --- build_agent_cmd (cross-provider harness) ---

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
            stack_review_prompt: None,
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
            keep_deleted_days: crate::config::DEFAULT_KEEP_DELETED_DAYS,
            cloud_agents: false,
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
        build_agent_cmd(
            issue,
            issue.current_session_id(),
            config,
            session,
            status_dir,
            TEST_PROMPT_PATH,
        )
    }

    // --- OpenCode ---

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
    fn agentic_prompt_asks_to_link_prs() {
        let issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(prompt
            .unwrap()
            .contains("bork integration attach-pr bork-1 <pr-number>"));
    }

    #[test]
    fn review_prompt_does_not_ask_to_link_prs() {
        let mut issue = test_issue(AgentKind::OpenCode, AgentMode::Build);
        issue.github_pr_links.push(LinkedGithubPr {
            number: 42,
            imported: true,
            import_source: Some(PrImportSource::ReviewRequested),
        });
        let config = test_config();
        let (_, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(!prompt.unwrap().contains("attach-pr"));
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
        assert!(!prompt.contains("attach-pr"));
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
        assert_eq!(sid.as_deref(), Some("ses_abc123"));
    }

    #[test]
    fn opencode_resume_build() {
        let issue = resumable_issue(AgentKind::OpenCode, AgentMode::Build, "ses_abc123");
        let config = test_config();
        let (cmd, sid, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("opencode --session 'ses_abc123'"));
        assert!(!cmd.contains("--agent plan"));
        assert_eq!(sid.as_deref(), Some("ses_abc123"));
        assert!(prompt.is_none());
    }

    // --- Claude ---

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

    // --- Codex ---

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

    // --- Pi ---

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

    // --- Cursor ---
    const CURSOR_TEST_CHAT_ID: &str = "a506b8cb-b2ea-4b22-b0bb-7c449eb14606";

    #[test]
    fn cursor_fresh_uses_prompt_file() {
        let issue = test_issue(AgentKind::Cursor, AgentMode::Build);
        let config = test_config();
        let (cmd, sid, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        // Binary is cursor-agent, not cursor.
        assert!(cmd.contains("cursor-agent "));
        assert!(cmd.contains("--trust"));
        // The prompt is delivered via the staged-file substitution.
        assert!(cmd.contains("\"$(cat '/tmp/status/prompt-bork-bork-1.txt')\""));
        assert!(cmd.contains("rm -f '/tmp/status/prompt-bork-bork-1.txt'"));
        // No --name flag exists on cursor-agent.
        assert!(!cmd.contains("--name"));
        // send-keys would mangle a literal newline in the typed command line.
        assert!(!cmd.contains('\n'));
        assert!(sid.is_none());
        assert!(prompt
            .unwrap()
            .contains("You are working on bork-1: Fix bug"));
    }

    #[test]
    fn cursor_resume_omits_prompt() {
        // Resume path: history is preserved, so no prompt is re-sent.
        let issue = resumable_issue(AgentKind::Cursor, AgentMode::Build, CURSOR_TEST_CHAT_ID);
        let config = test_config();
        let (cmd, sid, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains(&format!("cursor-agent --resume '{}'", CURSOR_TEST_CHAT_ID)));
        assert!(cmd.contains("--trust"));
        // No prompt substitution on resume.
        assert!(!cmd.contains("$(cat"));
        assert_eq!(sid, Some(CURSOR_TEST_CHAT_ID.to_string()));
        assert!(prompt.is_none());
    }

    #[test]
    fn cursor_yolo_uses_force_flag() {
        let issue = test_issue(AgentKind::Cursor, AgentMode::Yolo);
        let config = test_config();
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        // Yolo renders as `--trust -f` in the trailing args.
        assert!(cmd.contains("--trust -f"));
        // Guard against a later switch to the newer-build --yolo alias: -f is
        // the stable spelling, and Yolo must not leak into Build via --trust.
        assert!(!cmd.contains("--yolo"));
        assert!(!cmd.contains("--dangerously"));
    }

    #[test]
    fn cursor_trust_on_every_mode() {
        // A fresh worktree hits the Workspace Trust gate on first run, so every
        // mode must pass --trust or the launch hangs.
        let config = test_config();
        for mode in [AgentMode::Plan, AgentMode::Build, AgentMode::Yolo] {
            let issue = test_issue(AgentKind::Cursor, mode);
            let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
            assert!(cmd.contains("--trust"), "mode {:?} missing --trust", mode);
        }
    }

    #[test]
    fn cursor_plan_enforces_mode_plan() {
        // Plan mode is enforced via `--mode plan`; Build must not carry it.
        let config = test_config();
        let (plan_cmd, _, _) = agent_cmd(
            &test_issue(AgentKind::Cursor, AgentMode::Plan),
            &config,
            "bork-bork-1",
            "/tmp/status",
        );
        let (build_cmd, _, _) = agent_cmd(
            &test_issue(AgentKind::Cursor, AgentMode::Build),
            &config,
            "bork-bork-1",
            "/tmp/status",
        );
        assert!(plan_cmd.contains("--mode plan"));
        assert!(!build_cmd.contains("--mode plan"));
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
    fn lost_session_launches_fresh() {
        let issue = resumable_issue(AgentKind::Claude, AgentMode::Build, "dead-uuid");
        let config = test_config();
        // launch_session passes no resume id when has_session says it's gone.
        let (cmd, sid, prompt) = build_agent_cmd(
            &issue,
            None,
            &config,
            "bork-bork-1",
            "/tmp/status",
            TEST_PROMPT_PATH,
        );
        assert!(!cmd.contains("--resume"));
        assert!(!cmd.contains("dead-uuid"));
        assert!(prompt.is_some());
        if let Some(id) = sid {
            assert!(cmd.contains(&format!("--session-id '{id}'")));
        }
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

    #[test]
    fn attached_session_resumes_on_next_launch() {
        let mut issue = test_issue(AgentKind::Claude, AgentMode::Build);
        assert!(issue.attach_session("uuid-attached".to_string()));
        let config = test_config();
        let (cmd, _, prompt) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--resume 'uuid-attached'"));
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

    // --- per-issue model ---

    fn issue_with_model(issue: Issue, model: &str) -> Issue {
        Issue {
            model: Some(model.to_string()),
            ..issue
        }
    }

    #[test]
    fn model_flag_on_fresh_and_resume() {
        let config = test_config();
        let cases = [
            (
                test_issue(AgentKind::Claude, AgentMode::Build),
                "claude --name",
            ),
            (
                resumable_issue(AgentKind::Claude, AgentMode::Build, "abc"),
                "--resume 'abc'",
            ),
            (test_issue(AgentKind::Codex, AgentMode::Build), "codex "),
            (
                resumable_issue(AgentKind::Codex, AgentMode::Build, "abc"),
                "codex resume 'abc'",
            ),
            (test_issue(AgentKind::Pi, AgentMode::Build), "pi --name"),
            (
                resumable_issue(AgentKind::Pi, AgentMode::Build, "abc"),
                "pi --session 'abc'",
            ),
            (
                resumable_issue(AgentKind::Cursor, AgentMode::Build, "abc"),
                "cursor-agent --resume 'abc'",
            ),
        ];
        for (issue, expected) in cases {
            let issue = issue_with_model(issue, "sonnet");
            let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
            assert!(cmd.contains(expected), "{cmd}");
            assert_eq!(cmd.matches("--model 'sonnet'").count(), 1, "{cmd}");
        }
    }

    #[test]
    fn no_model_flag_when_unset() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let (cmd, _, _) = agent_cmd(&issue, &test_config(), "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("--model"));
    }

    #[test]
    fn opencode_never_gets_model_flag() {
        let issue = issue_with_model(test_issue(AgentKind::OpenCode, AgentMode::Build), "x/y");
        let (cmd, _, _) = agent_cmd(&issue, &test_config(), "bork-bork-1", "/tmp/status");
        assert!(!cmd.contains("--model"));
    }

    #[test]
    fn issue_model_replaces_configured_model_args() {
        let issue = issue_with_model(test_issue(AgentKind::Codex, AgentMode::Build), "gpt-5");
        let config = config_with_launch(
            AgentKind::Codex,
            &["-m", "o3", "--verbose", "--model=o4"],
            &[(
                AgentMode::Build,
                &["--model", "o1", "--sandbox", "read-only"],
            )],
        );
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(
            cmd.contains("'--sandbox' 'read-only' '--verbose' --model 'gpt-5'"),
            "{cmd}"
        );
        assert!(
            !cmd.contains("o3") && !cmd.contains("o4") && !cmd.contains("o1"),
            "{cmd}"
        );
    }

    #[test]
    fn configured_model_args_kept_without_issue_model() {
        let issue = test_issue(AgentKind::Claude, AgentMode::Build);
        let config = config_with_launch(AgentKind::Claude, &["--model", "opus"], &[]);
        let (cmd, _, _) = agent_cmd(&issue, &config, "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("'--model' 'opus'"));
    }

    #[test]
    fn model_value_is_shell_escaped() {
        let issue = issue_with_model(test_issue(AgentKind::Claude, AgentMode::Build), "it's");
        let (cmd, _, _) = agent_cmd(&issue, &test_config(), "bork-bork-1", "/tmp/status");
        assert!(cmd.contains("--model 'it'\\''s'"));
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

    fn setup_config(root: &Path, script: &str) -> AppConfig {
        AppConfig {
            project_root: root.to_path_buf(),
            setup_script: Some(script.to_string()),
            ..test_config()
        }
    }

    fn issue_in_worktree(root: &Path) -> Issue {
        fs::create_dir(root.join("bork-1")).unwrap();
        Issue {
            worktree: Some("bork-1".to_string()),
            ..test_issue(AgentKind::Claude, AgentMode::Build)
        }
    }

    #[test]
    fn launch_runs_setup_in_the_worktree_then_stages_the_prompt() {
        let root = tempfile::TempDir::new().unwrap();
        let status = root.path().join("status");
        let config = setup_config(root.path(), "touch setup-done");
        let issue = issue_in_worktree(root.path());

        let prepared = prepare_launch(&issue, &config, &status).unwrap();
        assert!(prepared.setup_ran);
        assert!(root.path().join("bork-1/setup-done").exists());
        assert!(prepared.agent_cmd.contains("claude --name"));
        assert!(prompt_file_path(&status, "bork-bork-1").exists());
    }

    fn error_log(root: &Path) -> String {
        fs::read_to_string(config::error_log_path(root)).unwrap_or_default()
    }

    #[test]
    fn abandoned_launch_keeps_the_pane_output_in_the_error_log() {
        let root = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(root.path().join(".bork")).unwrap();
        let pane = "$ bork issue launch bork-1\nnpm ERR! missing script: setup\n";

        let err = abandon_launch(
            root.path(),
            "bork-1",
            "bork-bork-1",
            AppError::Launch("setup error: script exited with status 1".to_string()),
            pane,
        );

        assert_eq!(
            err.to_string(),
            "setup error: script exited with status 1 (pane output in .bork/errors.log)"
        );
        let log = error_log(root.path());
        assert!(
            log.contains("Failed to launch bork-1: setup error: script exited with status 1"),
            "{log}"
        );
        assert!(log.contains("  | npm ERR! missing script: setup"), "{log}");
    }

    #[test]
    fn abandoned_launch_with_an_empty_pane_says_so_in_the_log() {
        let root = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(root.path().join(".bork")).unwrap();

        abandon_launch(
            root.path(),
            "bork-1",
            "bork-bork-1",
            AppError::Launch("launch command did not start in the agent pane".to_string()),
            "",
        );

        let log = error_log(root.path());
        assert!(log.contains("did not start in the agent pane"), "{log}");
        assert!(log.contains("(agent pane was empty)"), "{log}");
        assert!(!log.contains("agent pane output:"), "{log}");
    }

    #[test]
    fn abandoned_launch_removes_the_session_status_files() {
        let root = tempfile::TempDir::new().unwrap();
        let status_dir = config::agent_status_dir(root.path());
        fs::create_dir_all(&status_dir).unwrap();
        let status_file = status_dir.join("bork-bork-1.json");
        fs::write(&status_file, "{}").unwrap();

        abandon_launch(
            root.path(),
            "bork-1",
            "bork-bork-1",
            AppError::Launch("claude is not installed".to_string()),
            "",
        );

        assert!(!status_file.exists());
    }

    #[test]
    fn failed_setup_blocks_the_launch() {
        let root = tempfile::TempDir::new().unwrap();
        let config = setup_config(root.path(), "exit 7");
        let issue = issue_in_worktree(root.path());

        let status = root.path().join("status");

        let err = prepare_launch(&issue, &config, &status).unwrap_err();
        assert!(err.to_string().contains("status 7"), "{err}");
        assert!(!prompt_file_path(&status, "bork-bork-1").exists());
    }

    #[test]
    fn setup_killed_by_a_signal_reports_an_interruption() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(libc::SIGINT);
        let err = setup_result(status).unwrap_err();
        assert!(err.to_string().contains("interrupted"), "{err}");
    }

    #[test]
    fn setup_skipped_once_it_ran_for_the_worktree() {
        let root = tempfile::TempDir::new().unwrap();
        let config = setup_config(root.path(), "exit 7");
        let issue = Issue {
            setup_ran: true,
            ..issue_in_worktree(root.path())
        };

        let prepared = prepare_launch(&issue, &config, &root.path().join("status")).unwrap();
        assert!(!prepared.setup_ran);
    }

    #[test]
    fn resume_reports_the_stored_session() {
        let root = tempfile::TempDir::new().unwrap();
        let config = AppConfig {
            project_root: root.path().to_path_buf(),
            ..test_config()
        };
        // OpenCode can't tell whether a session is gone, so the stored one is
        // always resumed; other agents would check this machine's storage.
        let issue = resumable_issue(AgentKind::OpenCode, AgentMode::Build, "ses_1");

        let prepared = prepare_launch(&issue, &config, &root.path().join("status")).unwrap();
        assert_eq!(prepared.session_id.as_deref(), Some("ses_1"));
        assert!(prepared.agent_cmd.contains("opencode --session 'ses_1'"));
        assert!(!prepared.replaced_lost_session);
    }

    #[test]
    fn launch_command_is_short_and_quoted() {
        let cmd = launch_command("bork-204").unwrap();
        assert!(cmd.ends_with("' issue launch 'bork-204'"), "{cmd}");
        assert!(!cmd.contains("export"));
    }
}
