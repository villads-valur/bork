//! Issue plans around worktree removal.
//!
//! Each issue worktree keeps a gitignored `.claude/planning.md`. Removing the
//! worktree would lose it, so both removal primitives
//! (`worktree::remove_worktree_in` and `prune::execute_removals`) call
//! [`save_before_removal`] first. Neither can be called without saying which
//! issue the worktree belongs to, so a new removal path can't skip the save.

use std::path::Path;

/// Save `worktree_dir`'s plan before the worktree goes away: under its
/// issue, or under `plans/_orphans/<worktree_dir>/` when no issue claims it
/// (a deleted issue, a card that became an orchestrator, a hand-made
/// worktree). Projects with backups off are a no-op.
pub fn save_before_removal(
    project_root: &Path,
    issue_id: Option<&str>,
    worktree_dir: &str,
) -> Result<(), String> {
    let saved = match issue_id {
        Some(issue_id) => crate::backup::save_issue_plan(project_root, issue_id, worktree_dir),
        None => crate::backup::save_orphan_plan(project_root, worktree_dir),
    };
    saved.map(|_| ()).map_err(|e| format!("{e:#}"))
}

/// Record a plan that a forced removal went past, so it never disappears
/// without a trace.
pub fn log_dropped_plan(project_root: &Path, worktree_dir: &str, error: &str) {
    crate::config::append_error_log(
        project_root,
        &[format!(
            "removed {worktree_dir} without saving its plan: {error}"
        )],
    );
}

/// Drop a deleted issue's saved plans (`plans/<issue-id>/`). Archived and
/// pruned issues keep theirs. Only touches projects with backups on, and
/// only that one directory.
pub fn remove_saved_plans(project_root: &Path, issue_id: &str) -> anyhow::Result<()> {
    if !crate::backup::config::project_enabled(project_root)? {
        return Ok(());
    }
    if !crate::backup::select::is_plain_component(issue_id) {
        anyhow::bail!("invalid issue id {issue_id:?}");
    }
    let dir = crate::backup::issue_plan_dir(project_root, issue_id);
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(meta) if meta.is_dir() => Ok(std::fs::remove_dir_all(&dir)?),
        Ok(_) => Ok(std::fs::remove_file(&dir)?),
    }
}

#[cfg(test)]
pub mod test_support {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Turn backups on for the project at `root`.
    pub fn enable_backups(root: &Path) {
        let path = root.join(".bork/config.toml");
        let existing = fs::read_to_string(&path).unwrap_or_default();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{existing}\n[backup]\nenabled = true\n")).unwrap();
    }

    /// Turn backups on, with a file where `plans/` should be so no plan can
    /// be saved.
    pub fn enable_broken_backups(root: &Path) {
        enable_backups(root);
        fs::write(root.join(crate::backup::PLANS_DIR), "not a dir").unwrap();
    }

    pub fn write_plan(root: &Path, worktree_dir: &str, contents: &str) {
        let path = root.join(worktree_dir).join(".claude/planning.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Where a saved plan for `issue_id` ends up.
    pub fn saved_plan(root: &Path, issue_id: &str) -> PathBuf {
        crate::backup::issue_plan_dir(root, issue_id).join("planning.md")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::test_support::*;
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".bork")).unwrap();
        fs::write(
            dir.path().join(".bork/config.toml"),
            "project_name = \"demo\"\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn saves_the_plan_when_backups_are_on() {
        let root = project();
        enable_backups(root.path());
        write_plan(root.path(), "demo-1-x", "plan");

        save_before_removal(root.path(), Some("demo-1"), "demo-1-x").unwrap();

        let saved = saved_plan(root.path(), "demo-1");
        assert_eq!(fs::read_to_string(saved).unwrap(), "plan");
    }

    #[test]
    fn backups_off_is_a_no_op() {
        let root = project();
        write_plan(root.path(), "demo-1-x", "plan");

        save_before_removal(root.path(), Some("demo-1"), "demo-1-x").unwrap();

        assert!(!root.path().join(crate::backup::PLANS_DIR).exists());
    }

    #[test]
    fn orphan_plans_are_saved_by_worktree_name() {
        let root = project();
        enable_backups(root.path());
        write_plan(root.path(), "demo-1-x", "plan");

        save_before_removal(root.path(), None, "demo-1-x").unwrap();

        let saved = root.path().join("plans/_orphans/demo-1-x/planning.md");
        assert_eq!(fs::read_to_string(saved).unwrap(), "plan");
    }

    #[test]
    fn a_failed_save_is_reported() {
        let root = project();
        enable_broken_backups(root.path());
        write_plan(root.path(), "demo-1-x", "plan");

        let err = save_before_removal(root.path(), Some("demo-1"), "demo-1-x").unwrap_err();
        assert!(err.contains("plans"), "{err}");
    }

    #[test]
    fn dropped_plans_are_logged() {
        let root = project();
        log_dropped_plan(root.path(), "demo-1-x", "disk full");
        let log = fs::read_to_string(crate::config::error_log_path(root.path())).unwrap();
        assert!(
            log.contains("demo-1-x without saving its plan: disk full"),
            "{log}"
        );
    }

    #[test]
    fn delete_removes_only_that_issues_plans() {
        let root = project();
        enable_backups(root.path());
        let plans = root.path().join(crate::backup::PLANS_DIR);
        fs::create_dir_all(plans.join("demo-1")).unwrap();
        fs::write(plans.join("demo-1/planning.md"), "1").unwrap();
        fs::create_dir_all(plans.join("demo-10")).unwrap();
        fs::write(plans.join("demo-10/planning.md"), "10").unwrap();

        remove_saved_plans(root.path(), "demo-1").unwrap();

        assert!(!plans.join("demo-1").exists());
        assert!(plans.join("demo-10/planning.md").exists());
        // Already gone: nothing to do.
        remove_saved_plans(root.path(), "demo-1").unwrap();
        assert!(remove_saved_plans(root.path(), "..").is_err());
    }

    #[test]
    fn delete_leaves_plans_alone_when_backups_are_off() {
        let root = project();
        let dir = root.path().join(crate::backup::PLANS_DIR).join("demo-1");
        fs::create_dir_all(&dir).unwrap();

        remove_saved_plans(root.path(), "demo-1").unwrap();

        assert!(dir.exists());
    }
}
