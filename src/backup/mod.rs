//! Backups: one `.tar.gz` file holding the boards, container docs and issue
//! plans of every opted-in project, plus the global config, so a wiped
//! machine can be restored with `bork import`.
//!
//! `bork backup` writes `<path>/bork-backup.tar.gz` (all opted-in projects
//! and `_global/`); `bork backup --project <name>` writes
//! `<path>/bork-backup-<name>.tar.gz` with just that project. The archive
//! layout and manifest are documented in [`manifest`]. A run whose content
//! matches the last archive leaves the file untouched.
//!
//! `bork import <file>` ([`import`]) restores an archive.
//!
//! Backups run once a day automatically (the TUI worker checks [`is_due`])
//! and whenever `bork backup` is called. The CLI is a thin layer over
//! [`plan`] and [`run`]; the worker and the worktree removal paths
//! ([`save_issue_plan`]) call the same functions.

pub mod archive;
pub mod config;
pub mod import;
pub mod manifest;
pub mod select;
pub mod status;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use self::archive::ArchiveEntry;
use self::manifest::{Manifest, ProjectManifest, GLOBAL_DIR, MANIFEST_VERSION, PROJECTS_DIR};
use self::select::{ISSUES_DIR, PLAN_FILE};
use self::status::ArchiveStatus;
use crate::config::AppState;

/// File name of the full backup.
pub const BACKUP_FILE: &str = "bork-backup.tar.gz";

/// How long a successful full backup counts as current for [`is_due`].
pub const BACKUP_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// Global files worth restoring; `bork.pid`, locks and caches are not.
const GLOBAL_FILES: &[&str] = &["projects.json", "config.toml"];

/// File name of an issue plan, in the archive and in the container's `plans/<id>/`.
const PLAN_NAME: &str = "planning.md";

/// What a backup run covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Every registered, opted-in project plus the global files, written to
    /// [`BACKUP_FILE`].
    All,
    /// One opted-in project (its container root), written to
    /// `bork-backup-<project_name>.tar.gz`.
    Project(PathBuf),
}

/// Everything a run would write, resolved but not yet written.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Absolute path of the backup file.
    pub file: PathBuf,
    /// Manifest with `created_at` still 0; set when writing.
    pub manifest: Manifest,
    pub entries: Vec<ArchiveEntry>,
}

/// Result of a successful run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub file: PathBuf,
    pub size: u64,
    /// False when the content matched the last archive and the file was left alone.
    pub written: bool,
    pub projects: Vec<String>,
}

/// Resolve what a run would write. `Ok(None)` when [`Selection::All`] finds
/// no opted-in project.
pub fn plan(selection: &Selection) -> anyhow::Result<Option<Plan>> {
    let settings = config::load_settings();
    let Some(dir) = settings.path else {
        bail!(
            "no backup path configured (set `path` under [backup] in ~/.config/bork/config.toml)"
        );
    };
    if !dir.is_absolute() {
        bail!(
            "backup path {} must be absolute (or start with ~/)",
            dir.display()
        );
    }
    // Resolve symlinks and `..` so every spelling of the same folder maps to
    // one file (status records and ownership are keyed by it).
    let dir = resolve_through_existing(&dir);

    let registry = crate::global_config::try_list_projects()?;
    let labels: HashMap<PathBuf, Option<String>> = registry
        .iter()
        .map(|p| {
            (
                crate::global_config::normalize_path(&p.path),
                p.label.clone(),
            )
        })
        .collect();

    let (roots, file) = match selection {
        Selection::All => {
            let mut seen = HashSet::new();
            let mut roots: Vec<PathBuf> = Vec::new();
            for entry in &registry {
                let root = crate::global_config::normalize_path(&entry.path);
                if seen.insert(root.clone()) && config::project_enabled(&root)? {
                    roots.push(root);
                }
            }
            if roots.is_empty() {
                return Ok(None);
            }
            (roots, dir.join(BACKUP_FILE))
        }
        Selection::Project(root) => {
            let root = crate::global_config::normalize_path(root);
            if !config::project_enabled(&root)? {
                bail!(
                    "backups are not enabled for {} (set `enabled = true` under [backup] in .bork/config.toml)",
                    root.display()
                );
            }
            // Same-named projects (or names differing only in case, on a
            // case-insensitive disk) share a file; never let one replace
            // another's single-project backup. The archive's own manifest is
            // the source of truth, not the status file.
            let file = project_backup_file(&dir, &root)?;
            check_single_project_owner(&file, &root)?;
            (vec![root], file)
        }
    };

    // The backup must not land inside a project: it would be archived into
    // the next backup of that project.
    for root in roots.iter().chain(labels.keys()) {
        if dir.starts_with(root) {
            bail!(
                "backup path {} is inside the project at {}; pick a path outside it",
                dir.display(),
                root.display()
            );
        }
    }

    let mut projects = Vec::new();
    let mut entries = Vec::new();
    let mut used_dirs = HashSet::new();
    for root in &roots {
        let label = labels.get(root).cloned().flatten();
        let (project, mut project_entries) = project_part(root, label, &mut used_dirs)?;
        projects.push(project);
        entries.append(&mut project_entries);
    }

    let mut global_files = Vec::new();
    if *selection == Selection::All {
        let global_dir = crate::global_config::global_config_dir();
        for name in GLOBAL_FILES {
            let source = global_dir.join(name);
            if select::is_existing_file(&source)? {
                let path = format!("{GLOBAL_DIR}/{name}");
                global_files.push(path.clone());
                entries.push(ArchiveEntry {
                    path,
                    source,
                    follow: true,
                    captured: None,
                });
            }
        }
    }

    Ok(Some(Plan {
        file,
        manifest: Manifest {
            version: MANIFEST_VERSION,
            created_at: 0,
            bork_version: env!("CARGO_PKG_VERSION").to_string(),
            projects,
            global_files,
        },
        entries,
    }))
}

