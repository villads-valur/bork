//! Auto-prune mechanism for stale worktrees.
//!
//! Scans the project root for git worktree directories, classifies each by
//! safety, and runs `git worktree remove` for the user's selection. The
//! corresponding `Issue` records stay on the board; only `issue.worktree` is
//! cleared and `issue.pruned_at` set.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::app::Project;
use crate::types::{Column, Issue, WorktreeStatus};

/// Selected action for a candidate worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneAction {
    Keep,
    Remove,
}

/// A single worktree directory under the project root, with enough metadata
/// for the dialog and the executor to make a decision.
#[derive(Debug, Clone)]
pub struct PruneCandidate {
    pub worktree: String,
    pub issue_id: Option<String>,
    pub issue_column: Option<Column>,
    pub status: Option<WorktreeStatus>,
    pub session_alive: bool,
    /// Current keep/remove selection, seeded with a conservative default:
    /// anything dirty, with a live session, or linked to a non-Done issue
    /// starts as `Keep`; clean Done-issue worktrees and orphans as `Remove`.
    pub action: PruneAction,
}

impl PruneCandidate {
    pub fn new(
        worktree: String,
        status: Option<WorktreeStatus>,
        issue: Option<&Issue>,
        session_alive: bool,
    ) -> Self {
        // `status: None` means the git poll hasn't reached this worktree
        // yet — treat it like dirty for the default so we never pre-select
        // a worktree whose state we don't actually know.
        let clean = status.as_ref().is_some_and(|s| s.is_clean());
        let action = if !clean || session_alive {
            PruneAction::Keep
        } else {
            match issue.map(|i| i.column) {
                None | Some(Column::Done) => PruneAction::Remove,
                Some(_) => PruneAction::Keep,
            }
        };
        PruneCandidate {
            worktree,
            issue_id: issue.map(|i| i.id.clone()),
            issue_column: issue.map(|i| i.column),
            status,
            session_alive,
            action,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.status.as_ref().is_some_and(|s| !s.is_clean())
    }
}

/// Discover worktree directory names directly from disk: every child
/// directory of the project root with a `.git`, except `main/` and dotdirs.
/// Mirrors the discovery rule in `external::git::poll_all_worktrees`.
///
/// This is the single definition of "a prunable worktree". The dialog and
/// the auto-prune prompt both count from here rather than the git poll
/// cache, which fills one slow `git status` at a time and can lag the disk
/// by a minute on projects with hundreds of worktrees.
pub fn discover_worktree_names(project_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(project_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| {
            name != "main"
                && !name.starts_with('.')
                && project_root.join(name).join(".git").exists()
        })
        .collect();
    names.sort();
    names
}

/// Build the candidate list for `project`. Names come from the disk scan;
/// the live git poll cache only enriches them with dirty/session state, so
/// a worktree the poller hasn't reached yet still shows up (with unknown
/// status, defaulting to Keep).
pub fn scan_candidates(project: &Project) -> Vec<PruneCandidate> {
    let names = discover_worktree_names(&project.config.project_root);
    build_candidates(project, names)
}

pub fn build_candidates(project: &Project, names: Vec<String>) -> Vec<PruneCandidate> {
    names
        .into_iter()
        .map(|name| {
            let issue = project
                .issues
                .iter()
                .find(|i| i.worktree.as_deref() == Some(name.as_str()));
            let status = project
                .live
                .worktree_statuses
                .get(&name)
                .or_else(|| project.live.frozen_worktree_statuses.get(&name))
                .cloned();
            let session_alive = issue.is_some_and(|i| {
                project.is_session_alive(&i.session_name(&project.config.project_name))
            });
            PruneCandidate::new(name, status, issue, session_alive)
        })
        .collect()
}

/// Split the current selection into worktrees that are safe to remove and
/// dirty ones the caller must refuse. Both the dialog and the CLI make this
/// same decision, so the policy lives here.
pub fn partition_selection(candidates: &[PruneCandidate]) -> (Vec<String>, Vec<String>) {
    let mut safe = Vec::new();
    let mut dirty = Vec::new();
    for c in candidates {
        if c.action != PruneAction::Remove {
            continue;
        }
        if c.is_dirty() {
            dirty.push(c.worktree.clone());
        } else {
            safe.push(c.worktree.clone());
        }
    }
    (safe, dirty)
}

/// Outcome of running prune on a single worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct PruneResult {
    pub worktree: String,
    pub outcome: RemoveOutcome,
}

#[derive(Debug, Clone)]
pub struct PruneOutcome {
    pub results: Vec<PruneResult>,
}

impl PruneOutcome {
    pub fn removed_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == RemoveOutcome::Removed)
            .count()
    }

    pub fn failed_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, RemoveOutcome::Failed(_)))
            .count()
    }

    pub fn removed_worktrees(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|r| r.outcome == RemoveOutcome::Removed)
            .map(|r| r.worktree.as_str())
            .collect()
    }
}

