use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use crate::types::WorktreeStatus;

/// How a worktree is classified for adaptive polling. `Hot` worktrees back an
/// in-progress issue and are refreshed every cycle; `Cold` worktrees (code
/// review, todo, unassigned clones) are refreshed on a slower cadence; `Skip`
/// (done) worktrees are not polled (their status is frozen by the app).
/// Variants are ordered least-to-most frequently polled so `a.max(b)` picks the
/// higher priority when a worktree is shared by issues in different columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PollClass {
    Skip,
    Cold,
    Hot,
}

/// Cold worktrees refresh every N poll cycles, where N is jittered per worktree
/// within this range so cold refreshes don't all land on the same cycle.
pub const COLD_MULTIPLIER_MIN: u32 = 3; // ~15s at a 5s base
pub const COLD_MULTIPLIER_MAX: u32 = 6; // ~30s at a 5s base

/// A global bound on concurrent `git status` subprocesses. Shared across the
/// focused project and every swimlane worker so a machine with many projects
/// open can't spawn dozens of `git status` processes at once. Worktree index
/// locking is per-worktree (`.git/worktrees/<name>/index`), so running status
/// across different worktrees in parallel is safe.
#[derive(Clone)]
pub struct GitStatusPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    available: Mutex<usize>,
    cond: Condvar,
}

/// RAII permit released back to the pool on drop.
struct PoolPermit<'a> {
    pool: &'a GitStatusPool,
}

impl GitStatusPool {
    pub fn new(limit: usize) -> Self {
        GitStatusPool {
            inner: Arc::new(PoolInner {
                available: Mutex::new(limit.max(1)),
                cond: Condvar::new(),
            }),
        }
    }

    fn acquire(&self) -> PoolPermit<'_> {
        let mut available = self
            .inner
            .available
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while *available == 0 {
            available = self
                .inner
                .cond
                .wait(available)
                .unwrap_or_else(|e| e.into_inner());
        }
        *available -= 1;
        PoolPermit { pool: self }
    }
}

impl Drop for PoolPermit<'_> {
    fn drop(&mut self) {
        let mut available = self
            .pool
            .inner
            .available
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *available += 1;
        self.pool.inner.cond.notify_one();
    }
}

pub struct GitPollResult {
    pub statuses: HashMap<String, WorktreeStatus>,
    pub branches: HashMap<String, String>,
}

/// Per-worktree scheduling and cached data. Owned by a single `GitPoller`
/// (one per project worker thread) so it persists across poll cycles.
struct WorktreeState {
    /// The last computed status, reused on cycles where the worktree is not
    /// due for a refresh. `None` until the first successful status.
    status: Option<WorktreeStatus>,
    /// Cold worktrees refresh every `cold_multiplier` cycles; jittered per
    /// worktree so cold refreshes spread across the 15-30s window.
    cold_multiplier: u32,
}

/// Stateful git poller: caches branch-fallback resolution and per-worktree
/// status so unchanged/cold worktrees don't pay a subprocess every cycle.
/// One instance lives on each git worker thread.
pub struct GitPoller {
    project_root: std::path::PathBuf,
    pool: GitStatusPool,
    /// Monotonic cycle counter driving cold-worktree scheduling.
    cycle: u32,
    /// Per-worktree schedule + cached status, keyed by directory name.
    worktrees: HashMap<String, WorktreeState>,
    /// Cached branch-fallback resolution for directories missing from the
    /// batched `git worktree list` (independent clones). `None` means "resolved
    /// and confirmed to have no usable branch", so we don't rev-parse again.
    branch_fallback: HashMap<String, Option<String>>,
}

impl GitPoller {
    pub fn new(project_root: std::path::PathBuf, pool: GitStatusPool) -> Self {
        GitPoller {
            project_root,
            pool,
            cycle: 0,
            worktrees: HashMap::new(),
            branch_fallback: HashMap::new(),
        }
    }

    /// Run one poll cycle. `classify` maps a worktree directory name to its
    /// poll class; `force_all` refreshes every non-skipped worktree regardless
    /// of cadence (used after a wake so user actions get an immediate refresh).
    pub fn poll(&mut self, classify: &dyn Fn(&str) -> PollClass, force_all: bool) -> GitPollResult {
        self.cycle = self.cycle.wrapping_add(1);

        let mut statuses = HashMap::new();
        let mut branches = HashMap::new();

        let Ok(entries) = std::fs::read_dir(&self.project_root) else {
            return GitPollResult { statuses, branches };
        };

        // One `git worktree list --porcelain` resolves every worktree's branch,
        // instead of a `git rev-parse` subprocess per directory per poll.
        let mut batched_branches = list_worktree_branches(&self.project_root);

        let mut seen: HashSet<String> = HashSet::new();

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

            let class = classify(&dir_name);
            if class == PollClass::Skip {
                continue;
            }
            seen.insert(dir_name.clone());

            let due = self.due_for(&dir_name, class, force_all);

            // Branch resolution: prefer the batched list. For dirs git doesn't
            // track as worktrees (independent clones), fall back to a cached
            // `rev-parse` that refreshes on the same cadence as status, so a
            // manual checkout is picked up but idle polls stay subprocess-free.
            let branch = match batched_branches.remove(&dir_name) {
                Some(branch) => Some(branch),
                None => self.resolve_fallback_branch(&dir_name, &path, due),
            };
            if let Some(branch) = branch {
                branches.insert(dir_name.clone(), branch);
            }

            if let Some(status) = self.status_for(&dir_name, &path, due) {
                statuses.insert(dir_name.clone(), status);
            }
        }