/// Write the backup for `selection` and record the result in
/// `~/.config/bork/backup.json`. `Ok(None)` when there is nothing to back up.
pub fn run(selection: &Selection) -> anyhow::Result<Option<Outcome>> {
    let _lock = lock_backups()?;
    let plan = match plan(selection) {
        Ok(Some(plan)) => plan,
        Ok(None) => return Ok(None),
        Err(err) => {
            if let Some(file) = failure_file(selection) {
                record_failure(&file, &err);
            }
            return Err(err);
        }
    };
    let result = write_plan(&plan);
    match &result {
        Ok((outcome, fingerprint)) => {
            let record = ArchiveStatus {
                last_run_at: crate::app::unix_now(),
                ok: true,
                size: outcome.size,
                projects: outcome.projects.clone(),
                paths: plan
                    .manifest
                    .projects
                    .iter()
                    .map(|p| p.path.clone())
                    .collect(),
                fingerprint: Some(fingerprint.clone()),
                changed: outcome.written,
                ..ArchiveStatus::default()
            };
            if let Err(err) = status::record(&status::status_path(), &plan.file, record) {
                eprintln!("bork: warning: could not record backup status: {err:#}");
            }
        }
        Err(err) => record_failure(&plan.file, err),
    }
    result.map(|(outcome, _)| Some(outcome))
}

/// Whether the daily automatic backup should run at `now` (unix seconds):
/// true when `~/.config/bork/backup.json` has no successful full backup
/// ([`BACKUP_FILE`] at the configured path) or the last one is at least
/// [`BACKUP_MAX_AGE_SECS`] old. Explicit `bork backup` calls run regardless.
#[cfg_attr(not(test), allow(dead_code))] // Used by the TUI worker in a follow-up
pub fn is_due(now: u64) -> bool {
    let file = full_backup_file();
    is_due_at(&status::read(&status::status_path()), file.as_deref(), now)
}

fn is_due_at(records: &status::StatusFile, file: Option<&Path>, now: u64) -> bool {
    let last_success = file
        .and_then(|file| records.archives.get(file))
        .and_then(|record| record.last_success_at);
    match last_success {
        Some(at) => now.saturating_sub(at) >= BACKUP_MAX_AGE_SECS,
        None => true,
    }
}

/// Copy an issue worktree's `.claude/planning.md` into the container's
/// `plans/<issue_id>/planning.md`, so the plan survives the worktree being
/// removed (and lands in the next backup with the rest of `plans/`). Meant
/// for the archive/delete/prune paths, right before removal.
///
/// Returns `Ok(false)` when the project has backups off or the worktree has
/// no plan, `Ok(true)` once the plan is copied.
#[cfg_attr(not(test), allow(dead_code))] // Wired into the removal paths in a follow-up
pub fn save_issue_plan(
    project_root: &Path,
    issue_id: &str,
    worktree_dir: &str,
) -> anyhow::Result<bool> {
    if !config::project_enabled(project_root)? {
        return Ok(false);
    }
    if !select::is_plain_component(issue_id) || !select::is_plain_component(worktree_dir) {
        bail!("invalid issue id {issue_id:?} or worktree {worktree_dir:?}");
    }
    let source = project_root.join(worktree_dir).join(PLAN_FILE);
    let bytes = match fs::read(&source) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("reading {}", source.display())),
    };
    let dir = project_root.join("plans").join(issue_id);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::config::atomic_write(&dir.join(PLAN_NAME), &bytes)?;
    Ok(true)
}

/// Read the board (symlinks followed) as bytes and mode. `None` when the
/// project has no board yet; other read errors fail.
fn read_board(path: &Path) -> anyhow::Result<Option<(Vec<u8>, u32)>> {
    use std::os::unix::fs::PermissionsExt;
    let context = || format!("reading {}", path.display());
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(context),
    };
    let meta = file.metadata().with_context(context)?;
    if !meta.is_file() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).with_context(context)?;
    Ok(Some((bytes, meta.permissions().mode() & 0o777)))
}

/// ASCII-only directory name for a project in the archive: characters
/// outside `[A-Za-z0-9._-]` become `_`.
fn archive_dir_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || safe.chars().all(|c| c == '.') {
        "project".to_string()
    } else {
        safe
    }
}

