//! Decide which container files a project backup covers.
//!
//! Everything at the container root is backed up except `main/`, issue
//! worktrees and ephemeral files. From `.bork/` only the board state and the
//! project config are kept. Each issue worktree contributes only its
//! gitignored `.claude/planning.md`, stored per issue id.
//!
//! Read errors other than "not found" fail the selection: a backup that
//! silently misses files is worse than one that reports it couldn't run.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::AppState;

/// Files inside `.bork/` worth restoring, as paths relative to the
/// container. Everything else there is derived or ephemeral (agent-status/,
/// github-cache.json, errors.log, locks, temps). Symlinks are followed, since
/// these are what bork itself reads.
pub const BORK_FILES: &[&str] = &[".bork/state.json", ".bork/config.toml"];

/// Archive subdirectory (per project) holding issue plans
/// (`_issues/<issue-id>/planning.md`). The underscore keeps it clear of
/// container entries.
pub const ISSUES_DIR: &str = "_issues";

/// Plan file inside a worktree, relative to the worktree root.
pub const PLAN_FILE: &str = ".claude/planning.md";

/// An issue id usable as one plain directory name.
pub fn is_plain_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Whether `path` is a regular file (symlinks followed). Only "not found"
/// means no; other errors (permissions, symlink loops) are returned so a
/// backup never mistakes an unreadable file for a missing one.
pub fn is_existing_file(path: &Path) -> anyhow::Result<bool> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// `(issue_id, plan path)` for every issue whose worktree currently has a plan.
pub fn issue_plans(
    project_root: &Path,
    state: &AppState,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut plans = Vec::new();
    for issue in &state.issues {
        if !is_plain_component(&issue.id) {
            continue;
        }
        let Some(worktree) = issue.worktree.as_deref().filter(|w| is_plain_component(w)) else {
            continue;
        };
        let plan = project_root.join(worktree).join(PLAN_FILE);
        if is_existing_file(&plan)? {
            plans.push((issue.id.clone(), plan));
        }
    }
    plans.sort();
    Ok(plans)
}

/// Top-level directory names of every worktree in the container, from three
/// sources so a stale or partial one can't leak a checkout into the backup:
/// `git worktree list` paths directly under the container, the `worktree`
/// recorded on each issue, and any child dir with a `.git` entry (worktrees
/// have a `.git` file, clones a `.git` dir).
pub fn worktree_dirs(
    project_root: &Path,
    git_worktree_paths: &[PathBuf],
    state: &AppState,
) -> HashSet<String> {
    let mut dirs: HashSet<String> = HashSet::new();
    dirs.insert("main".to_string());

    for path in git_worktree_paths {
        if path.parent() == Some(project_root) {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                dirs.insert(name.to_string());
            }
        }
    }

    for issue in &state.issues {
        if let Some(name) = issue.worktree.as_deref().filter(|n| !n.is_empty()) {
            dirs.insert(name.to_string());
        }
    }

    if let Ok(entries) = fs::read_dir(project_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_real_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_real_dir && fs::symlink_metadata(path.join(".git")).is_ok() {
                if let Some(name) = entry.file_name().to_str() {
                    dirs.insert(name.to_string());
                }
            }
        }
    }

    dirs
}

