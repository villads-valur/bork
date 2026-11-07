//! `bork import <file>`: put projects back from a backup archive written by
//! [`super::run`].
//!
//! Per project: clone the repo into `<target>/main` (when there's no `main/`
//! yet), restore the board and container files, move each worktree plan to
//! `plans/<issue-id>/planning.md` and register the project. Worktrees are not
//! recreated; issues that had one lose the link and get reopened from their
//! pushed branches later. Agent sessions are dropped unless the board being
//! replaced still has them, since transcripts aren't backed up.
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
    /// Cloned, but the recorded default branch couldn't be checked out, so
    /// `main/` is on the remote's current default instead.
    WrongBranch {
        url: String,
        branch: String,
        error: String,
    },
}

impl RepoOutcome {
    /// Whether `main/` didn't end up as the backup describes it.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::WrongBranch { .. })
    }
}

#[derive(Debug, Clone)]
pub struct ProjectReport {
    pub name: String,
    pub label: Option<String>,
    pub target: PathBuf,
    /// `None` when the project failed before the clone step.
    pub repo: Option<RepoOutcome>,
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
    /// Issues on the replaced local board that the backup doesn't have.
    pub dropped_issues: Vec<String>,
    /// Where the replaced local board was saved, when there was one.
    pub previous_board: Option<PathBuf>,
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
            repo: None,
            board_restored: false,
            issues: 0,
            cleared_worktrees: Vec::new(),
            files: Vec::new(),
            plans: Vec::new(),
            kept_plans: Vec::new(),
            kept_files: Vec::new(),
            dropped_issues: Vec::new(),
            previous_board: None,
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
    Restored {
        path: PathBuf,
        /// Where the replaced config was saved, when `--force` replaced a
        /// different one.
        previous: Option<PathBuf>,
    },
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
    let mut restored_dirs = HashMap::new();
    for (project, target) in selected.iter().zip(&targets) {
        let mut report = ProjectReport::new(project, target);
        // Targets don't exist yet in a dry run, so `claim_target` can't
        // compare them; flag the likely case-only collisions instead.
        if opts.dry_run {
            let folded = target.to_string_lossy().to_lowercase();
            if let Some(other) = targets
                .iter()
                .take_while(|t| *t != target)
                .find(|t| t.to_string_lossy().to_lowercase() == folded)
            {
                report.warnings.push(format!(
                    "may be the same directory as {} on a case-insensitive disk; the real run would refuse it",
                    other.display()
                ));
            }
        }
        // One broken project shouldn't hide what happened to the others.
        // Archive-side checks first, so a bad entry leaves no empty target.
        let result = project_src(dir, project).and_then(|src| {
            claim_target(target, opts.dry_run, &mut restored_dirs)?;
            restore_project(&src, project, target, opts, now, &mut report)
        });
        if let Err(err) = result {
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
        // Exact paths only; names a file system treats as one directory
        // (case, Unicode normalization) are caught by `claim_target`.
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
    // An unreadable registry must stop the import: registering would
    // otherwise replace it with just the restored projects.
    let registry = crate::global_config::try_list_projects()?;
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

/// Create `target` and refuse it when an earlier project of this import
/// already landed in the same directory. File systems can treat different
/// spellings as one name (case, Unicode normalization on macOS), which the
/// string checks up front can't fully see.
fn claim_target(
    target: &Path,
    dry_run: bool,
    restored_dirs: &mut HashMap<(u64, u64), PathBuf>,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    // Every check below the target starts at the target; a link here would
    // send the whole restore somewhere else.
    if fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symlink; restore to a real directory",
            target.display()
        );
    }
    if fs::symlink_metadata(target.join(".git")).is_ok() {
        bail!(
            "{} is a git checkout, not a bork project container",
            target.display()
        );
    }
    if !dry_run {
        fs::create_dir_all(target).with_context(|| {
            format!(
                "creating {} (pass --root to restore somewhere else)",
                target.display()
            )
        })?;
    }
    let Ok(meta) = fs::metadata(target) else {
        return Ok(());
    };
    if let Some(other) = restored_dirs.insert((meta.dev(), meta.ino()), target.to_path_buf()) {
        bail!(
            "{} is the same directory as {}, restored just before; use --project to pick one",
            target.display(),
            other.display()
        );
    }
    Ok(())
}

/// The project's directory in the extracted archive, refusing names that
/// could leave it and symlinks that could point anywhere.
fn project_src(extracted: &Path, project: &ProjectManifest) -> anyhow::Result<PathBuf> {
    if !is_plain_component(&project.archive_dir) {
        bail!("invalid archive dir {:?}", project.archive_dir);
    }
    let rel = Path::new(PROJECTS_DIR).join(&project.archive_dir);
    if has_symlinked_parent(extracted, &rel.join("x")) {
        bail!(
            "projects/{} in the backup is a symlink",
            project.archive_dir
        );
    }
    Ok(extracted.join(rel))
}

/// Restore one project from its extracted archive dir `src` into `target`.
fn restore_project(
    src: &Path,
    project: &ProjectManifest,
    target: &Path,
    opts: &Options,
    now: u64,
    report: &mut ProjectReport,
) -> anyhow::Result<()> {
    report.repo = Some(restore_repo(
        project,
        target,
        opts.dry_run,
        &mut report.warnings,
    ));
    // Read once: it decides checkouts, sessions and the pre-import copy.
    let local = local_board(target);
    if local.is_none() && fs::metadata(target.join(STATE_FILE)).is_ok_and(|m| m.is_file()) {
        report.warnings.push(
            "the board at the target is unreadable; if replaced it's kept as .bork/state.json.corrupt"
                .to_string(),
        );
    }
    restore_files(src, project, target, local.as_ref(), opts.dry_run, report)?;
    restore_board(
        src,
        project,
        target,
        local.as_ref(),
        opts.dry_run,
        now,
        report,
    )?;
    restore_plans(src, project, target, opts.dry_run, report)?;

    if !opts.dry_run {
        // Same guards as every other write under the target.
        let status_rel = Path::new(".bork/agent-status/x");
        if has_symlinked_parent(target, status_rel)
            || checkout_ancestor(target, status_rel).is_some()
        {
            report.warnings.push(
                "didn't create .bork/agent-status/: .bork/ is a symlink or checkout".to_string(),
            );
        } else {
            crate::config::ensure_agent_status_dir(target);
        }
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

/// Container files from the manifest, except the board and worktree plans.
fn restore_files(
    src: &Path,
    project: &ProjectManifest,
    target: &Path,
    local: Option<&AppState>,
    dry_run: bool,
    report: &mut ProjectReport,
) -> anyhow::Result<()> {
    // Checkouts at the target (main/, worktrees, anything with a `.git`),
    // even ones that were plain container dirs at backup time: archived
    // files must never land on uncommitted work.
    let empty = AppState::default();
    let checkouts = super::select::worktree_dirs(target, &[], local.unwrap_or(&empty));
    for rel in &project.files {
        if rel == STATE_FILE || rel.starts_with(&format!("{ISSUES_DIR}/")) {
            continue;
        }
        let Some(rel_path) = plain_relative(rel) else {
            report.warnings.push(format!("skipped unsafe path {rel:?}"));
            continue;
        };
        let top = rel.split('/').next().unwrap_or_default();
        let checkout = if checkouts.contains(top) {
            Some(PathBuf::from(top))
        } else {
            checkout_ancestor(target, &rel_path)
        };
        if let Some(dir) = checkout {
            report.warnings.push(format!(
                "skipped {rel}: {}/ is a git checkout at the target",
                dir.display()
            ));
            continue;
        }
        // A live worktree's plan is newer than the copy saved in `plans/`;
        // `restore_plans` places it instead.
        let superseded = rel
            .strip_prefix("plans/")
            .and_then(|rest| rest.strip_suffix(&format!("/{PLAN_NAME}")))
            .is_some_and(|id| worktree_plan_available(src, project, id));
        if superseded {
            continue;
        }
        // Plans are the work itself; a local copy is newer than the backup.
        if rel_path.starts_with("plans") && fs::symlink_metadata(target.join(&rel_path)).is_ok() {
            report.kept_files.push(rel.clone());
            continue;
        }
        if copy_reported(src, rel, target, &rel_path, dry_run, report)? {
            report.files.push(rel.clone());
        }
    }
    Ok(())
}

/// The board, rewritten for a machine without the worktrees (see
/// [`rewrite_state`]). A missing or unreadable board is a warning.
fn restore_board(
    src: &Path,
    project: &ProjectManifest,
    target: &Path,
    local: Option<&AppState>,
    dry_run: bool,
    now: u64,
    report: &mut ProjectReport,
) -> anyhow::Result<()> {
    let state_rel = Path::new(STATE_FILE);
    let archived = fs::symlink_metadata(src.join(state_rel)).is_ok_and(|m| m.is_file())
        && !has_symlinked_parent(src, state_rel);
    if !archived {
        report
            .warnings
            .push("the backup has no board for this project".to_string());
        return Ok(());
    }
    if has_symlinked_parent(target, state_rel) {
        report
            .warnings
            .push("skipped the board: .bork/ at the target is a symlink".to_string());
        return Ok(());
    }
    if let Some(dir) = checkout_ancestor(target, state_rel) {
        report.warnings.push(format!(
            "skipped the board: {}/ is a git checkout at the target",
            dir.display()
        ));
        return Ok(());
    }
    // Saving would replace the link with a plain file and cut the board
    // loose from wherever it really lives.
    if fs::symlink_metadata(target.join(state_rel)).is_ok_and(|m| m.file_type().is_symlink()) {
        report.warnings.push(
            "kept the board: .bork/state.json is a symlink at the target; restore it by hand if needed"
                .to_string(),
        );
        return Ok(());
    }
    let Some(mut state) = crate::config::try_load_state(src) else {
        report
            .warnings
            .push("the board in the backup is unreadable; skipped it".to_string());
        return Ok(());
    };
    // Only the board being replaced can vouch for a session still existing.
    let branches: HashMap<&str, &str> = project
        .issues
        .iter()
        .filter_map(|i| Some((i.id.as_str(), i.branch.as_deref()?)))
        .collect();
    report.cleared_worktrees = rewrite_state(&mut state, target, local, now)
        .into_iter()
        .map(|id| {
            let branch = branches.get(id.as_str()).map(|b| b.to_string());
            (id, branch)
        })
        .collect();
    report.issues = state.issues.len();
    report.board_restored = true;
    if let Some(local) = local {
        report.dropped_issues = local
            .issues
            .iter()
            .filter(|old| !state.issues.iter().any(|i| i.id == old.id))
            .map(|old| old.id.clone())
            .collect();
        // A mistaken --force over a live project stays recoverable. Each
        // replaced board gets its own file, and a re-run that changes nothing
        // saves nothing, so a retry can't bury the first copy.
        let unchanged = serde_json::to_value(local)? == serde_json::to_value(&state)?;
        if !unchanged {
            let previous = unused_path(&target.join(format!(".bork/state.json.pre-import-{now}")));
            if !dry_run {
                fs::copy(target.join(state_rel), &previous)
                    .with_context(|| format!("saving the old board to {}", previous.display()))?;
            }
            report.previous_board = Some(previous);
        }
    }
    if !dry_run {
        crate::config::save_state(&state, target)?;
    }
    Ok(())
}

/// The closest directory between `target` and `target/rel` that holds a
/// `.git` entry (a repo or worktree), relative to `target`.
fn checkout_ancestor(target: &Path, rel: &Path) -> Option<PathBuf> {
    let mut dir = PathBuf::new();
    for part in rel.parent()?.components() {
        dir.push(part);
        if fs::symlink_metadata(target.join(&dir).join(".git")).is_ok() {
            return Some(dir);
        }
    }
    None
}

/// `path`, or `path-2`, `path-3`, ... for the first one that doesn't exist.
fn unused_path(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    let mut n = 2;
    while fs::symlink_metadata(&candidate).is_ok() {
        candidate = PathBuf::from(format!("{}-{n}", path.display()));
        n += 1;
    }
    candidate
}

/// The board at `target`, when it has a readable one.
fn local_board(target: &Path) -> Option<AppState> {
    fs::symlink_metadata(target.join(STATE_FILE))
        .is_ok_and(|m| m.is_file())
        .then(|| crate::config::try_load_state(target))
        .flatten()
}

/// Whether the archive really holds the worktree plan for issue `id`. When it
/// doesn't, the copy saved in `plans/<id>/` is the one to restore.
fn worktree_plan_available(src: &Path, project: &ProjectManifest, id: &str) -> bool {
    let rel = format!("{ISSUES_DIR}/{id}/{PLAN_NAME}");
    project.files.contains(&rel)
        && fs::symlink_metadata(src.join(&rel)).is_ok_and(|m| m.is_file())
        && !has_symlinked_parent(src, Path::new(&rel))
}

/// `_issues/<id>/planning.md` to `plans/<id>/planning.md`, keeping any plan
/// already there.
fn restore_plans(
    src: &Path,
    project: &ProjectManifest,
    target: &Path,
    dry_run: bool,
    report: &mut ProjectReport,
) -> anyhow::Result<()> {
    for rel in &project.files {
        let parts: Vec<&str> = rel.split('/').collect();
        let [ISSUES_DIR, id, PLAN_NAME] = parts[..] else {
            continue;
        };
        if !is_plain_component(id) {
            continue;
        }
        if !worktree_plan_available(src, project, id) {
            let saved = format!("plans/{id}/{PLAN_NAME}");
            let fallback = if project.files.contains(&saved) {
                "; using the plan saved in plans/ instead"
            } else {
                ""
            };
            report
                .warnings
                .push(format!("{rel} is missing from the backup{fallback}"));
            continue;
        }
        let dest_rel = Path::new("plans").join(id).join(PLAN_NAME);
        if fs::symlink_metadata(target.join(&dest_rel)).is_ok() {
            report.kept_plans.push(id.to_string());
            continue;
        }
        if let Some(dir) = checkout_ancestor(target, &dest_rel) {
            report.warnings.push(format!(
                "skipped {rel}: {}/ is a git checkout at the target",
                dir.display()
            ));
            continue;
        }
        if copy_reported(src, rel, target, &dest_rel, dry_run, report)? {
            report.plans.push(id.to_string());
        }
    }
    Ok(())
}

/// [`copy_entry`] with a skipped copy turned into a report warning. Returns
/// whether the file was (or would be) copied.
fn copy_reported(
    src: &Path,
    rel: &str,
    target: &Path,
    dest_rel: &Path,
    dry_run: bool,
    report: &mut ProjectReport,
) -> anyhow::Result<bool> {
    let warning = match copy_entry(src, Path::new(rel), target, dest_rel, dry_run)? {
        Copied::Yes => return Ok(true),
        Copied::Missing => format!("{rel} is missing from the backup"),
        Copied::Unsafe => format!("skipped {rel}: a directory on its path is a symlink"),
        Copied::KeptLink => format!("kept {rel}: it's a symlink at the target"),
    };
    report.warnings.push(warning);
    Ok(false)
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
    // Same choice as `RepoFacts::repo_manifest`: origin, else the first remote.
    let cloned_from = project
        .repo
        .remotes
        .iter()
        .find(|r| r.name == "origin" && r.url == url)
        .or_else(|| project.repo.remotes.iter().find(|r| r.url == url));
    if let Some(remote) = cloned_from.filter(|r| r.name != "origin" && !r.name.starts_with('-')) {
        if !git_ok(&["remote", "rename", "origin", &remote.name]) {
            warnings.push(format!("could not rename remote origin to {}", remote.name));
        }
    }
    let cloned_name = cloned_from.map_or("origin", |r| r.name.as_str());
    for remote in &project.repo.remotes {
        if remote.name == cloned_name {
            continue;
        }
        if remote.name.starts_with('-') {
            warnings.push(format!(
                "skipped remote {:?}: not a valid name",
                remote.name
            ));
            continue;
        }
        if !git_ok(&["remote", "add", "--", &remote.name, &remote.url]) {
            warnings.push(format!(
                "could not add remote {} ({})",
                remote.name, remote.url
            ));
        }
    }
    if let Some(branch) = &branch {
        if let Err(err) = checkout(&main, branch) {
            return RepoOutcome::WrongBranch {
                url,
                branch: branch.clone(),
                error: format!("{err:#}"),
            };
        }
    }
    RepoOutcome::Cloned { url, branch }
}

/// Check out `branch` in `repo` unless it's already the current branch.
fn checkout(repo: &Path, branch: &str) -> anyhow::Result<()> {
    if branch.starts_with('-') {
        bail!("{branch:?} is not a valid branch name");
    }
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
/// when `local` (the board being replaced at the target) holds the same id
/// for that issue and agent: transcripts aren't backed up, and OpenCode can't
/// tell a dead session from a live one. Everything else is kept. Returns the
/// ids whose worktree was cleared.
pub fn rewrite_state(
    state: &mut AppState,
    target: &Path,
    local: Option<&AppState>,
    now: u64,
) -> Vec<String> {
    let mut cleared = Vec::new();
    for issue in &mut state.issues {
        let local_issue = local.and_then(|board| board.issues.iter().find(|i| i.id == issue.id));
        let gone = issue
            .worktree
            .as_deref()
            .is_some_and(|w| !is_plain_component(w) || !target.join(w).is_dir());
        if gone {
            issue.worktree = None;
            // A re-run keeps the first import's timestamp.
            issue.pruned_at = local_issue.and_then(|i| i.pruned_at).or(Some(now));
            issue.setup_ran = false;
            issue.sessions.clear();
            cleared.push(issue.id.clone());
            continue;
        }
        issue
            .sessions
            .retain(|agent, id| local_issue.is_some_and(|i| i.sessions.get(agent) == Some(&*id)));
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
    /// The destination is a symlink (dotfiles, say) and was left alone.
    KeptLink,
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
    // Replacing a symlink at the target would cut it loose from what it
    // links to. The same link as in the backup is already restored.
    if fs::symlink_metadata(dest).is_ok_and(|m| m.file_type().is_symlink()) {
        let same_link =
            file_type.is_symlink() && fs::read_link(dest).ok() == fs::read_link(&src).ok();
        return Ok(if same_link {
            Copied::Yes
        } else {
            Copied::KeptLink
        });
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
    // Like the board: a config that --force replaces stays recoverable.
    let source = extracted.join(&rel);
    let replaced = fs::symlink_metadata(&dest).is_ok_and(|m| m.is_file())
        && fs::read(&dest).ok() != fs::read(&source).ok();
    let previous = replaced.then(|| {
        unused_path(&dir.join(format!(
            "{GLOBAL_CONFIG}.pre-import-{}",
            crate::app::unix_now()
        )))
    });
    if let (Some(previous), false) = (&previous, opts.dry_run) {
        if !has_symlinked_parent(extracted, Path::new(&rel))
            && fs::symlink_metadata(&source).is_ok_and(|m| m.is_file())
        {
            fs::copy(&dest, previous)
                .with_context(|| format!("saving the old config to {}", previous.display()))?;
        }
    }
    match copy_entry(
        extracted,
        Path::new(&rel),
        &dir,
        Path::new(GLOBAL_CONFIG),
        opts.dry_run,
    )? {
        Copied::Yes => Ok(GlobalConfigOutcome::Restored {
            path: dest,
            previous,
        }),
        Copied::KeptLink => Ok(GlobalConfigOutcome::KeptExisting(dest)),
        Copied::Missing | Copied::Unsafe => Ok(GlobalConfigOutcome::Skipped),
    }
}

impl Report {
    /// Whether at least one project was restored without an error.
    pub fn any_restored(&self) -> bool {
        self.projects.iter().any(|p| p.error.is_none())
    }

    /// Whether any project failed part-way or couldn't be cloned.
    pub fn failed(&self) -> bool {
        self.projects
            .iter()
            .any(|p| p.error.is_some() || p.repo.as_ref().is_some_and(RepoOutcome::is_failure))
            || matches!(self.global_config, GlobalConfigOutcome::Failed(_))
    }

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

            let repo = p.repo.as_ref().map(|repo| match repo {
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
                RepoOutcome::WrongBranch { url, branch, error } => format!(
                    "cloned {url}, but couldn't check out {branch} ({error}); main/ is on the remote's default branch"
                ),
            });
            if let Some(repo) = repo {
                let _ = writeln!(out, "  repo:     {repo}");
            }

            if p.board_restored {
                let _ = writeln!(
                    out,
                    "  board:    {} {}",
                    would("restored", "would restore"),
                    count(p.issues, "issue")
                );
            }
            if !p.dropped_issues.is_empty() {
                let _ = writeln!(
                    out,
                    "  board:    {} not in the backup: {}",
                    would("dropped", "would drop"),
                    p.dropped_issues.join(", ")
                );
            }
            if let Some(previous) = &p.previous_board {
                let _ = writeln!(
                    out,
                    "  board:    old board {} {}",
                    would("saved to", "would be saved to"),
                    previous.display()
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
            if p.repo.is_some() {
                let _ = writeln!(
                    out,
                    "  files:    {} {}",
                    would("restored", "would restore"),
                    count(p.files.len(), "container file")
                );
            }
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
            GlobalConfigOutcome::Restored { path, previous } => {
                let _ = writeln!(
                    out,
                    "global config: {} {}",
                    would("restored", "would restore"),
                    path.display()
                );
                if let Some(previous) = previous {
                    let _ = writeln!(
                        out,
                        "global config: old one {} {}",
                        would("saved to", "would be saved to"),
                        previous.display()
                    );
                }
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
                captured: None,
            })
            .collect();
        let file = dir.join("crafted.tar.gz");
        let contents = super::super::archive::load(&entries).unwrap();
        super::super::archive::write(&file, manifest.to_string().as_bytes(), &contents).unwrap();
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

        let mut local = state.clone();
        // A session the local board doesn't know is dead.
        local.issues[2].sessions.clear();
        local.issues[2]
            .sessions
            .insert(AgentKind::Claude, "other".to_string());
        let cleared = rewrite_state(&mut state, &target, Some(&local), 42);

        assert_eq!(cleared, vec!["demo-1".to_string()]);
        let gone = &state.issues[0];
        assert_eq!(gone.worktree, None);
        assert_eq!(gone.pruned_at, Some(42));
        assert!(!gone.setup_ran);
        assert!(gone.sessions.is_empty());
        assert_eq!(gone.prompt.as_deref(), Some("prompt demo-1"));
        assert_eq!(gone.column, Column::InProgress);
        assert_eq!(gone.linked_issues, vec!["demo-9".to_string()]);
        // Sessions the local board still has survive.
        assert_eq!(state.issues[1].worktree.as_deref(), Some("demo-2-b"));
        assert!(state.issues[1].setup_ran);
        assert_eq!(state.issues[1].sessions.len(), 1);
        assert!(state.issues[2].sessions.is_empty());

        // A fresh restore: every session goes, transcripts aren't backed up.
        let mut fresh = board(&[("demo-2", Some("demo-2-b")), ("demo-3", None)]);
        assert!(rewrite_state(&mut fresh, &target, None, 42).is_empty());
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
            assert_eq!(p.repo, Some(RepoOutcome::NoCloneUrl));
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
                GlobalConfigOutcome::Restored { .. }
            ));
            assert!(
                read(&crate::global_config::global_config_dir().join("config.toml"))
                    .contains("[backup]")
            );
        });
    }

    #[test]
    fn a_forced_rerun_does_not_bring_back_dead_sessions() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            import(&file, &opts(dest.path())).unwrap();
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            import(&file, &force).unwrap();
            let state = crate::config::load_state(&dest.path().join("demo"));
            assert!(state.issues.iter().all(|i| i.sessions.is_empty()));
        });
    }

    #[test]
    fn warns_about_an_unreadable_local_board() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(&dest.path().join("demo/.bork/state.json"), "{ truncated");
        crate::global_config::with_test_config_home(home.path(), || {
            let force = Options {
                force: true,
                dry_run: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            let p = &report.projects[0];
            assert!(
                p.warnings.iter().any(|w| w.contains("state.json.corrupt")),
                "{:?}",
                p.warnings
            );
        });
    }

    #[test]
    fn dry_run_flags_targets_that_differ_only_in_case() {
        let dir = tempfile::tempdir().unwrap();
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([
                project_json("Demo", "a", &[]),
                project_json("demo", "b", &[])
            ]),
            &[],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let dry = Options {
                dry_run: true,
                ..opts(dest.path())
            };
            let report = import(&file, &dry).unwrap();
            assert!(report.projects[0]
                .warnings
                .iter()
                .all(|w| !w.contains("case-insensitive")));
            assert!(
                report.projects[1]
                    .warnings
                    .iter()
                    .any(|w| w.contains("case-insensitive")),
                "{:?}",
                report.projects[1].warnings
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
        // A local board with an issue created after the backup.
        crate::config::save_state(
            &board(&[("demo-1", Some("demo-1-a")), ("demo-7", None)]),
            &target,
        )
        .unwrap();
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
            let mut first_saved = None;
            for run in 0..2 {
                let report = import(&file, &force).unwrap();
                let p = &report.projects[0];
                if run == 0 {
                    assert_eq!(p.dropped_issues, vec!["demo-7".to_string()]);
                    let saved = p.previous_board.clone().unwrap();
                    assert!(saved.starts_with(target.join(".bork")));
                    assert!(read(&saved).contains("demo-7"));
                    first_saved = Some(saved);
                } else {
                    // Nothing changed, so nothing is saved over the first copy.
                    assert!(p.dropped_issues.is_empty());
                    assert_eq!(p.previous_board, None);
                    assert!(read(first_saved.as_ref().unwrap()).contains("demo-7"));
                }
                assert_eq!(p.repo, Some(RepoOutcome::KeptExisting));
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
            // --force also replaces the global config, keeping the old one.
            assert!(read(&global).contains("[backup]"));
            let saved: Vec<_> = fs::read_dir(global.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.to_string_lossy().contains("config.toml.pre-import-"))
                .collect();
            assert_eq!(saved.len(), 1, "{saved:?}");
            assert_eq!(read(&saved[0]), "mine = true\n");
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
            let text = report.render();
            assert!(text.contains("stopped here"), "{text}");
            // It failed before the clone step, so nothing claims a missing remote,
            // and no empty target is left behind to need --force next time.
            assert_eq!(report.projects[0].repo, None);
            let broken = text.split("\n\n").next().unwrap();
            assert!(!broken.contains("repo:"), "{broken}");
            assert!(!dest.path().join("broken").exists());
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
                project_json("demo", "a", &[]),
                // Recorded at the same path as the first one.
                project_json("demo", "b", &[]),
            ]),
            &[],
        );
        let home = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let err = import(&file, &Options::default()).unwrap_err();
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
    fn names_one_file_system_treats_as_equal_never_share_a_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.md"), dir.path().join("b.md"));
        write(&a, "first");
        write(&b, "second");
        let (composed, decomposed) = ("caf\u{e9}", "cafe\u{301}");
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([
                project_json(composed, "cafe", &["AGENTS.md"]),
                project_json(decomposed, "cafe-2", &["AGENTS.md"]),
            ]),
            &[
                ("projects/cafe/AGENTS.md", &a),
                ("projects/cafe-2/AGENTS.md", &b),
            ],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let report = import(&file, &opts(dest.path())).unwrap();
            // macOS sees one directory and refuses the second; elsewhere both
            // restore side by side. Either way the first is never overwritten.
            assert_eq!(read(&dest.path().join(composed).join("AGENTS.md")), "first");
            if report.projects[1].error.is_none() {
                assert_eq!(
                    read(&dest.path().join(decomposed).join("AGENTS.md")),
                    "second"
                );
            } else {
                assert!(report.projects[1]
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("same directory"));
            }
        });
    }

    #[test]
    fn falls_back_to_the_saved_plan_when_the_worktree_plan_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let saved = dir.path().join("saved.md");
        write(&saved, "saved plan");
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([project_json(
                "demo",
                "demo",
                &["plans/demo-1/planning.md", "_issues/demo-1/planning.md"]
            )]),
            &[("projects/demo/plans/demo-1/planning.md", &saved)],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let dry = import(
                &file,
                &Options {
                    dry_run: true,
                    ..opts(dest.path())
                },
            )
            .unwrap();
            let report = import(&file, &opts(dest.path())).unwrap();
            assert_eq!(
                read(&dest.path().join("demo/plans/demo-1/planning.md")),
                "saved plan"
            );
            // Dry run and real run tell the same story.
            for p in [&dry.projects[0], &report.projects[0]] {
                assert!(p.kept_plans.is_empty());
                assert_eq!(p.files, vec!["plans/demo-1/planning.md".to_string()]);
                assert!(
                    p.warnings
                        .iter()
                        .any(|w| w.contains("using the plan saved")),
                    "{:?}",
                    p.warnings
                );
            }
        });
    }

    #[test]
    fn never_writes_into_a_checkout_at_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.md");
        write(&notes, "old container notes");
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([project_json("demo", "demo", &["scratch/README.md"])]),
            &[("projects/demo/scratch/README.md", &notes)],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let scratch = dest.path().join("demo/scratch");
        write(&scratch.join(".git"), "gitdir: elsewhere");
        write(&scratch.join("README.md"), "uncommitted work");
        crate::global_config::with_test_config_home(home.path(), || {
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            assert_eq!(read(&scratch.join("README.md")), "uncommitted work");
            let p = &report.projects[0];
            assert!(
                p.warnings.iter().any(|w| w.contains("git checkout")),
                "{:?}",
                p.warnings
            );
        });
    }

    #[test]
    fn never_writes_into_a_nested_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.md");
        write(&notes, "backup text");
        let file = crafted_archive(
            dir.path(),
            serde_json::json!([project_json("demo", "demo", &["docs/tool/README.md"])]),
            &[("projects/demo/docs/tool/README.md", &notes)],
        );
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let tool = dest.path().join("demo/docs/tool");
        fs::create_dir_all(tool.join(".git")).unwrap();
        write(&tool.join("README.md"), "uncommitted local work");
        crate::global_config::with_test_config_home(home.path(), || {
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            assert_eq!(read(&tool.join("README.md")), "uncommitted local work");
            let p = &report.projects[0];
            assert!(
                p.warnings
                    .iter()
                    .any(|w| w.contains("docs/tool/ is a git checkout")),
                "{:?}",
                p.warnings
            );
        });
    }

    #[test]
    fn leaves_checkouts_at_bork_and_plans_alone() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let target = dest.path().join("demo");
        fs::create_dir_all(target.join(".bork/.git")).unwrap();
        fs::create_dir_all(target.join("plans/.git")).unwrap();
        crate::config::save_state(&board(&[("demo-7", None)]), &target).unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            let p = &report.projects[0];
            assert!(!p.board_restored);
            assert!(p.plans.is_empty());
            assert_eq!(crate::config::load_state(&target).issues[0].id, "demo-7");
            assert!(!target.join("plans/demo-1/planning.md").exists());
            assert!(!target.join("plans/demo-2/planning.md").exists());
        });
    }

    #[test]
    fn refuses_a_symlinked_or_checkout_target() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        write(&elsewhere.path().join("AGENTS.md"), "someone else's");
        std::os::unix::fs::symlink(elsewhere.path(), dest.path().join("demo")).unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            let error = report.projects[0].error.clone().unwrap_or_default();
            assert!(error.contains("symlink"), "{error}");
            assert_eq!(read(&elsewhere.path().join("AGENTS.md")), "someone else's");

            fs::remove_file(dest.path().join("demo")).unwrap();
            fs::create_dir_all(dest.path().join("demo/.git")).unwrap();
            let report = import(&file, &force).unwrap();
            let error = report.projects[0].error.clone().unwrap_or_default();
            assert!(error.contains("git checkout"), "{error}");
            assert!(!dest.path().join("demo/AGENTS.md").exists());
        });
    }

    #[test]
    fn an_unreadable_registry_stops_the_import() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let registry = crate::global_config::global_config_dir().join("projects.json");
            write(&registry, "{ not json");
            let err = import(&file, &opts(dest.path())).unwrap_err();
            assert!(format!("{err:#}").contains("projects.json"), "{err:#}");
            assert_eq!(read(&registry), "{ not json");
            assert!(!dest.path().join("demo").exists());
        });
    }

    #[test]
    fn force_keeps_symlinked_files_at_the_target() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let dotfiles = tempfile::tempdir().unwrap();
        let agents = dotfiles.path().join("AGENTS.md");
        let global = dotfiles.path().join("config.toml");
        write(&agents, "dotfiles agents");
        write(&global, "dotfiles = true\n");
        let target = dest.path().join("demo");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&agents, target.join("AGENTS.md")).unwrap();
        // The backup has CLAUDE.md -> AGENTS.md; the target links elsewhere.
        std::os::unix::fs::symlink("local-rules.md", target.join("CLAUDE.md")).unwrap();
        let board = dotfiles.path().join("state.json");
        write(&board, r#"{"issues":[]}"#);
        fs::create_dir_all(target.join(".bork")).unwrap();
        std::os::unix::fs::symlink(&board, target.join(".bork/state.json")).unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let global_link = crate::global_config::global_config_dir().join("config.toml");
            fs::create_dir_all(global_link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&global, &global_link).unwrap();
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            let p = &report.projects[0];
            assert!(
                p.warnings.iter().any(|w| w.contains("kept AGENTS.md")),
                "{:?}",
                p.warnings
            );
            assert!(fs::symlink_metadata(target.join("AGENTS.md"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(read(&agents), "dotfiles agents");
            assert_eq!(
                fs::read_link(target.join("CLAUDE.md")).unwrap(),
                PathBuf::from("local-rules.md")
            );
            assert!(!p.board_restored);
            assert!(fs::symlink_metadata(target.join(".bork/state.json"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(read(&board), r#"{"issues":[]}"#);
            assert!(matches!(
                report.global_config,
                GlobalConfigOutcome::KeptExisting(_)
            ));
            assert!(fs::symlink_metadata(&global_link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(read(&global), "dotfiles = true\n");
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
            // Same URL, different name: still a remote of its own.
            git(
                &main,
                &["remote", "add", "mirror", origin.to_str().unwrap()],
            );
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
                Some(RepoOutcome::Cloned {
                    url: origin.to_str().unwrap().to_string(),
                    branch: Some("trunk".to_string()),
                })
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
            for name in ["fork", "mirror"] {
                assert_eq!(
                    git_out(&target.join("main"), &["remote", "get-url", name]),
                    origin.to_str().unwrap()
                );
            }
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
    fn a_missing_default_branch_is_reported_as_a_failure() {
        let work = tempfile::tempdir().unwrap();
        let seed = work.path().join("seed");
        let origin = work.path().join("origin.git");
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "trunk"]);
        git(&seed, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(
            work.path(),
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        let mut project = project_json("demo", "demo", &[]);
        project["repo"] = serde_json::json!({
            "clone_url": origin.to_str().unwrap(),
            "remotes": [{"name": "origin", "url": origin.to_str().unwrap()}],
            "default_branch": "old-default",
        });
        let file = crafted_archive(work.path(), serde_json::json!([project]), &[]);
        let home = work.path().join("home");
        let dest = work.path().join("dest");
        crate::global_config::with_test_config_home(&home, || {
            let report = import(&file, &opts(&dest)).unwrap();
            let p = &report.projects[0];
            assert!(
                matches!(&p.repo, Some(RepoOutcome::WrongBranch { branch, .. }) if branch == "old-default"),
                "{:?}",
                p.repo
            );
            assert!(report.failed());
            assert!(report.render().contains("couldn't check out old-default"));
            assert_eq!(
                git_out(&dest.join("demo/main"), &["branch", "--show-current"]),
                "trunk"
            );
        });
    }

    #[test]
    fn nothing_is_created_through_a_symlinked_bork_dir() {
        let out = tempfile::tempdir().unwrap();
        let file = demo_archive(out.path());
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = dest.path().join("demo");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(outside.path(), target.join(".bork")).unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let force = Options {
                force: true,
                ..opts(dest.path())
            };
            let report = import(&file, &force).unwrap();
            assert!(!report.projects[0].board_restored);
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
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
            assert!(
                matches!(p.repo, Some(RepoOutcome::Failed { .. })),
                "{:?}",
                p.repo
            );
            assert!(report.render().contains("clone it into main/ yourself"));
            // A project without its code isn't usable: the CLI exits non-zero.
            assert!(report.failed());
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