/// Archive files and manifest entry for one project. `used_dirs` keeps
/// archive directories unique when registered projects share a name.
fn project_part(
    root: &Path,
    label: Option<String>,
    used_dirs: &mut HashSet<String>,
) -> anyhow::Result<(ProjectManifest, Vec<ArchiveEntry>)> {
    let name = crate::config::load_config_from(root).project_name;
    let base = archive_dir_name(&name);
    // ASCII and compared case-insensitively, so names stay distinct on any
    // disk (case- or Unicode-normalization-insensitive ones included).
    let mut archive_dir = base.clone();
    let mut n = 2;
    while !used_dirs.insert(archive_dir.to_lowercase()) {
        archive_dir = format!("{base}-{n}");
        n += 1;
    }
    let prefix = format!("{PROJECTS_DIR}/{archive_dir}");

    // A corrupt board fails the run (keeping the previous backup) instead of
    // reading as empty, which would drop every issue and plan from the new
    // file. A project without a board yet backs up everything else.
    // The board is read once: the same bytes pick the plans, build the
    // manifest and go into the archive, so a board saved mid-run can't leave
    // them out of step.
    let board_rel = ".bork/state.json";
    let board_bytes = read_board(&root.join(board_rel))?;
    let board: AppState = match &board_bytes {
        Some((bytes, _)) => serde_json::from_slice(bytes)
            .with_context(|| format!("{} can't be parsed", root.join(board_rel).display()))?,
        None => AppState::default(),
    };
    let facts = manifest::RepoFacts::gather(root);
    let worktrees = select::worktree_dirs(root, &facts.worktree_paths(), &board);

    let mut files = Vec::new();
    let mut entries = Vec::new();
    for rel in select::select_files(root, &worktrees)? {
        let Some(rel_str) = rel.to_str().map(String::from) else {
            continue;
        };
        if rel_str == board_rel {
            continue;
        }
        entries.push(ArchiveEntry {
            path: format!("{prefix}/{rel_str}"),
            follow: select::BORK_FILES.contains(&rel_str.as_str()),
            source: root.join(&rel),
            captured: None,
        });
        files.push(rel_str);
    }

    if let Some(captured) = board_bytes {
        entries.push(ArchiveEntry {
            path: format!("{prefix}/{board_rel}"),
            source: root.join(board_rel),
            follow: true,
            captured: Some(captured),
        });
        files.push(board_rel.to_string());
    }

    let mut plans = HashMap::new();
    for (issue_id, source) in select::issue_plans(root, &board)? {
        let rel = format!("{ISSUES_DIR}/{issue_id}/{PLAN_NAME}");
        let path = format!("{prefix}/{rel}");
        plans.insert(issue_id, path.clone());
        entries.push(ArchiveEntry {
            path,
            source,
            follow: true,
            captured: None,
        });
        files.push(rel);
    }

    let project = ProjectManifest {
        issues: manifest::issue_manifests(root, &board, &facts, &plans),
        repo: facts.repo_manifest(),
        name,
        label,
        path: root.to_path_buf(),
        archive_dir,
        files,
    };
    Ok((project, entries))
}

/// Write the archive unless its content matches the last one written to the
/// same file. Returns the outcome and the content fingerprint.
fn write_plan(plan: &Plan) -> anyhow::Result<(Outcome, String)> {
    // `created_at` is still 0 here, so the fingerprint only tracks content.
    let contents = archive::load(&plan.entries)?;
    let fingerprint = archive::fingerprint(&serde_json::to_vec(&plan.manifest)?, &contents);
    let projects: Vec<String> = plan
        .manifest
        .projects
        .iter()
        .map(|p| p.name.clone())
        .collect();

    let previous = status::read(&status::status_path());
    if let Some(prev) = previous.archives.get(&plan.file) {
        let on_disk = fs::metadata(&plan.file).map(|m| m.len()).ok();
        if prev.fingerprint.as_deref() == Some(fingerprint.as_str()) && on_disk == Some(prev.size) {
            let outcome = Outcome {
                file: plan.file.clone(),
                size: prev.size,
                written: false,
                projects,
            };
            return Ok((outcome, fingerprint));
        }
    }

    let mut manifest = plan.manifest.clone();
    manifest.created_at = crate::app::unix_now();
    let mut json = serde_json::to_vec_pretty(&manifest)?;
    json.push(b'\n');
    let size = archive::write(&plan.file, &json, &contents)?;
    let outcome = Outcome {
        file: plan.file.clone(),
        size,
        written: true,
        projects,
    };
    Ok((outcome, fingerprint))
}

/// Refuse to replace an existing single-project archive that belongs to
/// another project, or whose owner can't be read from its manifest.
fn check_single_project_owner(file: &Path, root: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(file) {
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", file.display())),
        Ok(_) => {}
    }
    let manifest = archive::read_manifest(file).with_context(|| {
        format!(
            "{} exists but its owner can't be read; move it away to back up {} there",
            file.display(),
            root.display()
        )
    })?;
    let owners: Vec<&Path> = manifest.projects.iter().map(|p| p.path.as_path()).collect();
    if owners != [root] {
        let owners: Vec<String> = owners.iter().map(|p| p.display().to_string()).collect();
        bail!(
            "{} holds the backup of {}; give {} a different project_name, or move the old file away if this is the same project in a new place",
            file.display(),
            owners.join(", "),
            root.display()
        );
    }
    Ok(())
}

