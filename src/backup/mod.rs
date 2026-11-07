//! Project backups: board state, non-git container files and issue plans,
//! mirrored into a folder (typically a synced one such as iCloud Drive).
//!
//! Library entry points (`run_project`, `run_global`, `run_all`, `plan_*`,
//! `save_issue_plan`) are shared by the `bork backup` CLI, the background
//! worker and the worktree removal paths. Each project writes to
//! `<path>/<project_name>/`, the global files to `<path>/_global/`.

pub mod config;
pub mod mirror;
pub mod select;
pub mod status;

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};

use self::config::BackupConfig;
use self::mirror::{Entry, MirrorReport};
use self::select::{ISSUES_DIR, PLAN_FILE};
use self::status::BackupStatus;

/// Destination subdirectory for the global files. Leading underscore keeps it
/// clear of project names.
pub const GLOBAL_SCOPE: &str = "_global";

/// Global files worth restoring; `bork.pid`, locks and caches are not.
const GLOBAL_FILES: &[&str] = &["projects.json", "config.toml"];

/// File name of a backed-up issue plan under `_issues/<issue-id>/`.
const PLAN_NAME: &str = "planning.md";

/// Everything a backup run would write, resolved but not yet executed.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Destination subdirectory name: the project name, or [`GLOBAL_SCOPE`].
    pub scope: String,
    /// Destination folder root.
    pub root: PathBuf,
    /// What is being backed up: the project root, or the global config dir.
    /// Recorded in the destination's ownership marker.
    pub source: PathBuf,
    pub entries: Vec<Entry>,
    /// Issue plans to keep in the backup even without a live source: every
    /// issue still on the board (archived and pruned ones included). `None`
    /// means the board is missing or unreadable: keep every plan.
    pub board_issue_ids: Option<HashSet<String>>,
}

impl Plan {
    pub fn target(&self) -> PathBuf {
        self.root.join(&self.scope)
    }

    /// Whether a stale destination file must survive the mirror: the board
    /// and project config (never dropped because the source went missing),
    /// and issue plans while their issue is on the board.
    fn retains(&self, rel: &Path) -> bool {
        if self.scope != GLOBAL_SCOPE && select::BORK_FILES.iter().any(|f| rel == Path::new(f)) {
            return true;
        }
        let mut parts = rel.components();
        if parts.next() != Some(Component::Normal(ISSUES_DIR.as_ref())) {
            return false;
        }
        let Some(Component::Normal(issue_id)) = parts.next() else {
            return false;
        };
        match &self.board_issue_ids {
            None => true,
            Some(ids) => issue_id.to_str().is_some_and(|id| ids.contains(id)),
        }
    }
}

/// Resolve what a project backup would write. Fails when the project has not
/// opted in or has no destination path.
pub fn plan_project(project_root: &Path) -> anyhow::Result<Plan> {
    let root = crate::global_config::normalize_path(project_root);
    let (dest_root, scope) = resolve_project(&root)?;

    // A missing board loads as an empty one, which must not read as "every
    // issue was deleted".
    let state = if crate::config::state_mtime(&root).is_some() {
        crate::config::try_load_state(&root)
    } else {
        None
    };
    let board = state.clone().unwrap_or_default();
    let git_paths = select::git_worktree_paths(&root);
    let worktrees = select::worktree_dirs(&root, &git_paths, &board);

    let mut entries: Vec<Entry> = select::select_files(&root, &worktrees)?
        .into_iter()
        .map(|rel| Entry {
            source: root.join(&rel),
            follow: select::BORK_FILES.iter().any(|f| rel == Path::new(f)),
            rel,
        })
        .collect();
    entries.extend(
        select::issue_plans(&root, &board)
            .into_iter()
            .map(|(issue_id, source)| Entry {
                rel: issue_plan_rel(&issue_id),
                source,
                follow: false,
            }),
    );

    Ok(Plan {
        scope,
        root: dest_root,
        source: root,
        entries,
        // A corrupt state.json is still copied as-is, but must not drop plans.
        board_issue_ids: state.map(|s| s.issues.into_iter().map(|i| i.id).collect()),
    })
}