        // Drop scheduling/cache entries for directories that vanished or became
        // skipped so state doesn't grow unbounded.
        self.worktrees.retain(|name, _| seen.contains(name));
        self.branch_fallback.retain(|name, _| seen.contains(name));

        GitPollResult { statuses, branches }
    }

    /// Whether a worktree should be refreshed this cycle. Ensures a
    /// `WorktreeState` exists (creating one on first sight forces a refresh).
    fn due_for(&mut self, dir_name: &str, class: PollClass, force_all: bool) -> bool {
        let cycle = self.cycle;
        let state = self
            .worktrees
            .entry(dir_name.to_string())
            .or_insert_with(|| WorktreeState {
                status: None,
                cold_multiplier: cold_multiplier_for(dir_name),
            });
        force_all || state.status.is_none() || is_due(class, cycle, state.cold_multiplier)
    }

    /// Re-run `git status` under the global pool bound when `due`, else reuse
    /// the cached status.
    fn status_for(&mut self, dir_name: &str, path: &Path, due: bool) -> Option<WorktreeStatus> {
        let state = self.worktrees.get_mut(dir_name)?;
        if due {
            let _permit = self.pool.acquire();
            if let Some(fresh) = get_worktree_status(path) {
                state.status = Some(fresh);
            }
        }
        state.status
    }

    /// Branch for a dir absent from the batched worktree list. Re-runs
    /// `rev-parse` (under the pool bound) when `due` so a manual checkout on an
    /// independent clone is picked up; otherwise returns the cached value.
    fn resolve_fallback_branch(
        &mut self,
        dir_name: &str,
        path: &Path,
        due: bool,
    ) -> Option<String> {
        if due || !self.branch_fallback.contains_key(dir_name) {
            let _permit = self.pool.acquire();
            let branch = get_branch_name(path);
            self.branch_fallback
                .insert(dir_name.to_string(), branch.clone());
            branch
        } else {
            self.branch_fallback.get(dir_name).cloned().flatten()
        }
    }
}

/// A cold worktree's refresh multiplier, deterministically jittered by name so
/// cold refreshes across many worktrees don't all land on the same cycle.
fn cold_multiplier_for(dir_name: &str) -> u32 {
    let hash = dir_name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let span = COLD_MULTIPLIER_MAX - COLD_MULTIPLIER_MIN + 1;
    COLD_MULTIPLIER_MIN + (hash % span)
}

/// Whether an already-seen worktree is due for a fresh `git status` this cycle.
/// The first poll of a worktree is handled by the caller (no cached status).
fn is_due(class: PollClass, cycle: u32, cold_multiplier: u32) -> bool {
    match class {
        PollClass::Skip => false,
        PollClass::Hot => true,
        PollClass::Cold => cycle.is_multiple_of(cold_multiplier),
    }
}

/// One-shot poll of every worktree under `project_root`, for CLI paths (e.g.
/// prune) that don't run the adaptive worker. Runs `git status` on each
/// discovered worktree once with no caching or concurrency bound.
pub fn poll_all_worktrees(project_root: &Path, skip: &HashSet<String>) -> GitPollResult {
    let pool = GitStatusPool::new(1);
    let mut poller = GitPoller::new(project_root.to_path_buf(), pool);
    let classify = |name: &str| {
        if skip.contains(name) {
            PollClass::Skip
        } else {
            PollClass::Hot
        }
    };
    poller.poll(&classify, true)
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
    fn test_poll_skips_classified_worktrees() {
        let pool = GitStatusPool::new(2);
        let mut poller = GitPoller::new(std::path::PathBuf::from("/nonexistent"), pool);
        let result = poller.poll(&|_| PollClass::Skip, false);
        assert!(result.statuses.is_empty());
        assert!(result.branches.is_empty());
    }

    #[test]
    fn test_poll_all_worktrees_missing_root() {
        let skip: HashSet<String> = ["done-worktree".to_string()].into_iter().collect();
        let result = poll_all_worktrees(std::path::Path::new("/nonexistent"), &skip);
        assert!(result.statuses.is_empty());
    }

    #[test]
    fn test_pool_bounds_concurrency() {
        // A single-permit pool must serialize acquisitions: the second acquire
        // blocks until the first permit drops.
        let pool = GitStatusPool::new(1);
        {
            let _permit = pool.acquire();
            let available = *pool.inner.available.lock().unwrap();
            assert_eq!(available, 0);
        }
        let available = *pool.inner.available.lock().unwrap();
        assert_eq!(available, 1);
    }

    #[test]
    fn test_cold_multiplier_within_bounds() {
        for name in ["a", "bork-14", "some-longer-worktree-name", ""] {
            let m = cold_multiplier_for(name);
            assert!((COLD_MULTIPLIER_MIN..=COLD_MULTIPLIER_MAX).contains(&m));
        }
    }

    #[test]
    fn test_is_due_hot_always() {
        assert!(is_due(PollClass::Hot, 7, 4));
    }

    #[test]
    fn test_is_due_skip_never() {
        assert!(!is_due(PollClass::Skip, 4, 4));
    }

    #[test]
    fn test_is_due_cold_on_multiplier() {
        assert!(is_due(PollClass::Cold, 8, 4));
        assert!(!is_due(PollClass::Cold, 7, 4));
    }
}
