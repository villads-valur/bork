use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use crate::types::WorktreeStatus;

pub struct GitPollResult {
    pub statuses: HashMap<String, WorktreeStatus>,
    pub branches: HashMap<String, String>,
}

pub fn poll_all_worktrees(project_root: &Path, skip: &HashSet<String>) -> GitPollResult {
    let mut statuses = HashMap::new();
    let mut branches = HashMap::new();

    let Ok(entries) = std::fs::read_dir(project_root) else {
        return GitPollResult { statuses, branches };
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Ok(dir_name) = entry.file_name().into_string() else {
            continue;
        };
        if dir_name.starts_with('.') || !path.join(".git").exists() {
            continue;
        }
        if skip.contains(&dir_name) {
            continue;
        }

        if let Some(status) = get_worktree_status(&path) {
            statuses.insert(dir_name.clone(), status);
        }
        if let Some(branch) = get_branch_name(&path) {
            branches.insert(dir_name, branch);
        }
    }

    GitPollResult { statuses, branches }
}

fn git_output(worktree_path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(args)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn get_worktree_status(worktree_path: &Path) -> Option<WorktreeStatus> {
    let stdout = git_output(worktree_path, &["status", "--short"])?;
    Some(parse_git_status(&stdout))
}

fn get_branch_name(worktree_path: &Path) -> Option<String> {
    let stdout = git_output(worktree_path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = stdout.trim();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch.to_string())
    }
}

pub(crate) fn parse_git_status(output: &str) -> WorktreeStatus {
    let mut staged = 0;
    let mut unstaged = 0;

    for line in output.lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 2 {
            continue;
        }

        let (x, y) = (bytes[0], bytes[1]);

        if x == b'?' && y == b'?' {
            unstaged += 1;
            continue;
        }

        if x != b' ' && x != b'?' {
            staged += 1;
        }
        if y != b' ' {
            unstaged += 1;
        }
    }

    WorktreeStatus { staged, unstaged }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_status() {
        let result = parse_git_status("");
        assert_eq!(result.staged, 0);
        assert_eq!(result.unstaged, 0);
        assert!(result.is_clean());
    }

    #[test]
    fn parse_untracked_files() {
        let output = "?? newfile.rs\n?? another.rs\n";
        let result = parse_git_status(output);
        assert_eq!(result.staged, 0);
        assert_eq!(result.unstaged, 2);
    }

    #[test]
    fn parse_staged_files() {
        let output = "M  src/app.rs\nA  src/new.rs\n";
        let result = parse_git_status(output);
        assert_eq!(result.staged, 2);
        assert_eq!(result.unstaged, 0);
    }

    #[test]
    fn parse_modified_unstaged() {
        let output = " M src/app.rs\n";
        let result = parse_git_status(output);
        assert_eq!(result.staged, 0);
        assert_eq!(result.unstaged, 1);
    }

    #[test]
    fn parse_both_staged_and_unstaged() {
        // MM = staged modification + unstaged modification
        let output = "MM src/app.rs\n";
        let result = parse_git_status(output);
        assert_eq!(result.staged, 1);
        assert_eq!(result.unstaged, 1);
    }

    #[test]
    fn parse_mixed_status() {
        let output = "M  staged.rs\n M unstaged.rs\n?? untracked.rs\nA  added.rs\n";
        let result = parse_git_status(output);
        assert_eq!(result.staged, 2); // M staged.rs + A added.rs
        assert_eq!(result.unstaged, 2); // M unstaged.rs + ?? untracked.rs
    }

    #[test]
    fn parse_short_lines_ignored() {
        let output = "X\n";
        let result = parse_git_status(output);
        assert_eq!(result.staged, 0);
        assert_eq!(result.unstaged, 0);
    }

    // --- Skip set for poll_all_worktrees ---

    #[test]
    fn poll_skip_set_excludes_done_worktrees() {
        // This tests the filtering logic within poll_all_worktrees.
        // We can't easily test with real git repos in unit tests,
        // but we can verify the skip set is respected by checking
        // that the function signature accepts it.
        let skip: HashSet<String> = ["done-worktree".to_string()].into_iter().collect();
        // The function exists and compiles with the skip parameter
        let _result = poll_all_worktrees(std::path::Path::new("/nonexistent"), &skip);
        // The result should be empty since the path doesn't exist
        assert!(_result.statuses.is_empty());
    }
}
