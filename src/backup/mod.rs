//! Project backups: board state, non-git container files and a restore
//! manifest, copied to a folder or a git clone.
//!
//! Library entry points (`run_project`, `run_global`, `run_all`, `plan_*`)
//! are shared by the `bork backup` CLI and the background worker. Each
//! project writes to `<path>/<project_name>/`, the global files to
//! `<path>/_global/`.

pub mod config;
pub mod git_dest;
pub mod manifest;
pub mod mirror;
pub mod select;
pub mod status;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use self::config::{BackupConfig, Destination};
use self::git_dest::PushOutcome;
use self::mirror::{Entry, MirrorReport, Source};
use self::status::BackupStatus;

/// Destination subdirectory for the global files. Leading underscore keeps it
/// clear of project names.
pub const GLOBAL_SCOPE: &str = "_global";

/// Global files worth restoring; `bork.pid`, locks and caches are not.
const GLOBAL_FILES: &[&str] = &["projects.json", "config.toml"];

/// Everything a backup run would write, resolved but not yet executed.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Destination subdirectory name: the project name, or [`GLOBAL_SCOPE`].
    pub scope: String,
    pub config: BackupConfig,
    /// Destination root (the folder, or the git clone).
    pub root: PathBuf,
    pub entries: Vec<Entry>,
}

