//! `bork import <file>`: put projects back from a backup archive written by
//! [`super::run`].
//!
//! Per project: clone the repo into `<target>/main` (when there's no `main/`
//! yet), restore the board and container files, move each worktree plan to
//! `plans/<issue-id>/planning.md` and register the project. Worktrees are not
//! recreated; issues that had one lose the link and get reopened from their
//! pushed branches later.
//!
//! Nothing is overwritten without `force`, and even then `main/` and existing
//! worktrees are never deleted, only the restored files are replaced, so
//! re-running an import is safe.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use serde::Deserialize;

use super::archive::Staging;
use super::manifest::{
    Manifest, ProjectManifest, GLOBAL_DIR, MANIFEST_FILE, MANIFEST_VERSION, PROJECTS_DIR,
};
use super::select::{is_plain_component, ISSUES_DIR};
use super::PLAN_NAME;
use crate::config::AppState;

/// Board file inside a project's archive dir and container.
const STATE_FILE: &str = ".bork/state.json";
/// Global config file, under `_global/` in the archive.
const GLOBAL_CONFIG: &str = "config.toml";

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Only this project (manifest name or label).
    pub project: Option<String>,
    /// Restore to `<root>/<name>` instead of the recorded path. Absolute.
    pub root: Option<PathBuf>,
    /// Restore over existing targets and the global config.
    pub force: bool,
    /// Resolve and report everything, write nothing.
    pub dry_run: bool,
}

