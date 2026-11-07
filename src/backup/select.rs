//! Decide which container files a project backup covers.
//!
//! Everything at the container root is backed up except `main/`, issue
//! worktrees and ephemeral files. From `.bork/` only the board state and the
//! project config are kept.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::AppState;

/// Files inside `.bork/` worth restoring. Everything else there is derived or
/// ephemeral (agent-status/, github-cache.json, errors.log, locks, temps).
const BORK_FILES: &[&str] = &["state.json", "config.toml"];

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
pub fn select_files(project_root: &Path, worktrees: &HashSet<String>) -> Vec<PathBuf> {
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();

    for name in BORK_FILES {
        let rel = Path::new(".bork").join(name);
        if fs::symlink_metadata(project_root.join(&rel)).is_ok_and(|m| m.is_file()) {
            out.insert(rel);
        }
    }

    let Ok(entries) = fs::read_dir(project_root) else {
        return out.into_iter().collect();
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        if name == ".bork" || worktrees.contains(&name) || is_ephemeral(&name) {
            continue;
        }
        walk(project_root, Path::new(&name), &mut out);
    }

    out.into_iter().collect()
}

fn walk(root: &Path, rel: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(meta) = fs::symlink_metadata(root.join(rel)) else {
        return;
    };
    let file_type = meta.file_type();
    if file_type.is_file() || file_type.is_symlink() {
        out.insert(rel.to_path_buf());
        return;
    }
    if !file_type.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(root.join(rel)) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        // A nested repo's object store is not ours to copy.
        if name == ".git" || is_ephemeral(&name) {
            continue;
        }
        walk(root, &rel.join(&name), out);
    }
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
        // A nested repo below the top level is kept, minus its object store.
        touch(root, "plans/tool/.git/HEAD");
        touch(root, "plans/tool/readme.md");

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
        let selected = select_files(root, &worktrees);

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
                "plans/tool/readme.md",
            ])
        );
        assert!(!worktrees.contains("x"));
    }

    #[test]
    fn keeps_symlinks_as_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "AGENTS.md");
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
        let selected = select_files(root, &HashSet::new());
        assert_eq!(selected, paths(&["AGENTS.md", "CLAUDE.md"]));
    }
}