/// `<dir>/bork-backup-<project_name>.tar.gz` for a `--project` run. The
/// only place that name is built, so planning and status always agree.
fn project_backup_file(dir: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    let name = crate::config::load_config_from(root).project_name;
    if !select::is_plain_component(&name) {
        bail!("project name {name:?} can't be used in a file name");
    }
    Ok(dir.join(format!("bork-backup-{name}.tar.gz")))
}

/// Where a failed planning step is recorded: the full backup's file, or the
/// project's own `--project` file. Nothing is recorded for a project that
/// isn't opted in (it has no backup file to report on), or when the file
/// belongs to another project, so its healthy record stays intact.
fn failure_file(selection: &Selection) -> Option<PathBuf> {
    match selection {
        Selection::All => full_backup_file(),
        Selection::Project(root) => {
            let root = crate::global_config::normalize_path(root);
            if !config::project_enabled(&root).unwrap_or(false) {
                return None;
            }
            let dir = resolve_through_existing(&config::load_settings().path?);
            let file = project_backup_file(&dir, &root).ok()?;
            check_single_project_owner(&file, &root).ok()?;
            Some(file)
        }
    }
}

/// The full backup's file, when the backup path is configured.
fn full_backup_file() -> Option<PathBuf> {
    let dir = config::load_settings().path?;
    Some(resolve_through_existing(&dir).join(BACKUP_FILE))
}

fn record_failure(file: &Path, err: &anyhow::Error) {
    let record = ArchiveStatus {
        last_run_at: crate::app::unix_now(),
        ok: false,
        error: Some(format!("{err:#}")),
        ..ArchiveStatus::default()
    };
    if let Err(write_err) = status::record(&status::status_path(), file, record) {
        eprintln!("bork: warning: could not record backup status: {write_err:#}");
    }
}

/// Canonicalize the longest existing prefix of `path` and apply the rest
/// lexically (`..` pops, `.` is dropped), so symlinks and `..` resolve even
/// for folders that don't exist yet.
fn resolve_through_existing(path: &Path) -> PathBuf {
    use std::path::Component;
    let parts: Vec<Component> = path.components().collect();
    for split in (1..=parts.len()).rev() {
        let prefix: PathBuf = parts[..split].iter().collect();
        let Ok(mut resolved) = prefix.canonicalize() else {
            continue;
        };
        let mut popped = false;
        for part in &parts[split..] {
            match part {
                Component::Normal(name) => resolved.push(name),
                Component::ParentDir => {
                    resolved.pop();
                    popped = true;
                }
                _ => {}
            }
        }
        // `..` can step back into existing folders (and their symlinks), so
        // resolve once more; the result has no `..` left, so this terminates.
        if popped {
            return resolve_through_existing(&resolved);
        }
        return resolved;
    }
    path.to_path_buf()
}

