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
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalConfigOutcome {
    /// Nothing to restore: single-project archive, no global config in it,
    /// or `--project` was given.
    Skipped,
    Restored(PathBuf),
    /// The user already has one and `force` was off.
    KeptExisting(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Report {
    pub dry_run: bool,
    pub projects: Vec<ProjectReport>,
    pub global_config: GlobalConfigOutcome,
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

    let now = crate::app::unix_now();
    let mut projects = Vec::new();
    for (project, target) in selected.iter().zip(&targets) {
        let report = restore_project(dir, project, target, opts, now)
            .with_context(|| format!("restoring {} to {}", project.name, target.display()))?;
        projects.push(report);
    }

    let global_config = if opts.project.is_some() {
        GlobalConfigOutcome::Skipped
    } else {
        restore_global_config(dir, &manifest, opts)?
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
        if !seen.insert(target) {
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

fn restore_project(
    extracted: &Path,
    project: &ProjectManifest,
    target: &Path,
    opts: &Options,
    now: u64,
) -> anyhow::Result<ProjectReport> {
    if !is_plain_component(&project.archive_dir) {
        bail!("invalid archive dir {:?}", project.archive_dir);
    }
    let src = extracted.join(PROJECTS_DIR).join(&project.archive_dir);
    let mut report = ProjectReport {
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
        warnings: Vec::new(),
    };

    if !opts.dry_run {
        fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
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
        match copy_entry(&src, &rel_path, &target.join(&rel_path), opts.dry_run)? {
            Copied::Yes => report.files.push(rel.clone()),
            Copied::Missing => report
                .warnings
                .push(format!("{rel} is missing from the backup")),
            Copied::Unsafe => report
                .warnings
                .push(format!("skipped {rel}: it sits under a symlink")),
        }
    }

    if project.files.iter().any(|f| f == STATE_FILE) {
        match crate::config::try_load_state(&src) {
            Some(mut state) => {
                let branches: HashMap<&str, &str> = project
                    .issues
                    .iter()
                    .filter_map(|i| Some((i.id.as_str(), i.branch.as_deref()?)))
                    .collect();
                report.cleared_worktrees = rewrite_state(&mut state, target, &project.path, now)
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
    } else {
        report
            .warnings
            .push("the backup has no board for this project".to_string());
    }

    for rel in plan_files {
        let parts: Vec<&str> = rel.split('/').collect();
        let [_, id, file] = parts[..] else {
            continue;
        };
        if !is_plain_component(id) || file != PLAN_NAME {
            continue;
        }
        let dest = target.join("plans").join(id).join(PLAN_NAME);
        if fs::symlink_metadata(&dest).is_ok() {
            report.kept_plans.push(id.to_string());
            continue;
        }
        match copy_entry(&src, Path::new(rel), &dest, opts.dry_run)? {
            Copied::Yes => report.plans.push(id.to_string()),
            Copied::Missing => report
                .warnings
                .push(format!("{rel} is missing from the backup")),
            Copied::Unsafe => report
                .warnings
                .push(format!("skipped {rel}: it sits under a symlink")),
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
    Ok(report)
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
/// set, setup to re-run) and its agent sessions, since transcripts aren't
/// backed up and resume is tied to the worktree. Restoring somewhere else
/// than `recorded` drops every session for the same reason. Everything else
/// is kept. Returns the ids whose worktree was cleared.
pub fn rewrite_state(
    state: &mut AppState,
    target: &Path,
    recorded: &Path,
    now: u64,
) -> Vec<String> {
    let moved = crate::global_config::normalize_path(target)
        != crate::global_config::normalize_path(recorded);
    let mut cleared = Vec::new();
    for issue in &mut state.issues {
        let Some(worktree) = issue.worktree.clone() else {
            if moved {
                issue.sessions.clear();
            }
            continue;
        };
        let present = is_plain_component(&worktree) && target.join(&worktree).is_dir();
        if present {
            if moved {
                issue.sessions.clear();
            }
            continue;
        }
        issue.worktree = None;
        issue.pruned_at = Some(now);
        issue.setup_ran = false;
        issue.sessions.clear();
        cleared.push(issue.id.clone());
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
    /// A parent inside the extracted archive is a symlink, which could point
    /// anywhere on disk.
    Unsafe,
}

/// Copy `src_root/rel` to `dest`. Symlinks are recreated as symlinks, files
/// replaced atomically with their mode.
fn copy_entry(src_root: &Path, rel: &Path, dest: &Path, dry_run: bool) -> anyhow::Result<Copied> {
    let mut parent = src_root.to_path_buf();
    if let Some(dir) = rel.parent() {
        for part in dir.components() {
            parent.push(part);
            if fs::symlink_metadata(&parent).is_ok_and(|m| m.file_type().is_symlink()) {
                return Ok(Copied::Unsafe);
            }
        }
    }
    let src = src_root.join(rel);
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
    let dest = crate::global_config::global_config_dir().join(GLOBAL_CONFIG);
    if !opts.force && fs::symlink_metadata(&dest).is_ok() {
        return Ok(GlobalConfigOutcome::KeptExisting(dest));
    }
    match copy_entry(extracted, Path::new(&rel), &dest, opts.dry_run)? {
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
            if !p.kept_plans.is_empty() {
                let _ = writeln!(
                    out,
                    "  plans:    kept existing plans for {}",
                    p.kept_plans.join(", ")
                );
            }
            let _ = writeln!(
                out,
                "  {}",
                would(
                    "registered in projects.json",
                    "would register in projects.json"
                )
            );
            for warning in &p.warnings {
                let _ = writeln!(out, "  warning:  {warning}");
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

    fn demo_archive(out: &Path) -> PathBuf {
        backup_demo(out, |root| {
            crate::config::save_state(
                &board(&[("demo-1", Some("demo-1-a")), ("demo-2", None)]),
                root,
            )
            .unwrap();
            write(&root.join("AGENTS.md"), "agents");
            write(&root.join("plans/demo-2/planning.md"), "orchestrator plan");
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

        let cleared = rewrite_state(&mut state, &target, &target, 42);

        assert_eq!(cleared, vec!["demo-1".to_string()]);
        let gone = &state.issues[0];
        assert_eq!(gone.worktree, None);
        assert_eq!(gone.pruned_at, Some(42));
        assert!(!gone.setup_ran);
        assert!(gone.sessions.is_empty());
        assert_eq!(gone.prompt.as_deref(), Some("prompt demo-1"));
        assert_eq!(gone.column, Column::InProgress);
        assert_eq!(gone.linked_issues, vec!["demo-9".to_string()]);
        // Same path: present worktrees and worktree-less issues keep their sessions.
        assert_eq!(state.issues[1].worktree.as_deref(), Some("demo-2-b"));
        assert!(state.issues[1].setup_ran);
        assert_eq!(state.issues[1].sessions.len(), 1);
        assert_eq!(state.issues[2].sessions.len(), 1);

        // Another path: every session goes, their transcripts are tied to the old one.
        let mut moved = board(&[("demo-3", None)]);
        rewrite_state(&mut moved, &target, Path::new("/elsewhere"), 42);
        assert!(moved.issues[0].sessions.is_empty());
        assert_eq!(moved.issues[0].pruned_at, None);
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
            assert!(state.issues[0].sessions.is_empty());
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
            }
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
