use std::collections::{HashMap, HashSet};
use std::path::Path;

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

    // One `git worktree list --porcelain` resolves every worktree's branch,
    // instead of a `git rev-parse` subprocess per directory per poll.
    let mut batched_branches = list_worktree_branches(project_root);

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
        // Fall back to per-directory rev-parse for repos that aren't worktrees
        // of main/ (e.g. independent clones placed in the container).
        let branch = batched_branches
            .remove(&dir_name)
            .or_else(|| get_branch_name(&path));
        if let Some(branch) = branch {
            branches.insert(dir_name, branch);
        }
    }

    GitPollResult { statuses, branches }
}

fn list_worktree_branches(project_root: &Path) -> HashMap<String, String> {
    let main_dir = project_root.join("main");
    let Some(stdout) = git_output(&main_dir, &["worktree", "list", "--porcelain"]) else {
        return HashMap::new();
    };
    parse_worktree_list(&stdout)
}

/// Parse `git worktree list --porcelain` into dir-name -> branch-name.
/// Detached or bare entries (no `branch` line) are skipped.
pub(crate) fn parse_worktree_list(output: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut current_dir: Option<String> = None;

    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_dir = Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from);
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            if let Some(dir) = current_dir.take() {
                let branch = branch_ref.strip_prefix("refs/heads/").unwrap_or(branch_ref);
                map.insert(dir, branch.to_string());
            }
        } else if line.is_empty() {
            current_dir = None;
        }
    }

    map
}

fn git_output(worktree_path: &Path, args: &[&str]) -> Option<String> {
    let output = crate::external::git_command()
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
    fn test_parse_worktree_list_basic() {
        let output = "\
worktree /Users/me/code/bork/main
HEAD c8977b0deadbeef
branch refs/heads/main

worktree /Users/me/code/bork/bork-14-add-search
HEAD 1234567deadbeef
branch refs/heads/bork-14/add-search
";
        let map = parse_worktree_list(output);
        assert_eq!(map.len(), 2);
        assert_eq!(map["main"], "main");
        assert_eq!(map["bork-14-add-search"], "bork-14/add-search");
    }

    #[test]
    fn test_parse_worktree_list_skips_detached() {
        let output = "\
worktree /Users/me/code/bork/main
HEAD c8977b0deadbeef
branch refs/heads/main

worktree /Users/me/code/bork/detached-wt
HEAD 1234567deadbeef
detached
";
        let map = parse_worktree_list(output);
        assert_eq!(map.len(), 1);
        assert!(!map.contains_key("detached-wt"));
    }

    #[test]
    fn test_parse_worktree_list_empty() {
        assert!(parse_worktree_list("").is_empty());
    }

    #[test]
    fn test_parse_worktree_list_branch_without_refs_prefix() {
        let output = "worktree /tmp/x\nbranch feature/foo\n";
        let map = parse_worktree_list(output);
        assert_eq!(map["x"], "feature/foo");
    }

    #[test]
    fn test_poll_skip_set_excludes_done_worktrees() {
        let skip: HashSet<String> = ["done-worktree".to_string()].into_iter().collect();
        let result = poll_all_worktrees(std::path::Path::new("/nonexistent"), &skip);
        assert!(result.statuses.is_empty());
    }
}