/// Resolved global backup settings; `enabled` when the global `[backup]`
/// section has a path.
pub fn global_config() -> BackupConfig {
    BackupConfig::resolve_global(&config::load_global_layer())
}

/// Resolve the global files backup, or `None` when the global `[backup]`
/// section has no path.
pub fn plan_global() -> Option<anyhow::Result<Plan>> {
    let backup = global_config();
    let dest_root = backup.path.clone().filter(|_| backup.enabled)?;
    let dir = crate::global_config::normalize_path(&crate::global_config::global_config_dir());
    if let Err(err) = check_target(&dest_root, GLOBAL_SCOPE, &dir) {
        return Some(Err(err));
    }
    let entries = GLOBAL_FILES
        .iter()
        .filter(|name| dir.join(name).is_file())
        .map(|name| Entry {
            rel: PathBuf::from(name),
            source: dir.join(name),
            follow: true,
        })
        .collect();
    Some(Ok(Plan {
        scope: GLOBAL_SCOPE.to_string(),
        root: dest_root,
        source: dir,
        entries,
        board_issue_ids: Some(HashSet::new()),
    }))
}

/// Write a plan to its destination folder.
pub fn execute(plan: &Plan) -> anyhow::Result<MirrorReport> {
    let _lock = lock_backups()?;
    let target = plan.target();
    mirror::claim(&target, &plan.source)?;
    mirror::mirror(&plan.entries, &target, &|rel| plan.retains(rel))
}

/// Back up one project and record the result in `.bork/backup.json`.
pub fn run_project(project_root: &Path) -> anyhow::Result<MirrorReport> {
    let root = crate::global_config::normalize_path(project_root);
    let backup = config::load_for_project(&root);
    if !backup.enabled {
        bail!("backups are not enabled for {}", root.display());
    }
    let result = plan_project(&root).and_then(|plan| execute(&plan).map(|report| (plan, report)));
    finish(&status::project_status_path(&root), &backup, result)
}

/// Back up the global files, if configured, and record the result in
/// `~/.config/bork/backup.json`.
pub fn run_global() -> Option<anyhow::Result<MirrorReport>> {
    let result = plan_global()?.and_then(|plan| execute(&plan).map(|report| (plan, report)));
    Some(finish(
        &status::global_status_path(),
        &global_config(),
        result,
    ))
}

/// Result of backing up one registered project.
pub struct ProjectRun {
    pub name: String,
    pub result: anyhow::Result<MirrorReport>,
}