impl Plan {
    pub fn target(&self) -> PathBuf {
        self.root.join(&self.scope)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub report: MirrorReport,
    /// Git mode only: whether a commit was made.
    pub committed: bool,
    /// Git mode only.
    pub push: Option<PushOutcome>,
}

/// Resolve what a project backup would write. Fails when the project has not
/// opted in or has no destination path.
pub fn plan_project(project_root: &Path) -> anyhow::Result<Plan> {
    let root = crate::global_config::normalize_path(project_root);
    let backup = config::load_for_project(&root);
    if !backup.enabled {
        bail!(
            "backups are not enabled for {} (set `enabled = true` under [backup] in .bork/config.toml)",
            root.display()
        );
    }
    let Some(dest_root) = backup.path.clone() else {
        bail!("no backup path configured (set `path` under [backup])");
    };

    let app_config = crate::config::load_config_from(&root);
    let scope = app_config.project_name.clone();
    validate_scope(&scope)?;

    // A corrupt state.json is still copied as-is; it just can't name worktrees.
    let state = crate::config::try_load_state(&root).unwrap_or_default();
    let facts = manifest::RepoFacts::gather(&root);
    let git_paths: Vec<PathBuf> = facts.worktrees.iter().map(|w| w.path.clone()).collect();
    let worktrees = select::worktree_dirs(&root, &git_paths, &state);

    let mut entries: Vec<Entry> = select::select_files(&root, &worktrees)
        .into_iter()
        .map(|rel| Entry {
            source: Source::Path(root.join(&rel)),
            rel,
        })
        .collect();
    let manifest = manifest::build(&scope, &root, &facts, &state);
    let mut json = serde_json::to_vec_pretty(&manifest)?;
    json.push(b'\n');
    entries.push(Entry {
        rel: PathBuf::from(manifest::MANIFEST_FILE),
        source: Source::Bytes(json),
    });

    Ok(Plan {
        scope,
        config: backup,
        root: dest_root,
        entries,
    })
}

/// Resolve the global files backup, or `None` when the global `[backup]`
/// section has no path.
pub fn plan_global() -> Option<Plan> {
    let backup = BackupConfig::resolve_global(&config::load_global_layer());
    if !backup.enabled {
        return None;
    }
    let dest_root = backup.path.clone()?;
    let dir = crate::global_config::global_config_dir();
    let entries = GLOBAL_FILES
        .iter()
        .filter(|name| dir.join(name).is_file())
        .map(|name| Entry {
            rel: PathBuf::from(name),
            source: Source::Path(dir.join(name)),
        })
        .collect();
    Some(Plan {
        scope: GLOBAL_SCOPE.to_string(),
        config: backup,
        root: dest_root,
        entries,
    })
}

/// Write a plan to its destination.
pub fn execute(plan: &Plan) -> anyhow::Result<RunOutcome> {
    let _lock = lock_backups();
    match plan.config.destination {
        Destination::Folder => {
            let report = mirror::mirror(&plan.entries, &plan.target())?;
            Ok(RunOutcome {
                report,
                committed: false,
                push: None,
            })
        }
        Destination::Git => {
            git_dest::prepare(&plan.root)?;
            let report = mirror::mirror(&plan.entries, &plan.target())?;
            let message = format!(
                "backup: {} {}",
                plan.scope,
                format_utc(crate::app::unix_now())
            );
            let committed = git_dest::commit_subdir(&plan.root, &plan.scope, &message)?;
            let push = git_dest::push(&plan.root);
            Ok(RunOutcome {
                report,
                committed,
                push: Some(push),
            })
        }
    }
}

/// Back up one project and record the result in `.bork/backup.json`.
pub fn run_project(project_root: &Path) -> anyhow::Result<RunOutcome> {
    let root = crate::global_config::normalize_path(project_root);
    let backup = config::load_for_project(&root);
    if !backup.enabled {
        bail!("backups are not enabled for {}", root.display());
    }

    let result = (|| {
        if backup.wip {
            snapshot_worktrees(&root)?;
        }
        let plan = plan_project(&root)?;
        let outcome = execute(&plan)?;
        Ok((plan, outcome))
    })();
    finish(&status::project_status_path(&root), &backup, result)
}

/// Back up the global files, if configured, and record the result in
/// `~/.config/bork/backup.json`.
pub fn run_global() -> Option<anyhow::Result<RunOutcome>> {
    let plan = plan_global()?;
    let backup = plan.config.clone();
    let result = execute(&plan).map(|outcome| (plan, outcome));
    Some(finish(&status::global_status_path(), &backup, result))
}

/// Extension point for worktree WIP snapshots (hidden refs pushed to the
/// project's own remote, see `manifest::wip_ref_name`). Runs before the
/// manifest is built; an error fails the project's backup run.
fn snapshot_worktrees(_project_root: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Result of backing up one registered project.
pub struct ProjectRun {
    pub name: String,
    pub result: anyhow::Result<RunOutcome>,
}

/// Back up every registered project that opted in, then the global files.
pub fn run_all() -> (Vec<ProjectRun>, Option<anyhow::Result<RunOutcome>>) {
    let projects = crate::global_config::list_projects()
        .into_iter()
        .filter(|p| config::load_for_project(&p.path).enabled)
        .map(|p| ProjectRun {
            result: run_project(&p.path),
            name: p.name,
        })
        .collect();
    (projects, run_global())
}

fn finish(
    status_path: &Path,
    backup: &BackupConfig,
    result: anyhow::Result<(Plan, RunOutcome)>,
) -> anyhow::Result<RunOutcome> {
    let now = crate::app::unix_now();
    let record = match &result {
        Ok((plan, outcome)) => BackupStatus {
            last_run_at: now,
            ok: true,
            warning: push_warning(outcome.push.as_ref()),
            destination: backup.destination.as_str().to_string(),
            target: plan.target(),
            files: plan.entries.len(),
            changed: outcome.report.changed() || outcome.committed,
            ..BackupStatus::default()
        },
        Err(err) => BackupStatus {
            last_run_at: now,
            ok: false,
            error: Some(format!("{err:#}")),
            destination: backup.destination.as_str().to_string(),
            target: backup.path.clone().unwrap_or_default(),
            ..BackupStatus::default()
        },
    };
    if let Err(err) = status::record(status_path, record) {
        eprintln!(
            "bork: warning: could not write {}: {err}",
            status_path.display()
        );
    }
    result.map(|(_, outcome)| outcome)
}

fn push_warning(push: Option<&PushOutcome>) -> Option<String> {
    match push {
        Some(PushOutcome::Failed { remote, error }) => Some(format!(
            "committed locally, push to {remote} failed: {error}"
        )),
        _ => None,
    }
}

/// The scope becomes a directory under the destination, so it must be a
/// single plain path component.
fn validate_scope(scope: &str) -> anyhow::Result<()> {
    let plain = !scope.is_empty()
        && scope != "."
        && scope != ".."
        && !scope.contains('/')
        && !scope.contains('\0');
    if !plain || scope == GLOBAL_SCOPE {
        bail!("project name {scope:?} can't be used as a backup directory name");
    }
    Ok(())
}

/// Serialize backup runs machine-wide (CLI and worker can overlap on the same
/// git clone). Best effort: a lock failure doesn't block the backup.
fn lock_backups() -> Option<File> {
    let dir = crate::global_config::global_config_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("backup.lock"))
        .context("opening backup.lock")
        .ok()?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    // SAFETY: fd is a valid descriptor owned by `file` for the call's duration.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    (ret == 0).then_some(file)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for unix seconds (UTC).
pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn formats_utc() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_794_009_600 + 3_723), "2026-11-07T01:02:03Z");
    }

    #[test]
    fn rejects_unsafe_scopes() {
        for bad in ["", ".", "..", "a/b", GLOBAL_SCOPE] {
            assert!(validate_scope(bad).is_err(), "{bad:?}");
        }
        assert!(validate_scope("bork").is_ok());
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn project_backup_end_to_end_folder() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let root = project.path();
        write(
            &root.join(".bork/config.toml"),
            &format!(
                "project_name = \"demo\"\n[backup]\nenabled = true\npath = \"{}\"\n",
                dest.path().display()
            ),
        );
        write(&root.join(".bork/state.json"), "{\"issues\":[]}");
        write(&root.join(".bork/github-cache.json"), "{}");
        write(&root.join("AGENTS.md"), "agents");
        write(&root.join("main/README.md"), "code");

        crate::global_config::with_test_config_home(home.path(), || {
            let plan = plan_project(root).unwrap();
            assert_eq!(plan.scope, "demo");
            let rels: Vec<_> = plan.entries.iter().map(|e| e.rel.clone()).collect();
            assert_eq!(
                rels,
                vec![
                    PathBuf::from(".bork/config.toml"),
                    PathBuf::from(".bork/state.json"),
                    PathBuf::from("AGENTS.md"),
                    PathBuf::from("manifest.json"),
                ]
            );

            let first = run_project(root).unwrap();
            assert!(first.report.changed());
            let second = run_project(root).unwrap();
            assert!(!second.report.changed(), "manifest must be stable");

            let target = dest.path().join("demo");
            assert_eq!(
                fs::read_to_string(target.join("AGENTS.md")).unwrap(),
                "agents"
            );
            let manifest: manifest::Manifest =
                serde_json::from_str(&fs::read_to_string(target.join("manifest.json")).unwrap())
                    .unwrap();
            assert_eq!(manifest.project_name, "demo");

            let recorded = status::read(&status::project_status_path(root)).unwrap();
            assert!(recorded.ok);
            assert!(!recorded.changed);
            assert_eq!(recorded.files, 4);
            assert!(recorded.last_success_at.is_some());
        });
    }

    #[test]
    fn disabled_project_is_refused_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join(".bork/config.toml"),
            "[backup]\npath = \"/nope\"\n",
        );
        crate::global_config::with_test_config_home(home.path(), || {
            assert!(plan_project(project.path()).is_err());
            assert!(run_project(project.path()).is_err());
        });
        assert!(!status::project_status_path(project.path()).exists());
    }

    #[test]
    fn global_backup_uses_global_path() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            assert!(plan_global().is_none());
            let dir = crate::global_config::global_config_dir();
            write(
                &dir.join("config.toml"),
                &format!("[backup]\npath = \"{}\"\n", dest.path().display()),
            );
            write(&dir.join("projects.json"), "{\"projects\":[]}");
            write(&dir.join("bork.pid"), "1");
            let outcome = run_global().unwrap().unwrap();
            assert_eq!(outcome.report.written.len(), 2);
            assert!(dest.path().join("_global/projects.json").exists());
            assert!(!dest.path().join("_global/bork.pid").exists());
            assert!(status::read(&status::global_status_path()).unwrap().ok);
        });
    }
}