/// What happened to a project's `main/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoOutcome {
    /// Cloned (or would be, in a dry run).
    Cloned {
        url: String,
        branch: Option<String>,
    },
    /// `main/` already existed and was left alone.
    KeptExisting,
    /// The backup has no remote to clone from.
    NoCloneUrl,
    Failed {
        url: String,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct ProjectReport {
    pub name: String,
    pub label: Option<String>,
    pub target: PathBuf,
    pub repo: RepoOutcome,
    /// Whether the archive had a board to restore.
    pub board_restored: bool,
    pub issues: usize,
    /// Issues whose worktree link was cleared, with the branch they were on.
    pub cleared_worktrees: Vec<(String, Option<String>)>,
    /// Container files restored, relative to the target.
    pub files: Vec<String>,
    /// Issue ids whose plan was placed in `plans/<id>/`.
    pub plans: Vec<String>,
    /// Issue ids whose `plans/<id>/planning.md` already existed and was kept.
    pub kept_plans: Vec<String>,
    /// Files under `plans/` that already existed at the target and were kept.
    pub kept_files: Vec<String>,
    pub warnings: Vec<String>,
    /// Set when restoring this project stopped part-way; the rest of the
    /// import still ran.
    pub error: Option<String>,
}

impl ProjectReport {
    fn new(project: &ProjectManifest, target: &Path) -> Self {
        Self {
            name: project.name.clone(),
            label: project.label.clone(),
            target: target.to_path_buf(),
            repo: RepoOutcome::NoCloneUrl,
            board_restored: false,
            issues: 0,
            cleared_worktrees: Vec::new(),
            files: Vec::new(),
            plans: Vec::new(),
            kept_plans: Vec::new(),
            kept_files: Vec::new(),
            warnings: Vec::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalConfigOutcome {
    /// Nothing to restore: single-project archive, no global config in it,
    /// or `--project` was given.
    Skipped,
    Restored(PathBuf),
    Failed(String),
    /// The user already has one and `force` was off.
    KeptExisting(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Report {
    pub dry_run: bool,
    pub projects: Vec<ProjectReport>,
    pub global_config: GlobalConfigOutcome,
}

impl Report {
    /// Whether any project failed part-way.
    pub fn failed(&self) -> bool {
        self.projects.iter().any(|p| p.error.is_some())
            || matches!(self.global_config, GlobalConfigOutcome::Failed(_))
    }
}

/// Restore projects from the backup `file`.
pub fn import(file: &Path, opts: &Options) -> anyhow::Result<Report> {
    let extracted = extract(file)?;
    let dir = &extracted.0;
    let manifest = read_manifest(dir)?;
    let selected = select_projects(&manifest, opts.project.as_deref())?;
    let targets = selected
        .iter()
        .map(|p| resolve_target(p, opts.root.as_deref()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    check_targets(&targets, opts.force)?;
    check_names(&selected, &targets)?;

    let now = crate::app::unix_now();
    let mut projects = Vec::new();
    for (project, target) in selected.iter().zip(&targets) {
        let mut report = ProjectReport::new(project, target);
        // One broken project shouldn't hide what happened to the others.
        if let Err(err) = restore_project(dir, project, target, opts, now, &mut report) {
            report.error = Some(format!("{err:#}"));
        }
        projects.push(report);
    }

    let global_config = if opts.project.is_some() {
        GlobalConfigOutcome::Skipped
    } else {
        // The projects are already restored; their summary must still print.
        restore_global_config(dir, &manifest, opts)
            .unwrap_or_else(|err| GlobalConfigOutcome::Failed(format!("{err:#}")))
    };

    Ok(Report {
        dry_run: opts.dry_run,
        projects,
        global_config,
    })
}

/// Extract `file` into a private temp dir with the system `tar`.
fn extract(file: &Path) -> anyhow::Result<Staging> {
    if !file.is_file() {
        bail!("backup file {} not found", file.display());
    }
    let staging = Staging::new()?;
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(file)
        .arg("-C")
        .arg(&staging.0)
        .output()
        .context("running tar")?;
    if !output.status.success() {
        bail!(
            "could not extract {}: {}",
            file.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(staging)
}

fn read_manifest(dir: &Path) -> anyhow::Result<Manifest> {
    let path = dir.join(MANIFEST_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            bail!("not a bork backup: it has no {MANIFEST_FILE}")
        }
        Err(err) => return Err(err).context("reading manifest.json"),
    };
    parse_manifest(&bytes)
}

/// Parse `manifest.json`, checking its version before the rest so a newer
/// format gets a clear message instead of a field error.
pub fn parse_manifest(bytes: &[u8]) -> anyhow::Result<Manifest> {
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    let Versioned { version } =
        serde_json::from_slice(bytes).context("manifest.json is not a bork backup manifest")?;
    if version > MANIFEST_VERSION {
        bail!(
            "this backup uses format version {version}, newer than this bork reads ({MANIFEST_VERSION}); update bork (`bork update`) and retry"
        );
    }
    if version != MANIFEST_VERSION {
        bail!("unknown backup format version {version} (this bork reads {MANIFEST_VERSION})");
    }
    serde_json::from_slice(bytes).context("manifest.json is malformed")
}

fn select_projects<'a>(
    manifest: &'a Manifest,
    filter: Option<&str>,
) -> anyhow::Result<Vec<&'a ProjectManifest>> {
    if manifest.projects.is_empty() {
        bail!("the backup holds no projects");
    }
    let Some(filter) = filter else {
        return Ok(manifest.projects.iter().collect());
    };
    let matches: Vec<&ProjectManifest> = manifest
        .projects
        .iter()
        .filter(|p| p.name == filter || p.label.as_deref() == Some(filter))
        .collect();
    if matches.is_empty() {
        let names: Vec<&str> = manifest.projects.iter().map(|p| p.name.as_str()).collect();
        bail!(
            "no project '{filter}' in the backup (it has: {})",
            names.join(", ")
        );
    }
    Ok(matches)
}

/// Where a project goes: `<root>/<name>` with a root, else its recorded path.
pub fn resolve_target(project: &ProjectManifest, root: Option<&Path>) -> anyhow::Result<PathBuf> {
    match root {
        Some(root) => {
            if !is_plain_component(&project.name) {
                bail!(
                    "project name {:?} can't be used as a directory name",
                    project.name
                );
            }
            Ok(root.join(&project.name))
        }
        None => {
            if !project.path.is_absolute() {
                bail!(
                    "recorded path {} of {} is not absolute; pass --root",
                    project.path.display(),
                    project.name
                );
            }
            Ok(project.path.clone())
        }
    }
}

/// Refuse before writing anything: two projects on one target, or an
/// existing target without `force`.
fn check_targets(targets: &[PathBuf], force: bool) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for target in targets {
        // Case-insensitive, since macOS file systems usually are.
        if !seen.insert(target.to_string_lossy().to_lowercase()) {
            bail!(
                "two projects would be restored to {}; use --project to pick one",
                target.display()
            );
        }
    }
    if force {
        return Ok(());
    }
    let existing: Vec<String> = targets
        .iter()
        .filter(|t| fs::symlink_metadata(t).is_ok())
        .map(|t| t.display().to_string())
        .collect();
    if !existing.is_empty() {
        bail!(
            "already exists: {}\nPass --force to restore over it (main/ and worktrees are kept, restored files are overwritten), or --root to restore somewhere else.",
            existing.join(", ")
        );
    }
    Ok(())
}

/// Refuse to register a second project under a name that's already in use
/// at another path: issue ids and tmux sessions are built from the name, so
/// the two boards would share agent sessions.
fn check_names(projects: &[&ProjectManifest], targets: &[PathBuf]) -> anyhow::Result<()> {
    let mut names = HashSet::new();
    for project in projects {
        if !names.insert(project.name.as_str()) {
            bail!(
                "the backup holds two projects named '{}'; use --project with a label to pick one",
                project.name
            );
        }
    }
    let registry = crate::global_config::list_projects();
    for (project, target) in projects.iter().zip(targets) {
        let target = crate::global_config::normalize_path(target);
        let clash = registry.iter().find(|entry| {
            entry.name == project.name
                && crate::global_config::normalize_path(&entry.path) != target
                && entry.path.join(".bork").join("config.toml").exists()
        });
        if let Some(entry) = clash {
            bail!(
                "a project named '{}' is already registered at {}; two projects with one name share issue ids and tmux sessions.\nRestore to that path, or unregister it first (`bork project remove {}`).",
                project.name,
                entry.path.display(),
                entry.path.display()
            );
        }
    }
    Ok(())
}

fn restore_project(
    extracted: &Path,
    project: &ProjectManifest,
    target: &Path,
    opts: &Options,
    now: u64,
    report: &mut ProjectReport,
) -> anyhow::Result<()> {
    if !is_plain_component(&project.archive_dir) {
        bail!("invalid archive dir {:?}", project.archive_dir);
    }
    let src = extracted.join(PROJECTS_DIR).join(&project.archive_dir);
    if has_symlinked_parent(
        extracted,
        &Path::new(PROJECTS_DIR).join(&project.archive_dir).join("x"),
    ) {
        bail!(
            "projects/{} in the backup is a symlink",
            project.archive_dir
        );
    }
    // Sessions only survive when restoring over the same live project.
    let keep_sessions = fs::symlink_metadata(target).is_ok()
        && crate::global_config::normalize_path(target)
            == crate::global_config::normalize_path(&project.path);

    if !opts.dry_run {
        fs::create_dir_all(target).with_context(|| {
            format!(
                "creating {} (pass --root to restore somewhere else)",
                target.display()
            )
        })?;
    }
    report.repo = restore_repo(project, target, opts.dry_run, &mut report.warnings);

    let mut plan_files = Vec::new();
    for rel in &project.files {
        if rel == STATE_FILE {
            continue;
        }
        if rel.starts_with(&format!("{ISSUES_DIR}/")) {
            plan_files.push(rel);
            continue;
        }
        let Some(rel_path) = plain_relative(rel) else {
            report.warnings.push(format!("skipped unsafe path {rel:?}"));
            continue;
        };
        // A live worktree's plan is newer than the copy saved in `plans/`;
        // it's placed below instead.
        let superseded = rel
            .strip_prefix("plans/")
            .and_then(|rest| rest.strip_suffix(&format!("/{PLAN_NAME}")))
            .is_some_and(|id| {
                let worktree_plan = format!("{ISSUES_DIR}/{id}/{PLAN_NAME}");
                project.files.contains(&worktree_plan)
            });
        if superseded {
            continue;
        }
        // Plans are the work itself; a local copy is newer than the backup.
        if rel_path.starts_with("plans") && fs::symlink_metadata(target.join(&rel_path)).is_ok() {
            report.kept_files.push(rel.clone());
            continue;
        }
        match copy_entry(&src, &rel_path, target, &rel_path, opts.dry_run)? {
            Copied::Yes => report.files.push(rel.clone()),
            Copied::Missing => report
                .warnings
                .push(format!("{rel} is missing from the backup")),
            Copied::Unsafe => report.warnings.push(format!(
                "skipped {rel}: a directory on its path is a symlink"
            )),
        }
    }

    let state_rel = Path::new(STATE_FILE);
    let archived_board = fs::symlink_metadata(src.join(state_rel)).is_ok_and(|m| m.is_file())
        && !has_symlinked_parent(&src, state_rel);
    if !archived_board {
        report
            .warnings
            .push("the backup has no board for this project".to_string());
    } else if has_symlinked_parent(target, state_rel) {
        report
            .warnings
            .push("skipped the board: .bork/ at the target is a symlink".to_string());
    } else {
        match crate::config::try_load_state(&src) {
            Some(mut state) => {
                let branches: HashMap<&str, &str> = project
                    .issues
                    .iter()
                    .filter_map(|i| Some((i.id.as_str(), i.branch.as_deref()?)))
                    .collect();
                report.cleared_worktrees = rewrite_state(&mut state, target, keep_sessions, now)
                    .into_iter()
                    .map(|id| {
                        let branch = branches.get(id.as_str()).map(|b| b.to_string());
                        (id, branch)
                    })
                    .collect();
                report.issues = state.issues.len();
                report.board_restored = true;
                if !opts.dry_run {
                    crate::config::save_state(&state, target)?;
                }
            }
            None => report
                .warnings
                .push("the board in the backup is unreadable; skipped it".to_string()),
        }
    }

    for rel in plan_files {
        let parts: Vec<&str> = rel.split('/').collect();
        let [_, id, file] = parts[..] else {
            continue;
        };
        if !is_plain_component(id) || file != PLAN_NAME {
            continue;
        }
        let dest_rel = Path::new("plans").join(id).join(PLAN_NAME);
        if fs::symlink_metadata(target.join(&dest_rel)).is_ok() {
            report.kept_plans.push(id.to_string());
            continue;
        }
        match copy_entry(&src, Path::new(rel), target, &dest_rel, opts.dry_run)? {
            Copied::Yes => report.plans.push(id.to_string()),
            Copied::Missing => report
                .warnings
                .push(format!("{rel} is missing from the backup")),
            Copied::Unsafe => report.warnings.push(format!(
                "skipped {rel}: a directory on its path is a symlink"
            )),
        }
    }

    if !opts.dry_run {
        crate::config::ensure_agent_status_dir(target);
        crate::global_config::register_project(&project.name, target)
            .context("registering the project in projects.json")?;
        if let Some(label) = &project.label {
            if let Err(err) = crate::global_config::set_project_label(target, Some(label)) {
                report
                    .warnings
                    .push(format!("kept the name without label {label:?}: {err:#}"));
            }
        }
    }
    Ok(())
}

/// Clone the repo into `<target>/main` unless something is already there.
fn restore_repo(
    project: &ProjectManifest,
    target: &Path,
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> RepoOutcome {
    let main = target.join("main");
    if fs::symlink_metadata(&main).is_ok() {
        return RepoOutcome::KeptExisting;
    }
    let Some(url) = project.repo.clone_url.clone() else {
        return RepoOutcome::NoCloneUrl;
    };
    let branch = project.repo.default_branch.clone();
    if dry_run {
        return RepoOutcome::Cloned { url, branch };
    }
    println!("Cloning {url} into {} ...", main.display());
    if let Err(err) = crate::init::clone_repo(&url, &main) {
        return RepoOutcome::Failed {
            url,
            error: format!("{err:#}"),
        };
    }
    let git_ok = |args: &[&str]| {
        crate::external::git_command()
            .arg("-C")
            .arg(&main)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    // `git clone` names the remote `origin`; give it back its recorded name.
    let cloned_from = project.repo.remotes.iter().find(|r| r.url == url);
    if let Some(remote) = cloned_from.filter(|r| r.name != "origin") {
        if !git_ok(&["remote", "rename", "origin", &remote.name]) {
            warnings.push(format!("could not rename remote origin to {}", remote.name));
        }
    }
    for remote in &project.repo.remotes {
        if remote.url == url || remote.name == "origin" {
            continue;
        }
        if !git_ok(&["remote", "add", &remote.name, &remote.url]) {
            warnings.push(format!(
                "could not add remote {} ({})",
                remote.name, remote.url
            ));
        }
    }
    if let Some(branch) = &branch {
        if let Err(err) = checkout(&main, branch) {
            warnings.push(format!("could not check out {branch}: {err:#}"));
        }
    }
    RepoOutcome::Cloned { url, branch }
}

/// Check out `branch` in `repo` unless it's already the current branch.
fn checkout(repo: &Path, branch: &str) -> anyhow::Result<()> {
    let git = || {
        let mut cmd = crate::external::git_command();
        cmd.arg("-C").arg(repo);
        cmd
    };
    let current = git()
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .context("running git")?;
    if current.status.success() && String::from_utf8_lossy(&current.stdout).trim() == branch {
        return Ok(());
    }
    let output = git()
        .args(["checkout", "-q", branch])
        .output()
        .context("running git checkout")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// Prepare a backed-up board for a machine without its worktrees. An issue
/// whose worktree isn't at `target` loses the link (like a prune: `pruned_at`
/// set, setup to re-run) and its agent sessions. Other sessions are kept only
/// with `keep_sessions` (restoring over the same live project): transcripts
/// aren't backed up, and OpenCode can't tell a dead session from a live one.
/// Everything else is kept. Returns the ids whose worktree was cleared.
pub fn rewrite_state(
    state: &mut AppState,
    target: &Path,
    keep_sessions: bool,
    now: u64,
) -> Vec<String> {
    let mut cleared = Vec::new();
    for issue in &mut state.issues {
        let gone = issue
            .worktree
            .as_deref()
            .is_some_and(|w| !is_plain_component(w) || !target.join(w).is_dir());
        if gone {
            issue.worktree = None;
            issue.pruned_at = Some(now);
            issue.setup_ran = false;
            issue.sessions.clear();
            cleared.push(issue.id.clone());
        } else if !keep_sessions {
            issue.sessions.clear();
        }
    }
    cleared
}

/// `rel` as a relative path of plain components, `None` if it could escape.
fn plain_relative(rel: &str) -> Option<PathBuf> {
    let path = Path::new(rel);
    let plain = !rel.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)));
    plain.then(|| path.to_path_buf())
}

enum Copied {
    Yes,
    Missing,
    /// A directory on the source or destination path is a symlink.
    Unsafe,
}

/// Whether a directory between `root` and `root/rel` is a symlink, which
/// could point anywhere on disk.
fn has_symlinked_parent(root: &Path, rel: &Path) -> bool {
    let mut dir = root.to_path_buf();
    let Some(parents) = rel.parent() else {
        return false;
    };
    parents.components().any(|part| {
        dir.push(part);
        fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink())
    })
}

/// Copy `src_root/src_rel` to `dest_root/dest_rel`. Symlinks are recreated as
/// symlinks, files replaced atomically with their mode. Nothing is read or
/// written through a symlinked directory on either side.
fn copy_entry(
    src_root: &Path,
    src_rel: &Path,
    dest_root: &Path,
    dest_rel: &Path,
    dry_run: bool,
) -> anyhow::Result<Copied> {
    if has_symlinked_parent(src_root, src_rel) || has_symlinked_parent(dest_root, dest_rel) {
        return Ok(Copied::Unsafe);
    }
    let dest = &dest_root.join(dest_rel);
    let src = src_root.join(src_rel);
    let meta = match fs::symlink_metadata(&src) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Copied::Missing),
        Err(err) => return Err(err).with_context(|| format!("reading {}", src.display())),
    };
    let file_type = meta.file_type();
    if !file_type.is_symlink() && !file_type.is_file() {
        return Ok(Copied::Missing);
    }
    if dry_run {
        return Ok(Copied::Yes);
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let context = || format!("writing {}", dest.display());
    if file_type.is_symlink() {
        let link = fs::read_link(&src).with_context(|| format!("reading {}", src.display()))?;
        match fs::remove_file(dest) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(context),
        }
        std::os::unix::fs::symlink(link, dest).with_context(context)?;
    } else {
        let bytes = fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
        crate::config::atomic_write(dest, &bytes).with_context(context)?;
        fs::set_permissions(
            dest,
            fs::Permissions::from_mode(meta.permissions().mode() & 0o777),
        )
        .with_context(context)?;
    }
    Ok(Copied::Yes)
}

/// Put `_global/config.toml` back unless the user already has one.
fn restore_global_config(
    extracted: &Path,
    manifest: &Manifest,
    opts: &Options,
) -> anyhow::Result<GlobalConfigOutcome> {
    let rel = format!("{GLOBAL_DIR}/{GLOBAL_CONFIG}");
    if !manifest.global_files.contains(&rel) {
        return Ok(GlobalConfigOutcome::Skipped);
    }
    let dir = crate::global_config::global_config_dir();
    let dest = dir.join(GLOBAL_CONFIG);
    if !opts.force && fs::symlink_metadata(&dest).is_ok() {
        return Ok(GlobalConfigOutcome::KeptExisting(dest));
    }
    match copy_entry(
        extracted,
        Path::new(&rel),
        &dir,
        Path::new(GLOBAL_CONFIG),
        opts.dry_run,
    )? {
        Copied::Yes => Ok(GlobalConfigOutcome::Restored(dest)),
        Copied::Missing | Copied::Unsafe => Ok(GlobalConfigOutcome::Skipped),
    }
}

impl Report {
    /// Human-readable summary, one block per project.
    pub fn render(&self) -> String {
        let would = |done: &str, planned: &str| {
            if self.dry_run {
                planned.to_string()
            } else {
                done.to_string()
            }
        };
        let mut out = String::new();
        if self.dry_run {
            out.push_str("Dry run, nothing was changed.\n\n");
        }
        for p in &self.projects {
            let label = p
                .label
                .as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default();
            let _ = writeln!(out, "{}{label} -> {}", p.name, p.target.display());

            let repo = match &p.repo {
                RepoOutcome::Cloned { url, branch } => {
                    let branch = branch
                        .as_deref()
                        .map(|b| format!(" on {b}"))
                        .unwrap_or_default();
                    format!("{} {url}{branch}", would("cloned", "would clone"))
                }
                RepoOutcome::KeptExisting => "kept the existing main/".to_string(),
                RepoOutcome::NoCloneUrl => {
                    "no remote in the backup; clone the repo into main/ yourself".to_string()
                }
                RepoOutcome::Failed { url, error } => {
                    format!("clone of {url} failed ({error}); clone it into main/ yourself")
                }
            };
            let _ = writeln!(out, "  repo:     {repo}");

            if p.board_restored {
                let _ = writeln!(
                    out,
                    "  board:    {} {}",
                    would("restored", "would restore"),
                    count(p.issues, "issue")
                );
            }
            if !p.cleared_worktrees.is_empty() {
                let list: Vec<String> = p
                    .cleared_worktrees
                    .iter()
                    .map(|(id, branch)| match branch {
                        Some(b) => format!("{id} ({b})"),
                        None => id.clone(),
                    })
                    .collect();
                let _ = writeln!(
                    out,
                    "  worktrees: not recreated, reopen from their branches: {}",
                    list.join(", ")
                );
            }
            let _ = writeln!(
                out,
                "  files:    {} {}",
                would("restored", "would restore"),
                count(p.files.len(), "container file")
            );
            if self.dry_run {
                for file in &p.files {
                    let _ = writeln!(out, "            {file}");
                }
            }
            if !p.plans.is_empty() {
                let _ = writeln!(
                    out,
                    "  plans:    {} plans/<id>/planning.md for {}",
                    would("wrote", "would write"),
                    p.plans.join(", ")
                );
            }
            if !p.kept_files.is_empty() {
                let _ = writeln!(out, "  kept:     existing {}", p.kept_files.join(", "));
            }
            if !p.kept_plans.is_empty() {
                let _ = writeln!(
                    out,
                    "  plans:    kept existing plans for {}",
                    p.kept_plans.join(", ")
                );
            }
            if p.error.is_none() {
                let _ = writeln!(
                    out,
                    "  {}",
                    would(
                        "registered in projects.json",
                        "would register in projects.json"
                    )
                );
            }
            for warning in &p.warnings {
                let _ = writeln!(out, "  warning:  {warning}");
            }
            if let Some(error) = &p.error {
                let _ = writeln!(out, "  error:    stopped here: {error}");
            }
            out.push('\n');
        }
        match &self.global_config {
            GlobalConfigOutcome::Skipped => {}
            GlobalConfigOutcome::Restored(path) => {
                let _ = writeln!(
                    out,
                    "global config: {} {}",
                    would("restored", "would restore"),
                    path.display()
                );
            }
            GlobalConfigOutcome::Failed(error) => {
                let _ = writeln!(out, "global config: not restored: {error}");
            }
            GlobalConfigOutcome::KeptExisting(path) => {
                let _ = writeln!(
                    out,
                    "global config: kept the existing {} (--force replaces it)",
                    path.display()
                );
            }
        }
        out.trim_end().to_string()
    }
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentKind, Column, Issue};

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = crate::external::git_command()
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = crate::external::git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn opts(root: &Path) -> Options {
        Options {
            root: Some(root.to_path_buf()),
            ..Options::default()
        }
    }

    fn manifest_json(version: u32) -> String {
        format!(
            r#"{{"version":{version},"created_at":1,"bork_version":"0","projects":[],"global_files":[]}}"#
        )
    }

    /// A board with one issue per `(id, worktree)`, each carrying a session.
    fn board(issues: &[(&str, Option<&str>)]) -> AppState {
        let mut state = AppState::default();
        for (id, worktree) in issues {
            let mut issue = Issue::new(*id, "t", Column::InProgress, AgentKind::Claude);
            issue.worktree = worktree.map(String::from);
            issue.prompt = Some(format!("prompt {id}"));
            issue.setup_ran = true;
            issue.linked_issues = vec!["demo-9".to_string()];
            issue
                .sessions
                .insert(AgentKind::Claude, format!("sess-{id}"));
            state.issues.push(issue);
        }
        state
    }

    /// Back up a registered, opted-in project named `demo` (no `main/`, so no
    /// clone URL) built by `setup`, in its own config home. Returns the archive.
    fn backup_demo(out: &Path, setup: impl FnOnce(&Path)) -> PathBuf {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let mut file = None;
        crate::global_config::with_test_config_home(home.path(), || {
            let global = crate::global_config::global_config_dir().join("config.toml");
            write(
                &global,
                &format!("[backup]\npath = \"{}\"\n", out.display()),
            );
            write(
                &root.join(".bork/config.toml"),
                "project_name = \"demo\"\n[backup]\nenabled = true\n",
            );
            crate::global_config::register_project("demo", &root).unwrap();
            setup(&root);
            file = Some(
                super::super::run(&super::super::Selection::All)
                    .unwrap()
                    .unwrap()
                    .file,
            );
        });
        file.unwrap()
    }

    /// An archive with a hand-written manifest and `(archive path, source)` entries.
    fn crafted_archive(
        dir: &Path,
        projects: serde_json::Value,
        entries: &[(&str, &Path)],
    ) -> PathBuf {
        let manifest = serde_json::json!({
            "version": MANIFEST_VERSION, "created_at": 1, "bork_version": "0",
            "projects": projects,
        });
        let entries: Vec<_> = entries
            .iter()
            .map(|(path, source)| super::super::archive::ArchiveEntry {
                path: path.to_string(),
                source: source.to_path_buf(),
                follow: false,
            })
            .collect();
        let file = dir.join("crafted.tar.gz");
        super::super::archive::write(&file, manifest.to_string().as_bytes(), &entries).unwrap();
        file
    }

    fn project_json(name: &str, archive_dir: &str, files: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "name": name, "path": format!("/old/{name}"), "archive_dir": archive_dir,
            "repo": {"clone_url": null, "remotes": [], "default_branch": null},
            "files": files, "issues": []
        })
    }

    fn demo_archive(out: &Path) -> PathBuf {
        backup_demo(out, |root| {
            crate::config::save_state(
                &board(&[("demo-1", Some("demo-1-a")), ("demo-2", None)]),
                root,
            )
            .unwrap();
            write(&root.join("AGENTS.md"), "agents");
            write(&root.join("plans/demo-2/planning.md"), "orchestrator plan");
            write(&root.join(".claude/settings.json"), "backup settings");
            std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
            write(&root.join("demo-1-a/.git"), "gitdir: x");
            write(&root.join("demo-1-a/.claude/planning.md"), "worktree plan");
        })
    }

    #[test]
    fn manifest_version_is_checked_first() {
        assert!(parse_manifest(manifest_json(MANIFEST_VERSION).as_bytes()).is_ok());
        let newer = parse_manifest(br#"{"version":99,"whatever":true}"#).unwrap_err();
        assert!(format!("{newer:#}").contains("newer"), "{newer:#}");
        let older = parse_manifest(manifest_json(0).as_bytes()).unwrap_err();
        assert!(format!("{older:#}").contains("unknown backup format version 0"));
        assert!(parse_manifest(b"[]").is_err());
        assert!(parse_manifest(br#"{"version":1}"#).is_err());
    }

    #[test]
    fn resolves_targets() {
        let mut project: ProjectManifest = serde_json::from_value(serde_json::json!({
            "name": "demo", "path": "/old/demo", "archive_dir": "demo",
            "repo": {"clone_url": null, "remotes": [], "default_branch": null},
            "files": [], "issues": []
        }))
        .unwrap();
        assert_eq!(
            resolve_target(&project, None).unwrap(),
            PathBuf::from("/old/demo")
        );
        assert_eq!(
            resolve_target(&project, Some(Path::new("/new"))).unwrap(),
            PathBuf::from("/new/demo")
        );
        project.path = PathBuf::from("rel/demo");
        assert!(resolve_target(&project, None).is_err());
        project.name = "../x".to_string();
        assert!(resolve_target(&project, Some(Path::new("/new"))).is_err());
    }

    #[test]
    fn rewrite_clears_missing_worktrees_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().canonicalize().unwrap();
        fs::create_dir_all(target.join("demo-2-b")).unwrap();
        let mut state = board(&[
            ("demo-1", Some("demo-1-a")),
            ("demo-2", Some("demo-2-b")),
            ("demo-3", None),
        ]);
        state.issues[0].pruned_at = None;

        let cleared = rewrite_state(&mut state, &target, true, 42);

        assert_eq!(cleared, vec!["demo-1".to_string()]);
        let gone = &state.issues[0];
        assert_eq!(gone.worktree, None);
        assert_eq!(gone.pruned_at, Some(42));
        assert!(!gone.setup_ran);
        assert!(gone.sessions.is_empty());
        assert_eq!(gone.prompt.as_deref(), Some("prompt demo-1"));
        assert_eq!(gone.column, Column::InProgress);
        assert_eq!(gone.linked_issues, vec!["demo-9".to_string()]);
        // Restoring over the live project: present worktrees and worktree-less
        // issues keep their sessions.
        assert_eq!(state.issues[1].worktree.as_deref(), Some("demo-2-b"));
        assert!(state.issues[1].setup_ran);
        assert_eq!(state.issues[1].sessions.len(), 1);
        assert_eq!(state.issues[2].sessions.len(), 1);

        // A fresh restore: every session goes, transcripts aren't backed up.
        let mut fresh = board(&[("demo-2", Some("demo-2-b")), ("demo-3", None)]);
        assert!(rewrite_state(&mut fresh, &target, false, 42).is_empty());
        assert!(fresh.issues.iter().all(|i| i.sessions.is_empty()));
        assert_eq!(fresh.issues[1].pruned_at, None);
    }

    #[test]
    fn restores_board_files_and_plans() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let dest_root = dest.path().canonicalize().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let report = import(&file, &opts(&dest_root)).unwrap();

            let target = dest_root.join("demo");
            let p = &report.projects[0];
            assert_eq!(p.repo, RepoOutcome::NoCloneUrl);
            assert_eq!(p.issues, 2);
            assert_eq!(p.cleared_worktrees, vec![("demo-1".to_string(), None)]);
            assert_eq!(p.plans, vec!["demo-1".to_string()]);
            assert!(p.warnings.is_empty(), "{:?}", p.warnings);

            let state = crate::config::load_state(&target);
            assert_eq!(state.issues.len(), 2);
            assert_eq!(state.issues[0].worktree, None);
            assert!(state.issues.iter().all(|i| i.sessions.is_empty()));
            assert_eq!(read(&target.join("AGENTS.md")), "agents");
            assert_eq!(
                fs::read_link(target.join("CLAUDE.md")).unwrap(),
                PathBuf::from("AGENTS.md")
            );
            assert!(read(&target.join(".bork/config.toml")).contains("project_name = \"demo\""));
            assert_eq!(
                read(&target.join("plans/demo-1/planning.md")),
                "worktree plan"
            );
            assert_eq!(
                read(&target.join("plans/demo-2/planning.md")),
                "orchestrator plan"
            );
            assert!(!target.join(ISSUES_DIR).exists());
            assert!(target.join(".bork/agent-status").is_dir());

            let registry = crate::global_config::list_projects();
            assert_eq!(registry.len(), 1);
            assert_eq!(registry[0].name, "demo");
            assert_eq!(registry[0].path, target);
            assert!(matches!(
                report.global_config,
                GlobalConfigOutcome::Restored(_)
            ));
            assert!(
                read(&crate::global_config::global_config_dir().join("config.toml"))
                    .contains("[backup]")
            );
        });
    }

    #[test]
    fn dry_run_changes_nothing() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let report = import(
                &file,
                &Options {
                    dry_run: true,
                    ..opts(dest.path())
                },
            )
            .unwrap();

            assert!(!dest.path().join("demo").exists());
            assert!(crate::global_config::list_projects().is_empty());
            assert!(!crate::global_config::global_config_dir()
                .join("config.toml")
                .exists());
            let p = &report.projects[0];
            assert_eq!(p.plans, vec!["demo-1".to_string()]);
            assert!(p.files.contains(&"AGENTS.md".to_string()));
            let text = report.render();
            assert!(text.contains("Dry run"), "{text}");
            assert!(text.contains("would restore 2 issues"), "{text}");
            assert!(text.contains("AGENTS.md"), "{text}");
        });
    }

    #[test]
    fn existing_target_needs_force_and_force_keeps_local_work() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let dest_root = dest.path().canonicalize().unwrap();
        let target = dest_root.join("demo");
        write(&target.join("main/code.rs"), "local code");
        write(&target.join("demo-1-a/wip.rs"), "local wip");
        write(&target.join("plans/demo-1/planning.md"), "newer local plan");
        write(&target.join("AGENTS.md"), "stale");
        write(
            &target.join("plans/demo-2/planning.md"),
            "newer orchestrator plan",
        );
        let outside = dest_root.join("outside");
        write(&outside.join("settings.json"), "outside settings");
        std::os::unix::fs::symlink(&outside, target.join(".claude")).unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let global = crate::global_config::global_config_dir().join("config.toml");
            write(&global, "mine = true\n");

            let err = import(&file, &opts(&dest_root)).unwrap_err();
            assert!(format!("{err:#}").contains("--force"), "{err:#}");
            assert_eq!(read(&target.join("AGENTS.md")), "stale");

            let force = Options {
                force: true,
                ..opts(&dest_root)
            };
            for _ in 0..2 {
                let report = import(&file, &force).unwrap();
                let p = &report.projects[0];
                assert_eq!(p.repo, RepoOutcome::KeptExisting);
                assert!(p.cleared_worktrees.is_empty());
                assert_eq!(p.kept_plans, vec!["demo-1".to_string()]);
                assert_eq!(p.kept_files, vec!["plans/demo-2/planning.md".to_string()]);
                assert!(
                    p.warnings
                        .iter()
                        .any(|w| w.contains(".claude/settings.json")),
                    "{:?}",
                    p.warnings
                );
            }
            assert_eq!(read(&outside.join("settings.json")), "outside settings");
            assert_eq!(
                read(&target.join("plans/demo-2/planning.md")),
                "newer orchestrator plan"
            );
            assert_eq!(read(&target.join("main/code.rs")), "local code");
            assert_eq!(read(&target.join("demo-1-a/wip.rs")), "local wip");
            assert_eq!(
                read(&target.join("plans/demo-1/planning.md")),
                "newer local plan"
            );
            assert_eq!(read(&target.join("AGENTS.md")), "agents");
            let state = crate::config::load_state(&target);
            assert_eq!(state.issues[0].worktree.as_deref(), Some("demo-1-a"));
            assert_eq!(crate::global_config::list_projects().len(), 1);
            // --force also replaces the global config.
            assert!(read(&global).contains("[backup]"));
        });
    }

    #[test]
    fn a_board_missing_from_the_archive_keeps_the_local_one() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        write(&src.join("AGENTS.md"), "agents");
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([project_json("demo", "demo", &[STATE_FILE, "AGENTS.md"])]),
            &[("projects/demo/AGENTS.md", &src.join("AGENTS.md"))],
        );

        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let target = dest.path().join("demo");
        crate::config::save_state(&board(&[("demo-1", None)]), &target).unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            let p = &report.projects[0];
            assert!(!p.board_restored);
            assert!(
                p.warnings.iter().any(|w| w.contains("no board")),
                "{:?}",
                p.warnings
            );
            assert_eq!(crate::config::load_state(&target).issues.len(), 1);
        });
    }

    #[test]
    fn nothing_is_read_through_a_symlinked_archive_dir() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write(&outside.join("settings.json"), "outside the archive");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([project_json("demo", "demo", &[".claude/settings.json"])]),
            &[("projects/demo/.claude", &link)],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let report = import(&file, &opts(dest.path())).unwrap();
            let p = &report.projects[0];
            assert!(p.files.is_empty());
            assert!(
                p.warnings.iter().any(|w| w.contains("symlink")),
                "{:?}",
                p.warnings
            );
            assert!(!dest.path().join("demo/.claude/settings.json").exists());
        });
    }

    #[test]
    fn a_failing_project_does_not_stop_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("AGENTS.md");
        write(&agents, "agents");
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([
                project_json("broken", "..", &["AGENTS.md"]),
                project_json("good", "good", &["AGENTS.md"]),
            ]),
            &[("projects/good/AGENTS.md", &agents)],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let report = import(&file, &opts(dest.path())).unwrap();
            assert!(report.failed());
            assert!(report.projects[0].error.is_some());
            assert!(report.projects[1].error.is_none());
            assert_eq!(read(&dest.path().join("good/AGENTS.md")), "agents");
            assert!(report.render().contains("stopped here"));
            let names: Vec<String> = crate::global_config::list_projects()
                .into_iter()
                .map(|p| p.name)
                .collect();
            assert_eq!(names, vec!["good".to_string()]);
        });
    }

    #[test]
    fn refuses_a_name_registered_at_another_path() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let original = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(
            &original.path().join(".bork/config.toml"),
            "project_name = \"demo\"\n",
        );
        crate::global_config::with_test_config_home(home.path(), || {
            crate::global_config::register_project("demo", original.path()).unwrap();
            let err = import(&file, &opts(dest.path())).unwrap_err();
            assert!(format!("{err:#}").contains("already registered"), "{err:#}");
            assert!(!dest.path().join("demo").exists());
        });
    }

    #[test]
    fn refuses_a_symlinked_project_dir_in_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write(&outside.join("secret.md"), "not in the backup");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([project_json("demo", "demo", &["secret.md"])]),
            &[("projects/demo", &link)],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let report = import(&file, &opts(dest.path())).unwrap();
            let error = report.projects[0].error.as_deref().unwrap_or_default();
            assert!(error.contains("symlink"), "{error}");
            assert!(!dest.path().join("demo/secret.md").exists());
        });
    }

    #[test]
    fn refuses_targets_and_names_that_collide() {
        let dir = tempfile::tempdir().unwrap();
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([
                project_json("Demo", "Demo", &[]),
                project_json("demo", "demo-2", &[]),
            ]),
            &[],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let err = import(&file, &opts(dest.path())).unwrap_err();
            assert!(
                format!("{err:#}").contains("two projects would be restored"),
                "{err:#}"
            );

            let mut a: ProjectManifest =
                serde_json::from_value(project_json("demo", "demo", &[])).unwrap();
            let b = a.clone();
            a.path = PathBuf::from("/elsewhere/demo");
            let err = check_names(&[&a, &b], &[a.path.clone(), b.path.clone()]).unwrap_err();
            assert!(format!("{err:#}").contains("two projects named"), "{err:#}");
        });
    }

    #[test]
    fn a_failed_global_config_still_returns_the_report() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            // A directory where the config file goes can't be replaced.
            fs::create_dir_all(crate::global_config::global_config_dir().join("config.toml/x"))
                .unwrap();
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            assert!(matches!(
                report.global_config,
                GlobalConfigOutcome::Failed(_)
            ));
            assert!(report.failed());
            assert!(report.projects[0].error.is_none());
            assert!(report.render().contains("global config: not restored"));
        });
    }

    #[test]
    fn keeps_an_existing_global_config_and_filters_projects() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let global = crate::global_config::global_config_dir().join("config.toml");
            write(&global, "mine = true\n");

            let unknown = Options {
                project: Some("nope".to_string()),
                ..opts(dest.path())
            };
            let err = import(&file, &unknown).unwrap_err();
            assert!(format!("{err:#}").contains("it has: demo"), "{err:#}");

            let report = import(&file, &opts(dest.path())).unwrap();
            assert!(matches!(
                report.global_config,
                GlobalConfigOutcome::KeptExisting(_)
            ));
            assert_eq!(read(&global), "mine = true\n");
        });
    }

    #[test]
    fn missing_or_bad_files_fail_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let missing = import(&dir.path().join("nope.tar.gz"), &Options::default()).unwrap_err();
        assert!(format!("{missing:#}").contains("not found"));

        let junk = dir.path().join("junk.tar.gz");
        write(&junk, "not a tarball");
        assert!(import(&junk, &Options::default()).is_err());

        let staging = dir.path().join("stage");
        write(&staging.join("manifest.json"), &manifest_json(7));
        let newer = dir.path().join("newer.tar.gz");
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&newer)
            .arg("-C")
            .arg(&staging)
            .arg("manifest.json")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let err = import(&newer, &Options::default()).unwrap_err();
        assert!(
            format!("{err:#}").contains("newer than this bork"),
            "{err:#}"
        );
    }

    /// Full round trip: a project whose `main/` was cloned from a local bare
    /// repo is backed up with the real backup code and imported elsewhere.
    #[test]
    fn round_trip_clones_restores_and_registers() {
        let work = tempfile::tempdir().unwrap();
        let work = work.path().canonicalize().unwrap();
        let origin = work.join("origin.git");
        let seed = work.join("seed");
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "trunk"]);
        write(&seed.join("README.md"), "hello");
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(
            &work,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );

        let out = work.join("backups");
        let file = backup_demo(&out, |root| {
            let main = root.join("main");
            git(
                root,
                &[
                    "clone",
                    "-q",
                    origin.to_str().unwrap(),
                    main.to_str().unwrap(),
                ],
            );
            // No `origin`: the clone has to give `fork` its name back.
            git(&main, &["remote", "rename", "origin", "fork"]);
            git(
                &main,
                &["remote", "add", "upstream", "https://example.com/up.git"],
            );
            // An older saved plan loses to the live worktree's plan.
            write(&root.join("plans/demo-1/planning.md"), "old saved plan");
            let wt = root.join("demo-1-fix");
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "demo-1/fix",
                    wt.to_str().unwrap(),
                ],
            );
            write(&wt.join(".claude/planning.md"), "fix plan");
            crate::config::save_state(&board(&[("demo-1", Some("demo-1-fix"))]), root).unwrap();
            write(&root.join("AGENTS.md"), "agents");
            crate::global_config::set_project_label(root, Some("Demo")).unwrap();
        });

        let home = work.join("new-home");
        let dest = work.join("restored");
        crate::global_config::with_test_config_home(&home, || {
            let report = import(&file, &opts(&dest)).unwrap();
            let p = &report.projects[0];
            assert_eq!(
                p.repo,
                RepoOutcome::Cloned {
                    url: origin.to_str().unwrap().to_string(),
                    branch: Some("trunk".to_string()),
                }
            );
            assert_eq!(
                p.cleared_worktrees,
                vec![("demo-1".to_string(), Some("demo-1/fix".to_string()))]
            );

            let target = dest.join("demo");
            assert_eq!(read(&target.join("main/README.md")), "hello");
            assert_eq!(
                git_out(&target.join("main"), &["remote", "get-url", "upstream"]),
                "https://example.com/up.git"
            );
            assert_eq!(
                git_out(&target.join("main"), &["remote", "get-url", "fork"]),
                origin.to_str().unwrap()
            );
            assert_eq!(
                git_out(&target.join("main"), &["remote", "get-url", "origin"]),
                ""
            );
            assert_eq!(
                git_out(&target.join("main"), &["symbolic-ref", "--short", "HEAD"]),
                "trunk"
            );
            assert!(!target.join("demo-1-fix").exists());
            assert_eq!(read(&target.join("plans/demo-1/planning.md")), "fix plan");
            assert_eq!(read(&target.join("AGENTS.md")), "agents");
            let state = crate::config::load_state(&target);
            assert_eq!(state.issues[0].worktree, None);
            assert_eq!(state.issues[0].prompt.as_deref(), Some("prompt demo-1"));

            let registry = crate::global_config::list_projects();
            assert_eq!(registry.len(), 1);
            assert_eq!(registry[0].path, target);
            assert_eq!(registry[0].label.as_deref(), Some("Demo"));
        });
    }

    #[test]
    fn failed_clone_still_restores_the_board() {
        let out = tempfile::tempdir().unwrap();
        let file = backup_demo(out.path(), |root| {
            let main = root.join("main");
            fs::create_dir_all(&main).unwrap();
            git(&main, &["init", "-q", "-b", "trunk"]);
            git(&main, &["remote", "add", "origin", "/nonexistent/repo.git"]);
            crate::config::save_state(&board(&[("demo-1", None)]), root).unwrap();
        });
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let report = import(&file, &opts(dest.path())).unwrap();
            let p = &report.projects[0];
            assert!(matches!(p.repo, RepoOutcome::Failed { .. }), "{:?}", p.repo);
            assert!(report.render().contains("clone it into main/ yourself"));
            assert_eq!(
                crate::config::load_state(&dest.path().join("demo"))
                    .issues
                    .len(),
                1
            );
            assert_eq!(crate::global_config::list_projects().len(), 1);
        });
    }
}