/// True for files that are never worth backing up, wherever they live.
pub fn is_ephemeral(name: &str) -> bool {
    if name == ".DS_Store" {
        return true;
    }
    if name.ends_with(".lock") || name.ends_with(".tmp") || name.ends_with(".corrupt") {
        return true;
    }
    // `atomic_write` temps: `<stem>.tmp.<pid>`.
    if let Some((_, pid)) = name.rsplit_once(".tmp.") {
        return !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

/// Relative paths (files and symlinks) under `project_root` to back up,
/// sorted. Directories are implied by their contents; empty ones are dropped.
pub fn select_files(
    project_root: &Path,
    worktrees: &HashSet<String>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();

    for rel in BORK_FILES {
        if is_existing_file(&project_root.join(rel))? {
            out.insert(PathBuf::from(rel));
        }
    }

    for name in list_dir(project_root)? {
        if name == ".bork" || name == ISSUES_DIR || worktrees.contains(&name) || is_ephemeral(&name)
        {
            continue;
        }
        walk(project_root, Path::new(&name), &mut out)?;
    }

    Ok(out.into_iter().collect())
}

/// Any error other than "not found" fails the selection, since a backup that
/// silently misses files would replace a complete one.
fn walk(root: &Path, rel: &Path, out: &mut BTreeSet<PathBuf>) -> anyhow::Result<()> {
    let path = root.join(rel);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let file_type = meta.file_type();
    if file_type.is_file() || file_type.is_symlink() {
        out.insert(rel.to_path_buf());
        return Ok(());
    }
    if !file_type.is_dir() {
        return Ok(());
    }
    // A nested checkout (anything with its own `.git`) is code, not
    // container docs: skip it whole, like top-level worktrees.
    match fs::symlink_metadata(path.join(".git")) {
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    }
    for name in list_dir(&path)? {
        if is_ephemeral(&name) {
            continue;
        }
        walk(root, &rel.join(&name), out)?;
    }
    Ok(())
}

/// UTF-8 entry names of `dir`; empty if it vanished.
fn list_dir(dir: &Path) -> anyhow::Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentKind, Column, Issue};

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, rel).unwrap();
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn ephemeral_names() {
        for name in [
            ".DS_Store",
            "projects.lock",
            "x.tmp",
            "state.tmp.1234",
            "state.json.corrupt",
        ] {
            assert!(is_ephemeral(name), "{name}");
        }
        for name in ["state.json", "notes.tmp.md", "AGENTS.md", "x.tmp.abc"] {
            assert!(!is_ephemeral(name), "{name}");
        }
    }

    #[test]
    fn selects_container_files_and_skips_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, ".bork/state.json");
        touch(root, ".bork/config.toml");
        touch(root, ".bork/github-cache.json");
        touch(root, ".bork/errors.log");
        touch(root, ".bork/agent-status/bork-1");
        touch(root, ".bork/state.tmp.99");
        touch(root, "AGENTS.md");
        touch(root, "opencode.jsonc");
        touch(root, "plans/bork-7/planning.md");
        touch(root, ".claude/settings.json");
        touch(root, ".agents/skills/x.md");
        touch(root, "plans/.DS_Store");
        touch(root, "main/src/lib.rs");
        // Worktree known only from git.
        touch(root, "bork-2-from-git/file.rs");
        // Worktree known only from state.
        touch(root, "bork-3-from-state/file.rs");
        // Worktree known only from its `.git` file.
        touch(root, "bork-4-orphan/.git");
        touch(root, "bork-4-orphan/file.rs");
        // A nested checkout below the top level is skipped whole, `.git`
        // file or dir alike.
        touch(root, "plans/tool/.git/HEAD");
        touch(root, "plans/tool/readme.md");
        touch(root, "refs/other/.git");
        touch(root, "refs/other/node_modules/pkg/index.js");

        let mut state = AppState::default();
        let mut issue = Issue::new("bork-3", "t", Column::InProgress, AgentKind::Claude);
        issue.worktree = Some("bork-3-from-state".into());
        state.issues.push(issue);

        let worktrees = worktree_dirs(
            root,
            &[
                root.join("main"),
                root.join("bork-2-from-git"),
                PathBuf::from("/elsewhere/x"),
            ],
            &state,
        );
        let selected = select_files(root, &worktrees).unwrap();

        assert_eq!(
            selected,
            paths(&[
                ".agents/skills/x.md",
                ".bork/config.toml",
                ".bork/state.json",
                ".claude/settings.json",
                "AGENTS.md",
                "opencode.jsonc",
                "plans/bork-7/planning.md",
            ])
        );
        assert!(!worktrees.contains("x"));
    }

    #[test]
    fn finds_issue_plans_in_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "p-1-a/.claude/planning.md");
        touch(root, "p-2-b/src/lib.rs");
        touch(root, "_issues/stale");

        let mut state = AppState::default();
        for (id, wt) in [
            ("p-1", Some("p-1-a")),
            ("p-2", Some("p-2-b")),
            ("p-3", None),
            ("../x", Some("p-1-a")),
        ] {
            let mut issue = Issue::new(id, "t", Column::InProgress, AgentKind::Claude);
            issue.worktree = wt.map(String::from);
            state.issues.push(issue);
        }

        assert_eq!(
            issue_plans(root, &state).unwrap(),
            vec![("p-1".to_string(), root.join("p-1-a/.claude/planning.md"))]
        );
        // `_issues` in the container is never picked up as a container entry.
        assert!(select_files(root, &HashSet::new())
            .unwrap()
            .iter()
            .all(|p| !p.starts_with(ISSUES_DIR)));
    }

    #[test]
    fn unreadable_dir_fails_the_selection() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "plans/a.md");
        let plans = dir.path().join("plans");
        fs::set_permissions(&plans, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read_dir(&plans).is_ok(); // true when running as root
        let result = select_files(dir.path(), &HashSet::new());
        fs::set_permissions(&plans, fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert!(result.is_err());
        }
    }

    #[test]
    fn keeps_symlinks_as_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "AGENTS.md");
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
        let selected = select_files(root, &HashSet::new()).unwrap();
        assert_eq!(selected, paths(&["AGENTS.md", "CLAUDE.md"]));
    }
}