/// Back up every registered project that opted in, then the global files.
pub fn run_all() -> (Vec<ProjectRun>, Option<anyhow::Result<MirrorReport>>) {
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

/// Save one issue's `.claude/planning.md` from `<project_root>/<worktree_dir>`
/// into the backup, so the plan survives the worktree being removed. Meant
/// for the archive/delete/prune paths, right before removal.
///
/// Returns `Ok(false)` when backups are off for the project or the worktree
/// has no plan, `Ok(true)` once the plan is in the backup.
pub fn save_issue_plan(
    project_root: &Path,
    issue_id: &str,
    worktree_dir: &str,
) -> anyhow::Result<bool> {
    let root = crate::global_config::normalize_path(project_root);
    if !config::load_for_project(&root).enabled {
        return Ok(false);
    }
    if !select::is_plain_component(issue_id) || !select::is_plain_component(worktree_dir) {
        bail!("invalid issue id {issue_id:?} or worktree {worktree_dir:?}");
    }
    let (dest_root, scope) = resolve_project(&root)?;
    let source = root.join(worktree_dir).join(PLAN_FILE);
    let target = dest_root.join(scope);
    let _lock = lock_backups()?;
    mirror::claim(&target, &root)?;
    Ok(mirror::sync_path(&source, false, &target, &issue_plan_rel(issue_id))?.is_some())
}

/// Relative destination path of an issue's plan.
pub fn issue_plan_rel(issue_id: &str) -> PathBuf {
    Path::new(ISSUES_DIR).join(issue_id).join(PLAN_NAME)
}

/// Destination root and scope name for an opted-in project.
fn resolve_project(root: &Path) -> anyhow::Result<(PathBuf, String)> {
    let backup = config::load_for_project(root);
    if !backup.enabled {
        bail!(
            "backups are not enabled for {} (set `enabled = true` under [backup] in .bork/config.toml)",
            root.display()
        );
    }
    let Some(dest_root) = backup.path.clone() else {
        bail!("no backup path configured (set `path` under [backup])");
    };
    let scope = crate::config::load_config_from(root).project_name;
    if !select::is_plain_component(&scope) || scope == GLOBAL_SCOPE {
        bail!("project name {scope:?} can't be used as a backup directory name");
    }

    check_target(&dest_root, &scope, root)?;
    Ok((dest_root, scope))
}

/// Refuse destinations the mirror could damage: relative paths (they would
/// depend on the cwd), and targets that are, contain or sit inside the
/// source or any registered project. Ownership of the target itself is
/// checked by [`mirror::claim`] when writing.
fn check_target(dest_root: &Path, scope: &str, source: &Path) -> anyhow::Result<()> {
    if !dest_root.is_absolute() {
        bail!(
            "backup path {} must be absolute (or start with ~/)",
            dest_root.display()
        );
    }
    let target = resolve_through_existing(&dest_root.join(scope));
    let projects = crate::global_config::list_projects()
        .into_iter()
        .map(|p| crate::global_config::normalize_path(&p.path));
    for protected in std::iter::once(source.to_path_buf()).chain(projects) {
        if target.starts_with(&protected) || protected.starts_with(&target) {
            bail!(
                "backup target {} overlaps {}; pick a path outside it",
                target.display(),
                protected.display()
            );
        }
    }
    Ok(())
}

/// Canonicalize the deepest existing ancestor of `path` and append the rest,
/// so symlinks and `..` resolve even before the target exists.
fn resolve_through_existing(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(canonical, |acc, part| acc.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn finish(
    status_path: &Path,
    backup: &BackupConfig,
    result: anyhow::Result<(Plan, MirrorReport)>,
) -> anyhow::Result<MirrorReport> {
    let now = crate::app::unix_now();
    let record = match &result {
        Ok((plan, report)) => BackupStatus {
            last_run_at: now,
            ok: true,
            target: plan.target(),
            files: plan.entries.len(),
            changed: report.changed(),
            ..BackupStatus::default()
        },
        Err(err) => BackupStatus {
            last_run_at: now,
            ok: false,
            error: Some(format!("{err:#}")),
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
    result.map(|(_, report)| report)
}

/// Serialize backup runs machine-wide (CLI, worker and removal hooks can
/// overlap on the same folder). Held until the returned file is dropped.
fn lock_backups() -> anyhow::Result<File> {
    let dir = crate::global_config::global_config_dir();
    std::fs::create_dir_all(&dir)?;
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
    use std::fs;

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

    /// A project opted in to backups at `dest`.
    fn project(dest: &Path) -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join(".bork/config.toml"),
            &format!(
                "project_name = \"demo\"\n[backup]\nenabled = true\npath = \"{}\"\n",
                dest.display()
            ),
        );
        project
    }

    #[test]
    fn project_backup_end_to_end() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let project = project(dest.path());
        let root = project.path();
        save_board(root, &[("demo-1", Some("demo-1-a"))]);
        write(&root.join(".bork/github-cache.json"), "{}");
        write(&root.join("AGENTS.md"), "agents");
        write(&root.join("main/README.md"), "code");
        write(&root.join("demo-1-a/.git"), "gitdir: x");
        write(&root.join("demo-1-a/src/lib.rs"), "code");
        write(&root.join("demo-1-a/.claude/planning.md"), "plan v1");

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
                    PathBuf::from("_issues/demo-1/planning.md"),
                ]
            );

            assert!(run_project(root).unwrap().changed());
            assert!(!run_project(root).unwrap().changed());

            let target = dest.path().join("demo");
            assert_eq!(
                fs::read_to_string(target.join("AGENTS.md")).unwrap(),
                "agents"
            );
            assert_eq!(
                fs::read_to_string(target.join("_issues/demo-1/planning.md")).unwrap(),
                "plan v1"
            );
            let recorded = status::read(&status::project_status_path(root)).unwrap();
            assert!(recorded.ok);
            assert!(!recorded.changed);
            assert_eq!(recorded.files, 4);
            assert!(recorded.last_success_at.is_some());
        });
    }

    #[test]
    fn issue_plans_kept_until_issue_is_deleted() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let project = project(dest.path());
        let root = project.path();
        let plan_path = dest.path().join("demo/_issues/demo-1/planning.md");
        save_board(root, &[("demo-1", Some("demo-1-a"))]);
        write(&root.join("demo-1-a/.git"), "gitdir: x");
        write(&root.join("demo-1-a/.claude/planning.md"), "plan");
        write(&root.join("AGENTS.md"), "a");

        crate::global_config::with_test_config_home(home.path(), || {
            run_project(root).unwrap();
            assert!(plan_path.exists());

            // Archived/pruned: worktree gone, issue still on the board.
            fs::remove_dir_all(root.join("demo-1-a")).unwrap();
            save_board(root, &[("demo-1", None)]);
            fs::remove_file(root.join("AGENTS.md")).unwrap();
            let report = run_project(root).unwrap();
            assert_eq!(report.removed, vec![PathBuf::from("AGENTS.md")]);
            assert!(plan_path.exists());

            // Unreadable board: nothing is dropped.
            write(&root.join(".bork/state.json"), "{ not json");
            run_project(root).unwrap();
            assert!(plan_path.exists());

            // Deleted from the board: the plan goes.
            save_board(root, &[]);
            run_project(root).unwrap();
            assert!(!plan_path.exists());
        });
    }

    #[test]
    fn missing_board_keeps_plans_and_backed_up_board() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let project = project(dest.path());
        let root = project.path();
        let backup = dest.path().join("demo");
        save_board(root, &[("demo-1", None)]);

        crate::global_config::with_test_config_home(home.path(), || {
            run_project(root).unwrap();
            write(&backup.join("_issues/demo-1/planning.md"), "archived plan");
            fs::remove_file(root.join(".bork/state.json")).unwrap();
            run_project(root).unwrap();
            assert!(backup.join("_issues/demo-1/planning.md").exists());
            assert!(backup.join(".bork/state.json").exists());
        });
    }

    #[test]
    fn refuses_targets_overlapping_the_project() {
        let home = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("demo");
        write(&root.join("main/src.rs"), "code");
        for path in [parent.path().to_path_buf(), root.join("backups")] {
            write(
                &root.join(".bork/config.toml"),
                &format!(
                    "project_name = \"demo\"\n[backup]\nenabled = true\npath = \"{}\"\n",
                    path.display()
                ),
            );
            crate::global_config::with_test_config_home(home.path(), || {
                let err = run_project(&root).unwrap_err();
                assert!(format!("{err:#}").contains("overlaps"), "{err:#}");
            });
        }
        assert!(root.join("main/src.rs").exists());
    }

    #[test]
    fn second_project_cannot_take_over_a_backup() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let first = project(dest.path());
        let second = project(dest.path());
        write(&first.path().join("first.md"), "1");
        write(&second.path().join("second.md"), "2");

        crate::global_config::with_test_config_home(home.path(), || {
            run_project(first.path()).unwrap();
            let err = run_project(second.path()).unwrap_err();
            assert!(
                format!("{err:#}").contains("holds the backup of"),
                "{err:#}"
            );
            assert!(save_issue_plan(second.path(), "demo-1", "wt").is_err());
            run_project(first.path()).unwrap();
        });
        assert!(dest.path().join("demo/first.md").exists());
        assert!(!dest.path().join("demo/second.md").exists());
    }

    #[test]
    fn nested_backups_survive_each_other() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        // `parent` backs up to <dest>/parent, `child` to <dest>/parent/child.
        let parent = tempfile::tempdir().unwrap();
        let child = tempfile::tempdir().unwrap();
        for (dir, name, path) in [
            (parent.path(), "parent", dest.path().to_path_buf()),
            (child.path(), "child", dest.path().join("parent")),
        ] {
            write(
                &dir.join(".bork/config.toml"),
                &format!(
                    "project_name = \"{name}\"\n[backup]\nenabled = true\npath = \"{}\"\n",
                    path.display()
                ),
            );
            write(&dir.join("AGENTS.md"), name);
        }
        crate::global_config::with_test_config_home(home.path(), || {
            // Child first: the parent's folder is non-empty and unowned, so the
            // parent is refused instead of wiping the child.
            run_project(child.path()).unwrap();
            assert!(run_project(parent.path()).is_err());
            assert!(dest.path().join("parent/child/AGENTS.md").exists());

            // Parent first: its cleanup skips the child's marked folder.
            fs::remove_dir_all(dest.path().join("parent")).unwrap();
            run_project(parent.path()).unwrap();
            run_project(child.path()).unwrap();
            run_project(parent.path()).unwrap();
        });
        assert!(dest.path().join("parent/child/AGENTS.md").exists());
        assert!(dest.path().join("parent/AGENTS.md").exists());
    }

    #[test]
    fn refuses_scope_symlink_and_unowned_dirs() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("precious.txt"), "keep");
        let project = project(dest.path());
        write(&project.path().join("AGENTS.md"), "a");
        std::os::unix::fs::symlink(outside.path(), dest.path().join("demo")).unwrap();
        crate::global_config::with_test_config_home(home.path(), || {
            assert!(run_project(project.path()).is_err());
        });
        assert!(outside.path().join("precious.txt").exists());
        assert!(!outside.path().join("AGENTS.md").exists());
    }

    #[test]
    fn global_backup_refuses_live_dirs_and_relative_paths() {
        let home = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        write(&scratch.path().join("_global/.bork/config.toml"), "x");
        write(&scratch.path().join("_global/main/code.rs"), "code");
        crate::global_config::with_test_config_home(home.path(), || {
            let dir = crate::global_config::global_config_dir();
            write(&dir.join("projects.json"), "{\"projects\":[]}");
            for path in [scratch.path().display().to_string(), "backups".to_string()] {
                write(
                    &dir.join("config.toml"),
                    &format!("[backup]\npath = \"{path}\"\n"),
                );
                assert!(run_global().unwrap().is_err(), "{path}");
            }
        });
        assert!(scratch.path().join("_global/main/code.rs").exists());
    }

    #[test]
    fn symlinked_board_is_backed_up_by_content() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let project = project(dest.path());
        let root = project.path();
        let outside = tempfile::tempdir().unwrap();
        save_board(root, &[("demo-1", None)]);
        crate::global_config::with_test_config_home(home.path(), || {
            run_project(root).unwrap();
            let real = outside.path().join("state.json");
            fs::rename(root.join(".bork/state.json"), &real).unwrap();
            std::os::unix::fs::symlink(&real, root.join(".bork/state.json")).unwrap();
            run_project(root).unwrap();
        });
        let backed_up = dest.path().join("demo/.bork/state.json");
        assert!(fs::symlink_metadata(&backed_up).unwrap().is_file());
        assert!(fs::read_to_string(backed_up).unwrap().contains("demo-1"));
    }

    #[test]
    fn save_issue_plan_copies_one_plan() {
        let home = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let project = project(dest.path());
        let root = project.path();
        write(&root.join("demo-2-b/.claude/planning.md"), "the plan");

        crate::global_config::with_test_config_home(home.path(), || {
            assert!(save_issue_plan(root, "demo-2", "demo-2-b").unwrap());
            assert_eq!(
                fs::read_to_string(dest.path().join("demo/_issues/demo-2/planning.md")).unwrap(),
                "the plan"
            );
            assert!(!save_issue_plan(root, "demo-3", "no-such-dir").unwrap());
            assert!(save_issue_plan(root, "../x", "demo-2-b").is_err());
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
        write(&project.path().join("wt/.claude/planning.md"), "p");
        crate::global_config::with_test_config_home(home.path(), || {
            assert!(plan_project(project.path()).is_err());
            assert!(run_project(project.path()).is_err());
            assert!(!save_issue_plan(project.path(), "x-1", "wt").unwrap());
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
            let report = run_global().unwrap().unwrap();
            assert_eq!(report.written.len(), 2);
            assert!(dest.path().join("_global/projects.json").exists());
            assert!(!dest.path().join("_global/bork.pid").exists());
            assert!(status::read(&status::global_status_path()).unwrap().ok);
        });
    }
}
