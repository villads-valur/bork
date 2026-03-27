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
    fn test_parse_clean_repo() {
        let status = parse_git_status("");
        assert_eq!(status.staged, 0);
        assert_eq!(status.unstaged, 0);
        assert!(status.is_clean());
    }

    #[test]
    fn test_parse_untracked_files() {
        let output = "?? new_file.txt\n?? another.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 0);
        assert_eq!(status.unstaged, 2);
    }

    #[test]
    fn test_parse_staged_modification() {
        let output = "M  src/main.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 1);
        assert_eq!(status.unstaged, 0);
    }

    #[test]
    fn test_parse_unstaged_modification() {
        let output = " M src/main.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 0);
        assert_eq!(status.unstaged, 1);
    }

    #[test]
    fn test_parse_staged_and_unstaged() {
        let output = "MM src/main.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 1);
        assert_eq!(status.unstaged, 1);
    }

    #[test]
    fn test_parse_mixed_statuses() {
        let output = "M  staged.rs\n M unstaged.rs\n?? untracked.txt\nA  added.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 2);
        assert_eq!(status.unstaged, 2);
    }

    #[test]
    fn test_parse_short_line_ignored() {
        let output = "X\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 0);
        assert_eq!(status.unstaged, 0);
    }

    #[test]
    fn test_parse_added_file() {
        let output = "A  new_file.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 1);
        assert_eq!(status.unstaged, 0);
    }

    #[test]
    fn test_parse_deleted_file() {
        let output = "D  removed.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 1);
        assert_eq!(status.unstaged, 0);
    }

    #[test]
    fn test_parse_renamed_file() {
        let output = "R  old.rs -> new.rs\n";
        let status = parse_git_status(output);
        assert_eq!(status.staged, 1);
        assert_eq!(status.unstaged, 0);
    }

    #[test]
    fn test_poll_skip_set_excludes_done_worktrees() {
        let skip: HashSet<String> = ["done-worktree".to_string()].into_iter().collect();
        let result = poll_all_worktrees(std::path::Path::new("/nonexistent"), &skip);
        assert!(result.statuses.is_empty());
    }
}
