//! Single-slot undo for board actions.
//!
//! An action records an `UndoEntry` holding the field-level `Change`s it made.
//! Undo reverts each change only if the issue still looks the way the action
//! left it, so edits made since (by agents, the CLI or pollers) are never
//! clobbered. Only column moves are recorded today; new kinds of undoable
//! action add a `Change` variant.

use std::time::{Duration, Instant};

use crate::app::{Project, ProjectId};
use crate::types::{Column, Issue};

/// How long an action stays undoable after it happened.
pub const UNDO_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Column {
        issue_id: String,
        from: Column,
        to: Column,
        done_at_before: Option<u64>,
    },
}

impl Change {
    fn issue_id(&self) -> &str {
        match self {
            Change::Column { issue_id, .. } => issue_id,
        }
    }

    fn is_noop(&self) -> bool {
        match self {
            Change::Column { from, to, .. } => from == to,
        }
    }

    /// Fold a follow-up change to the same issue into this one, keeping the
    /// original starting point. Returns false when the two don't chain.
    fn absorb(&mut self, next: &Change) -> bool {
        match (self, next) {
            (
                Change::Column { to, .. },
                Change::Column {
                    from, to: next_to, ..
                },
            ) => {
                if to != from {
                    return false;
                }
                *to = *next_to;
                true
            }
        }
    }

    /// Revert this change on `project`. Returns false (and leaves the issue
    /// alone) when the issue is gone or has changed since.
    fn revert(&self, project: &mut Project) -> bool {
        match self {
            Change::Column {
                issue_id,
                from,
                to,
                done_at_before,
            } => project.restore_column(issue_id, *to, *from, *done_at_before),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub project_id: ProjectId,
    /// What the action did, e.g. "Moved bork-12 to Done".
    pub label: String,
    pub created_at: Instant,
    pub changes: Vec<Change>,
}

impl UndoEntry {
    pub fn new(project_id: ProjectId, label: impl Into<String>, changes: Vec<Change>) -> Self {
        UndoEntry {
            project_id,
            label: label.into(),
            created_at: Instant::now(),
            changes,
        }
    }

    pub fn is_live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created_at) < UNDO_WINDOW
    }

    /// Merge `next` into this entry when it continues the same action on the
    /// same issues (e.g. pressing `L` three times), so one undo returns them
    /// to where they started. Leaves `self` untouched when they don't chain.
    fn try_absorb(&mut self, next: &UndoEntry) -> bool {
        if self.project_id != next.project_id || self.changes.len() != next.changes.len() {
            return false;
        }
        let mut merged = self.changes.clone();
        for next_change in &next.changes {
            let Some(change) = merged.iter_mut().find(|change| {
                change
                    .issue_id()
                    .eq_ignore_ascii_case(next_change.issue_id())
            }) else {
                return false;
            };
            if !change.absorb(next_change) {
                return false;
            }
        }
        merged.retain(|change| !change.is_noop());
        self.changes = merged;
        self.label = next.label.clone();
        self.created_at = next.created_at;
        true
    }

    /// Revert every change, newest first. Returns how many were reverted.
    pub fn revert(&self, project: &mut Project) -> usize {
        self.changes
            .iter()
            .rev()
            .filter(|change| change.revert(project))
            .count()
    }
}

/// Store `next` as the undoable action, merging it into the current entry when
/// it continues it. Returns whether there is something to undo afterwards:
/// false when `next` changed nothing or cancelled out the current entry.
pub fn record(slot: &mut Option<UndoEntry>, next: UndoEntry) -> bool {
    if next.changes.is_empty() {
        return false;
    }
    if let Some(current) = slot.as_mut() {
        if current.is_live(next.created_at) && current.try_absorb(&next) {
            if current.changes.is_empty() {
                *slot = None;
                return false;
            }
            return true;
        }
    }
    *slot = Some(next);
    true
}

/// Each issue's column before an action runs, diffed afterwards to find what
/// the action moved. Column moves never reorder `issues`, so entries line up
/// by index.
pub struct ColumnSnapshot(Vec<(Column, Option<u64>)>);

impl ColumnSnapshot {
    pub fn take(issues: &[Issue]) -> Self {
        ColumnSnapshot(
            issues
                .iter()
                .map(|issue| (issue.column, issue.done_at))
                .collect(),
        )
    }

    pub fn changes_since(&self, issues: &[Issue]) -> Vec<Change> {
        self.0
            .iter()
            .zip(issues)
            .filter(|((column, _), issue)| issue.column != *column)
            .map(|((column, done_at), issue)| Change::Column {
                issue_id: issue.id.clone(),
                from: *column,
                to: issue.column,
                done_at_before: *done_at,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn column_change(id: &str, from: Column, to: Column) -> Change {
        Change::Column {
            issue_id: id.to_string(),
            from,
            to,
            done_at_before: None,
        }
    }

    fn entry(changes: Vec<Change>) -> UndoEntry {
        UndoEntry::new(PathBuf::from("/p"), "Moved", changes)
    }

    #[test]
    fn record_ignores_empty_actions() {
        let mut slot = None;
        record(&mut slot, entry(vec![]));
        assert!(slot.is_none());
    }

    #[test]
    fn record_merges_chained_moves_of_the_same_issue() {
        let mut slot = None;
        record(
            &mut slot,
            entry(vec![column_change("b-1", Column::Todo, Column::InProgress)]),
        );
        record(
            &mut slot,
            entry(vec![column_change(
                "b-1",
                Column::InProgress,
                Column::CodeReview,
            )]),
        );
        let changes = &slot.unwrap().changes;
        assert_eq!(
            changes,
            &vec![column_change("b-1", Column::Todo, Column::CodeReview)]
        );
    }

    #[test]
    fn record_clears_slot_when_moves_cancel_out() {
        let mut slot = None;
        record(
            &mut slot,
            entry(vec![column_change("b-1", Column::Todo, Column::InProgress)]),
        );
        record(
            &mut slot,
            entry(vec![column_change("b-1", Column::InProgress, Column::Todo)]),
        );
        assert!(slot.is_none());
    }

    #[test]
    fn record_replaces_entry_for_a_different_issue() {
        let mut slot = None;
        record(
            &mut slot,
            entry(vec![column_change("b-1", Column::Todo, Column::InProgress)]),
        );
        record(
            &mut slot,
            entry(vec![column_change("b-2", Column::Todo, Column::Done)]),
        );
        assert_eq!(
            slot.unwrap().changes,
            vec![column_change("b-2", Column::Todo, Column::Done)]
        );
    }

    #[test]
    fn record_replaces_expired_entry_instead_of_merging() {
        let mut stale = entry(vec![column_change("b-1", Column::Todo, Column::InProgress)]);
        stale.created_at = Instant::now() - UNDO_WINDOW - Duration::from_secs(1);
        let mut slot = Some(stale);
        record(
            &mut slot,
            entry(vec![column_change(
                "b-1",
                Column::InProgress,
                Column::CodeReview,
            )]),
        );
        assert_eq!(
            slot.unwrap().changes,
            vec![column_change("b-1", Column::InProgress, Column::CodeReview)]
        );
    }

    #[test]
    fn entry_expires_after_window() {
        let entry = entry(vec![]);
        assert!(entry.is_live(entry.created_at));
        assert!(!entry.is_live(entry.created_at + UNDO_WINDOW));
    }
}