/// Run `git worktree remove` for each worktree. We deliberately never pass
/// `--force`, so git itself refuses dirty or otherwise unsafe removals at
/// execute time and we surface its stderr. Returns one result per worktree.
pub fn execute_removals(project_root: &Path, worktrees: &[String]) -> PruneOutcome {
    let main_dir = project_root.join("main");
    let results = worktrees
        .iter()
        .map(|name| PruneResult {
            worktree: name.clone(),
            outcome: run_git_worktree_remove(&main_dir, &project_root.join(name)),
        })
        .collect();
    PruneOutcome { results }
}

fn run_git_worktree_remove(main_dir: &Path, worktree_path: &Path) -> RemoveOutcome {
    let output = Command::new("git")
        .args(["worktree", "remove"])
        .arg(worktree_path)
        .current_dir(main_dir)
        .output();

    match output {
        Ok(out) if out.status.success() => RemoveOutcome::Removed,
        Ok(out) => {
            let msg = String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("git worktree remove failed")
                .trim()
                .to_string();
            RemoveOutcome::Failed(msg)
        }
        Err(e) => RemoveOutcome::Failed(format!("failed to spawn git: {e}")),
    }
}

/// Apply the result of a prune run back to the in-memory issue list. Returns
/// `true` if any issue was mutated so the caller can mark the project dirty.
pub fn apply_outcome_to_issues(issues: &mut [Issue], outcome: &PruneOutcome, now: u64) -> bool {
    let mut changed = false;
    let removed: HashSet<&str> = outcome.removed_worktrees().into_iter().collect();
    if removed.is_empty() {
        return false;
    }
    for issue in issues {
        let Some(wt) = issue.worktree.as_deref() else {
            continue;
        };
        if removed.contains(wt) {
            issue.worktree = None;
            issue.pruned_at = Some(now);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WorktreeStatus;

    fn candidate(
        worktree: &str,
        column: Option<Column>,
        dirty: bool,
        session_alive: bool,
    ) -> PruneCandidate {
        let status = Some(WorktreeStatus {
            staged: 0,
            unstaged: if dirty { 1 } else { 0 },
        });
        let issue = column.map(|c| {
            let mut i = make_issue("bork-1", Some(worktree));
            i.column = c;
            i
        });
        PruneCandidate::new(worktree.into(), status, issue.as_ref(), session_alive)
    }

    #[test]
    fn done_clean_no_session_defaults_remove() {
        let c = candidate("wt", Some(Column::Done), false, false);
        assert_eq!(c.action, PruneAction::Remove);
    }

    #[test]
    fn orphan_clean_defaults_remove() {
        let c = candidate("wt", None, false, false);
        assert_eq!(c.action, PruneAction::Remove);
    }

    #[test]
    fn in_progress_clean_defaults_keep() {
        let c = candidate("wt", Some(Column::InProgress), false, false);
        assert_eq!(c.action, PruneAction::Keep);
    }

    #[test]
    fn todo_clean_defaults_keep() {
        let c = candidate("wt", Some(Column::Todo), false, false);
        assert_eq!(c.action, PruneAction::Keep);
    }

    #[test]
    fn code_review_clean_defaults_keep() {
        let c = candidate("wt", Some(Column::CodeReview), false, false);
        assert_eq!(c.action, PruneAction::Keep);
    }

    #[test]
    fn dirty_always_defaults_keep() {
        let c = candidate("wt", Some(Column::Done), true, false);
        assert_eq!(c.action, PruneAction::Keep);
    }

    #[test]
    fn live_session_defaults_keep() {
        let c = candidate("wt", Some(Column::Done), false, true);
        assert_eq!(c.action, PruneAction::Keep);
    }

    #[test]
    fn discover_finds_git_dirs_excluding_main_and_hidden() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        // Real worktrees have a `.git` file; a `.git` dir (plain clone)
        // must count too. Both spellings are covered here.
        for name in ["main", "wt-b"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join(".git"), "gitdir: elsewhere").unwrap();
        }
        std::fs::create_dir_all(root.join("wt-a").join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".bork")).unwrap();
        std::fs::create_dir_all(root.join("not-a-repo")).unwrap();

        let names = discover_worktree_names(root);
        assert_eq!(names, vec!["wt-a".to_string(), "wt-b".to_string()]);
    }

    #[test]
    fn discover_missing_root_returns_empty() {
        assert!(discover_worktree_names(Path::new("/nonexistent/bork-prune-test")).is_empty());
    }

    #[test]
    fn unknown_status_defaults_keep() {
        let c = PruneCandidate::new("wt".into(), None, None, false);
        assert_eq!(c.action, PruneAction::Keep);
        assert!(!c.is_dirty());
    }

    #[test]
    fn partition_splits_safe_and_dirty_removals() {
        let clean = candidate("clean-wt", Some(Column::Done), false, false);
        let mut dirty = candidate("dirty-wt", Some(Column::Done), true, false);
        dirty.action = PruneAction::Remove;
        let mut kept = candidate("kept-wt", Some(Column::Done), false, false);
        kept.action = PruneAction::Keep;

        let (safe, dirty_names) = partition_selection(&[clean, dirty, kept]);
        assert_eq!(safe, vec!["clean-wt".to_string()]);
        assert_eq!(dirty_names, vec!["dirty-wt".to_string()]);
    }

    fn make_issue(id: &str, worktree: Option<&str>) -> Issue {
        Issue {
            worktree: worktree.map(String::from),
            ..Issue::new(id, "t", Column::Done, crate::types::AgentKind::OpenCode)
        }
    }

    #[test]
    fn apply_outcome_clears_worktree_and_sets_pruned_at() {
        let mut issues = vec![
            make_issue("bork-1", Some("wt-1")),
            make_issue("bork-2", Some("wt-2")),
        ];
        let outcome = PruneOutcome {
            results: vec![PruneResult {
                worktree: "wt-1".into(),
                outcome: RemoveOutcome::Removed,
            }],
        };
        let changed = apply_outcome_to_issues(&mut issues, &outcome, 12345);
        assert!(changed);
        assert_eq!(issues[0].worktree, None);
        assert_eq!(issues[0].pruned_at, Some(12345));
        // Unaffected issue is unchanged.
        assert_eq!(issues[1].worktree, Some("wt-2".into()));
        assert_eq!(issues[1].pruned_at, None);
    }

    #[test]
    fn apply_outcome_skips_when_nothing_removed() {
        let mut issues = vec![make_issue("bork-1", Some("wt-1"))];
        let outcome = PruneOutcome {
            results: vec![PruneResult {
                worktree: "wt-1".into(),
                outcome: RemoveOutcome::Failed("dirty".into()),
            }],
        };
        let changed = apply_outcome_to_issues(&mut issues, &outcome, 12345);
        assert!(!changed);
        assert_eq!(issues[0].worktree, Some("wt-1".into()));
        assert_eq!(issues[0].pruned_at, None);
    }

    #[test]
    fn apply_outcome_handles_multiple_removals() {
        let mut issues = vec![
            make_issue("bork-1", Some("wt-1")),
            make_issue("bork-2", Some("wt-2")),
            make_issue("bork-3", Some("wt-3")),
        ];
        let outcome = PruneOutcome {
            results: vec![
                PruneResult {
                    worktree: "wt-1".into(),
                    outcome: RemoveOutcome::Removed,
                },
                PruneResult {
                    worktree: "wt-3".into(),
                    outcome: RemoveOutcome::Removed,
                },
                PruneResult {
                    worktree: "wt-2".into(),
                    outcome: RemoveOutcome::Failed("nope".into()),
                },
            ],
        };
        let changed = apply_outcome_to_issues(&mut issues, &outcome, 42);
        assert!(changed);
        assert_eq!(issues[0].pruned_at, Some(42));
        assert_eq!(issues[0].worktree, None);
        // Failed removal does NOT mutate the matching issue.
        assert_eq!(issues[1].pruned_at, None);
        assert_eq!(issues[1].worktree, Some("wt-2".into()));
        assert_eq!(issues[2].pruned_at, Some(42));
        assert_eq!(issues[2].worktree, None);
    }

    #[test]
    fn apply_outcome_does_not_touch_unrelated_worktree() {
        let mut issues = vec![make_issue("bork-1", Some("other-wt"))];
        let outcome = PruneOutcome {
            results: vec![PruneResult {
                worktree: "wt-1".into(),
                outcome: RemoveOutcome::Removed,
            }],
        };
        let changed = apply_outcome_to_issues(&mut issues, &outcome, 1);
        assert!(!changed);
        assert_eq!(issues[0].worktree, Some("other-wt".into()));
        assert_eq!(issues[0].pruned_at, None);
    }

    #[test]
    fn outcome_counts_match_results() {
        let outcome = PruneOutcome {
            results: vec![
                PruneResult {
                    worktree: "a".into(),
                    outcome: RemoveOutcome::Removed,
                },
                PruneResult {
                    worktree: "b".into(),
                    outcome: RemoveOutcome::Removed,
                },
                PruneResult {
                    worktree: "d".into(),
                    outcome: RemoveOutcome::Failed("boom".into()),
                },
            ],
        };
        assert_eq!(outcome.removed_count(), 2);
        assert_eq!(outcome.failed_count(), 1);
        assert_eq!(outcome.removed_worktrees(), vec!["a", "b"]);
    }
}