/// Serialize backup runs machine-wide (CLI and the TUI worker can overlap).
/// Held until the returned file is dropped.
fn lock_backups() -> anyhow::Result<File> {
    let dir = crate::global_config::global_config_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join("backup.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    // SAFETY: fd is a valid descriptor owned by `file` for the call's duration.
    if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("locking {}", path.display()));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppState;
    use crate::types::{AgentKind, Column, Issue};
    use std::process::Command;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn save_board(root: &Path, issues: &[(&str, Option<&str>)]) {
        let mut state = AppState::default();
        for (id, worktree) in issues {
            let mut issue = Issue::new(*id, "t", Column::InProgress, AgentKind::Claude);
            issue.worktree = worktree.map(String::from);
            state.issues.push(issue);
        }
        crate::config::save_state(&state, root).unwrap();
    }

    /// A registered project named `name`, opted in when `enabled`.
    fn project(name: &str, enabled: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".bork/config.toml"),
            &format!("project_name = \"{name}\"\n[backup]\nenabled = {enabled}\n"),
        );
        crate::global_config::register_project(name, dir.path()).unwrap();
        dir
    }

    fn set_global_path(path: &Path) {
        write(
            &crate::global_config::global_config_dir().join("config.toml"),
            &format!("[backup]\npath = \"{}\"\n", path.display()),
        );
    }

    fn list(archive: &Path) -> Vec<String> {
        let out = Command::new("tar")
            .arg("-tzf")
            .arg(archive)
            .output()
            .unwrap();
        let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.ends_with('/'))
            .map(String::from)
            .collect();
        names.sort();
        names
    }

    fn read_manifest(archive: &Path) -> Manifest {
        let out = Command::new("tar")
            .arg("-xzOf")
            .arg(archive)
            .arg("manifest.json")
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    }

    #[test]
    fn backs_up_opted_in_projects_into_one_file() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            let _off = project("off", false);
            let root = demo.path();
            save_board(root, &[("demo-1", Some("demo-1-a")), ("demo-2", None)]);
            write(&root.join(".bork/github-cache.json"), "{}");
            write(&root.join("AGENTS.md"), "agents");
            write(&root.join("main/README.md"), "code");
            write(&root.join("demo-1-a/.git"), "gitdir: x");
            write(&root.join("demo-1-a/src/lib.rs"), "code");
            write(&root.join("demo-1-a/.claude/planning.md"), "plan");

            let outcome = run(&Selection::All).unwrap().unwrap();

            let file = resolve_through_existing(&dest.path().join(BACKUP_FILE));
            assert_eq!(outcome.file, file);
            assert!(outcome.written);
            assert_eq!(outcome.projects, vec!["demo".to_string()]);
            assert_eq!(
                list(&file),
                vec![
                    "_global/config.toml",
                    "_global/projects.json",
                    "manifest.json",
                    "projects/demo/.bork/config.toml",
                    "projects/demo/.bork/state.json",
                    "projects/demo/AGENTS.md",
                    "projects/demo/_issues/demo-1/planning.md",
                ]
            );
            let manifest = read_manifest(&file);
            assert_eq!(manifest.version, MANIFEST_VERSION);
            assert!(manifest.created_at > 0);
            assert_eq!(manifest.projects.len(), 1);
            let p = &manifest.projects[0];
            assert_eq!(p.archive_dir, "demo");
            assert_eq!(p.issues.len(), 2);
            assert_eq!(
                p.issues[0].plan.as_deref(),
                Some("projects/demo/_issues/demo-1/planning.md")
            );
            assert_eq!(manifest.global_files.len(), 2);

            let recorded = &status::read(&status::status_path()).archives[&file];
            assert!(recorded.ok);
            assert_eq!(recorded.size, outcome.size);
        });
    }

    #[test]
    fn unchanged_content_skips_the_write() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            write(&demo.path().join("AGENTS.md"), "v1");

            assert!(run(&Selection::All).unwrap().unwrap().written);
            let second = run(&Selection::All).unwrap().unwrap();
            assert!(!second.written);
            assert!(!status::read(&status::status_path()).archives[&second.file].changed);

            write(&demo.path().join("AGENTS.md"), "v2");
            assert!(run(&Selection::All).unwrap().unwrap().written);

            // A deleted backup file is rewritten even when nothing changed.
            fs::remove_file(dest.path().join(BACKUP_FILE)).unwrap();
            assert!(run(&Selection::All).unwrap().unwrap().written);
        });
    }

    #[test]
    fn single_project_archive() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            let other = project("other", true);
            write(&demo.path().join("AGENTS.md"), "a");
            write(&other.path().join("AGENTS.md"), "b");

            let outcome = run(&Selection::Project(demo.path().to_path_buf()))
                .unwrap()
                .unwrap();

            assert_eq!(
                outcome.file,
                resolve_through_existing(&dest.path().join("bork-backup-demo.tar.gz"))
            );
            assert_eq!(
                list(&outcome.file),
                vec![
                    "manifest.json",
                    "projects/demo/.bork/config.toml",
                    "projects/demo/AGENTS.md",
                ]
            );
            assert!(read_manifest(&outcome.file).global_files.is_empty());
            assert!(!dest.path().join(BACKUP_FILE).exists());

            let off = project("off", false);
            assert!(run(&Selection::Project(off.path().to_path_buf())).is_err());
            let off_file = resolve_through_existing(&dest.path().join("bork-backup-off.tar.gz"));
            assert!(!status::read(&status::status_path())
                .archives
                .contains_key(&off_file));
        });
    }

    #[test]
    fn same_named_projects_get_separate_dirs() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let a = project("demo", true);
            let b = project("demo", true);
            write(&a.path().join("AGENTS.md"), "a");
            write(&b.path().join("AGENTS.md"), "b");
            let outcome = run(&Selection::All).unwrap().unwrap();
            let dirs: HashSet<String> = read_manifest(&outcome.file)
                .projects
                .into_iter()
                .map(|p| p.archive_dir)
                .collect();
            assert_eq!(
                dirs,
                HashSet::from(["demo".to_string(), "demo-2".to_string()])
            );
        });
    }

    #[test]
    fn nothing_opted_in_is_a_no_op() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let _off = project("off", false);
            assert_eq!(run(&Selection::All).unwrap(), None);
        });
        assert!(!dest.path().join(BACKUP_FILE).exists());
    }

    #[test]
    fn refuses_bad_paths_and_records_failures() {
        let home = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let demo = project("demo", true);
            assert!(run(&Selection::All).is_err(), "no path configured");

            write(
                &crate::global_config::global_config_dir().join("config.toml"),
                "[backup]\npath = \"relative\"\n",
            );
            assert!(run(&Selection::All).is_err());

            set_global_path(&demo.path().join("backups"));
            let err = run(&Selection::All).unwrap_err();
            assert!(format!("{err:#}").contains("inside the project"), "{err:#}");
            assert!(!demo.path().join("backups").exists());

            let file = resolve_through_existing(&demo.path().join("backups").join(BACKUP_FILE));
            let recorded = &status::read(&status::status_path()).archives[&file];
            assert!(!recorded.ok);
        });
    }

    #[test]
    fn failed_read_keeps_the_previous_archive() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            let private = demo.path().join("private.md");
            write(&private, "v1");
            let first = run(&Selection::All).unwrap().unwrap();

            write(&private, "v2");
            fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
            let readable = fs::read(&private).is_ok(); // true when running as root
            let result = run(&Selection::All);
            fs::set_permissions(&private, fs::Permissions::from_mode(0o644)).unwrap();
            if readable {
                return;
            }
            assert!(result.is_err());
            assert_eq!(fs::metadata(&first.file).unwrap().len(), first.size);
            let recorded = &status::read(&status::status_path()).archives[&first.file];
            assert!(!recorded.ok);
            assert_eq!(recorded.size, first.size);
        });
    }

    #[test]
    fn due_daily_after_the_last_success() {
        let file = Path::new("/b/bork-backup.tar.gz");
        let mut records = status::StatusFile::default();
        assert!(is_due_at(&records, Some(file), 1_000));
        assert!(is_due_at(&records, None, 1_000));

        let success = ArchiveStatus {
            last_run_at: 1_000,
            last_success_at: Some(1_000),
            ok: true,
            ..ArchiveStatus::default()
        };
        records.archives.insert(file.to_path_buf(), success);
        assert!(!is_due_at(&records, Some(file), 1_000));
        assert!(!is_due_at(
            &records,
            Some(file),
            1_000 + BACKUP_MAX_AGE_SECS - 1
        ));
        assert!(is_due_at(&records, Some(file), 1_000 + BACKUP_MAX_AGE_SECS));

        // A later failure doesn't reset the clock; a single-project file doesn't count.
        let failed = ArchiveStatus {
            last_run_at: 2_000,
            ok: false,
            ..records.archives[file].clone()
        };
        records.archives.insert(file.to_path_buf(), failed);
        assert!(!is_due_at(&records, Some(file), 2_000));
        assert!(is_due_at(
            &records,
            Some(Path::new("/b/other.tar.gz")),
            2_000
        ));
    }

    #[test]
    fn is_due_reads_the_status_file() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            write(&demo.path().join("AGENTS.md"), "a");
            assert!(is_due(crate::app::unix_now()));
            run(&Selection::All).unwrap();
            let success = status::read(&status::status_path()).archives
                [&resolve_through_existing(&dest.path().join(BACKUP_FILE))]
                .last_success_at
                .unwrap();
            assert!(!is_due(success));
            assert!(!is_due(success + BACKUP_MAX_AGE_SECS - 1));
            assert!(is_due(success + BACKUP_MAX_AGE_SECS));
        });
    }

    /// Make `path` unreadable for the duration of `check`, restoring it
    /// after. Skips the check when permissions don't apply (root).
    fn with_unreadable(path: &Path, check: impl FnOnce()) {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path).unwrap().permissions().mode();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = if path.is_dir() {
            fs::read_dir(path).is_err()
        } else {
            fs::read(path).is_err()
        };
        if blocked {
            check();
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn unreadable_inputs_fail_instead_of_shrinking_the_backup() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let alpha = project("alpha", true);
            let beta = project("beta", true);
            let root = alpha.path();
            save_board(root, &[("alpha-1", Some("alpha-1-task"))]);
            write(&root.join("alpha-1-task/.git"), "gitdir: x");
            write(&root.join("alpha-1-task/.claude/planning.md"), "plan");
            write(&beta.path().join("AGENTS.md"), "b");
            let first = run(&Selection::All).unwrap().unwrap();
            let keeps_previous = |result: anyhow::Result<Option<Outcome>>| {
                assert!(result.is_err());
                assert_eq!(fs::metadata(&first.file).unwrap().len(), first.size);
            };

            with_unreadable(&root.join(".bork/config.toml"), || {
                keeps_previous(run(&Selection::All));
            });
            with_unreadable(&root.join("alpha-1-task/.claude"), || {
                keeps_previous(run(&Selection::All));
            });

            // A symlink loop is an error, not a missing board.
            let state = root.join(".bork/state.json");
            fs::rename(&state, root.join(".bork/state.json.bak")).unwrap();
            std::os::unix::fs::symlink(&state, &state).unwrap();
            keeps_previous(run(&Selection::All));
        });
    }

    #[test]
    fn single_project_file_is_not_taken_over_by_a_same_named_project() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let first = project("demo", true);
            let second = project("demo", true);
            write(&first.path().join("AGENTS.md"), "1");
            write(&second.path().join("AGENTS.md"), "2");

            let outcome = run(&Selection::Project(first.path().to_path_buf()))
                .unwrap()
                .unwrap();
            let err = run(&Selection::Project(second.path().to_path_buf())).unwrap_err();
            assert!(
                format!("{err:#}").contains("holds the backup of"),
                "{err:#}"
            );
            // The refusal doesn't mark the owner's healthy backup as failed.
            assert!(status::read(&status::status_path()).archives[&outcome.file].ok);
            let manifest = read_manifest(&outcome.file);
            assert_eq!(
                manifest.projects[0].path,
                crate::global_config::normalize_path(first.path())
            );
            // The owner can still update it.
            write(&first.path().join("AGENTS.md"), "1b");
            assert!(
                run(&Selection::Project(first.path().to_path_buf()))
                    .unwrap()
                    .unwrap()
                    .written
            );
        });
    }

    #[test]
    fn resolves_symlinks_and_parent_dirs_through_missing_folders() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().canonicalize().unwrap();
        fs::create_dir(real.join("dest")).unwrap();
        std::os::unix::fs::symlink(real.join("dest"), real.join("alias")).unwrap();
        assert_eq!(
            resolve_through_existing(&real.join("alias")),
            real.join("dest")
        );
        assert_eq!(
            resolve_through_existing(&real.join("alias/new/sub")),
            real.join("dest/new/sub")
        );
        assert_eq!(
            resolve_through_existing(&real.join("outside/new/../../alias/x")),
            real.join("dest/x")
        );
    }

    #[test]
    fn path_spellings_cannot_bypass_checks() {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let base = scratch.path().canonicalize().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let first = project("demo", true);
            let second = project("demo", true);
            write(&first.path().join("AGENTS.md"), "1");
            fs::create_dir(base.join("dest")).unwrap();
            std::os::unix::fs::symlink(base.join("dest"), base.join("alias")).unwrap();

            set_global_path(&base.join("dest"));
            run(&Selection::Project(first.path().to_path_buf())).unwrap();
            set_global_path(&base.join("alias"));
            assert!(run(&Selection::Project(second.path().to_path_buf())).is_err());

            // `..` through missing folders into a project is still "inside".
            let first_root = crate::global_config::normalize_path(first.path());
            let rel = format!(
                "{}/outside/new/../../{}/backups",
                first_root.parent().unwrap().display(),
                first_root.file_name().unwrap().to_str().unwrap()
            );
            write(
                &crate::global_config::global_config_dir().join("config.toml"),
                &format!("[backup]\npath = \"{rel}\"\n"),
            );
            let err = run(&Selection::All).unwrap_err();
            assert!(format!("{err:#}").contains("inside the project"), "{err:#}");
        });
    }

    #[test]
    fn unreadable_registry_fails_instead_of_backing_up_nothing() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            write(&demo.path().join("AGENTS.md"), "a");
            run(&Selection::All).unwrap();
            let registry = crate::global_config::global_config_dir().join("projects.json");
            with_unreadable(&registry, || {
                assert!(run(&Selection::All).is_err());
                let file = resolve_through_existing(&dest.path().join(BACKUP_FILE));
                assert!(!status::read(&status::status_path()).archives[&file].ok);
            });
            write(&registry, "{ not json");
            assert!(run(&Selection::All).is_err());
        });
    }

    #[test]
    fn ownership_comes_from_the_archive_not_the_status_file() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let first = project("demo", true);
            let second = project("demo", true);
            write(&first.path().join("AGENTS.md"), "1");
            let outcome = run(&Selection::Project(first.path().to_path_buf()))
                .unwrap()
                .unwrap();

            fs::remove_file(status::status_path()).unwrap();
            let err = run(&Selection::Project(second.path().to_path_buf())).unwrap_err();
            assert!(
                format!("{err:#}").contains("holds the backup of"),
                "{err:#}"
            );

            // An existing file without a readable manifest is never replaced.
            write(&outcome.file, "not a tarball");
            let err = run(&Selection::Project(first.path().to_path_buf())).unwrap_err();
            assert!(format!("{err:#}").contains("can't be read"), "{err:#}");
            assert_eq!(fs::read_to_string(&outcome.file).unwrap(), "not a tarball");
        });
    }

    #[test]
    fn names_differing_in_case_get_separate_dirs() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let upper = project("Demo", true);
            let lower = project("demo", true);
            write(&upper.path().join("AGENTS.md"), "first");
            write(&lower.path().join("AGENTS.md"), "second");
            let outcome = run(&Selection::All).unwrap().unwrap();
            let manifest = read_manifest(&outcome.file);
            let dirs: HashSet<String> = manifest
                .projects
                .iter()
                .map(|p| p.archive_dir.to_lowercase())
                .collect();
            assert_eq!(dirs.len(), 2);
            let agents: Vec<String> = list(&outcome.file)
                .into_iter()
                .filter(|p| p.ends_with("/AGENTS.md"))
                .collect();
            assert_eq!(agents.len(), 2, "{agents:?}");
        });
    }

    #[test]
    fn archive_dirs_are_ascii_and_distinct() {
        assert_eq!(archive_dir_name("bork"), "bork");
        assert_eq!(archive_dir_name("Caf\u{e9}"), "Caf_");
        assert_eq!(archive_dir_name("Cafe\u{301}"), "Cafe_");
        assert_eq!(archive_dir_name(".."), "project");
        assert_eq!(archive_dir_name("a/b c"), "a_b_c");

        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let a = project("Caf\u{e9}", true);
            let b = project("Cafe\u{301}", true);
            write(&a.path().join("AGENTS.md"), "first");
            write(&b.path().join("AGENTS.md"), "second");
            let outcome = run(&Selection::All).unwrap().unwrap();
            let agents = list(&outcome.file)
                .into_iter()
                .filter(|p| p.ends_with("/AGENTS.md"))
                .count();
            assert_eq!(agents, 2);
        });
    }

    #[test]
    fn container_entry_aliasing_issues_dir_never_writes_outside() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            let root = demo.path();
            save_board(root, &[("demo-1", Some("demo-1-task"))]);
            write(&root.join("demo-1-task/.git"), "gitdir: x");
            write(&root.join("demo-1-task/.claude/planning.md"), "live plan");
            write(&outside.path().join("demo-1/planning.md"), "outside");
            std::os::unix::fs::symlink(outside.path(), root.join("_ISSUES")).unwrap();

            // On a case-insensitive disk this collides and fails; on a
            // case-sensitive one both entries are archived. Either way the
            // outside file is untouched.
            if let Ok(Some(outcome)) = run(&Selection::All) {
                assert!(list(&outcome.file)
                    .contains(&"projects/demo/_issues/demo-1/planning.md".to_string()));
            }
        });
        assert_eq!(
            fs::read_to_string(outside.path().join("demo-1/planning.md")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn corrupt_board_fails_and_keeps_the_previous_archive() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            let root = demo.path();
            save_board(root, &[("demo-1", Some("demo-1-a"))]);
            write(&root.join("demo-1-a/.git"), "gitdir: x");
            write(&root.join("demo-1-a/.claude/planning.md"), "plan");
            let first = run(&Selection::All).unwrap().unwrap();

            write(&root.join(".bork/state.json"), "{ corrupt");
            let err = run(&Selection::All).unwrap_err();
            assert!(format!("{err:#}").contains("state.json"), "{err:#}");
            assert!(
                list(&first.file).contains(&"projects/demo/_issues/demo-1/planning.md".to_string())
            );
            assert_eq!(fs::metadata(&first.file).unwrap().len(), first.size);
            assert!(!status::read(&status::status_path()).archives[&first.file].ok);
        });
    }

    #[test]
    fn board_is_archived_from_the_bytes_that_were_parsed() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            let root = demo.path();
            save_board(root, &[("demo-1", None)]);
            let plan = plan(&Selection::All).unwrap().unwrap();
            let board = plan
                .entries
                .iter()
                .find(|e| e.path == "projects/demo/.bork/state.json")
                .unwrap();
            let (bytes, _) = board.captured.as_ref().unwrap();
            assert_eq!(bytes, &fs::read(root.join(".bork/state.json")).unwrap());
            assert_eq!(plan.manifest.projects[0].issues.len(), 1);

            // A board saved after planning doesn't reach this archive.
            save_board(root, &[("demo-1", None), ("demo-2", None)]);
            let contents = archive::load(&plan.entries).unwrap();
            let file = dest.path().join("snapshot.tar.gz");
            archive::write(&file, b"{}", &contents).unwrap();
            let out = std::process::Command::new("tar")
                .arg("-xzOf")
                .arg(&file)
                .arg("projects/demo/.bork/state.json")
                .output()
                .unwrap();
            assert_eq!(&out.stdout, bytes);
        });
    }

    #[test]
    fn failed_project_run_is_recorded_on_its_own_file() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let demo = project("demo", true);
            save_board(demo.path(), &[("demo-1", None)]);
            let outcome = run(&Selection::Project(demo.path().to_path_buf()))
                .unwrap()
                .unwrap();
            write(&demo.path().join(".bork/state.json"), "{ corrupt");
            assert!(run(&Selection::Project(demo.path().to_path_buf())).is_err());
            let record = &status::read(&status::status_path()).archives[&outcome.file];
            assert!(!record.ok);
            assert_eq!(record.size, outcome.size);
        });
    }

    #[test]
    fn unusable_project_name_fails_without_a_status_row() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            set_global_path(dest.path());
            let odd = project("a/b", true);
            let err = run(&Selection::Project(odd.path().to_path_buf())).unwrap_err();
            assert!(
                format!("{err:#}").contains("can't be used in a file name"),
                "{err:#}"
            );
            assert!(status::read(&status::status_path()).archives.is_empty());
        });
    }

    #[test]
    fn save_issue_plan_copies_into_container_plans() {
        let home = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            let demo = project("demo", true);
            let root = demo.path();
            write(&root.join("demo-2-b/.claude/planning.md"), "the plan");

            assert!(save_issue_plan(root, "demo-2", "demo-2-b").unwrap());
            assert_eq!(
                fs::read_to_string(root.join("plans/demo-2/planning.md")).unwrap(),
                "the plan"
            );
            assert!(!save_issue_plan(root, "demo-3", "no-such-dir").unwrap());
            assert!(save_issue_plan(root, "../x", "demo-2-b").is_err());

            let off = project("off", false);
            write(&off.path().join("wt/.claude/planning.md"), "p");
            assert!(!save_issue_plan(off.path(), "off-1", "wt").unwrap());
            assert!(!off.path().join("plans").exists());
        });
    }
}
