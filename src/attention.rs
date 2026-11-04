use crate::types::MergeQueueState;

/// What a card asks of its owner, worth surfacing over the board's ordinary
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Own PR: approved (or no review required), checks green (or none), and
    /// GitHub's merge-state raises no objection.
    ReadyToMerge,
    /// Own PR: checks failing or erroring.
    ChecksFailing,
    /// Own PR: a reviewer requested changes.
    ChangesRequested,
    /// Own PR: changes requested and checks failing or erroring.
    ChecksAndChanges,
    /// Own PR: in conflict with its base, whatever its review and checks say.
    NeedsRebase,
    /// Own PR: approved and green, but behind a base branch that has no merge
    /// queue to bring it up to date.
    NeedsUpdate,
    /// Own PR: approved and green, but GitHub blocks the merge, typically on
    /// an unresolved conversation.
    Blocked,
    /// Own PR: in a merge queue. While queued or testing it is left out of
    /// the needs-you count and filter: the queue owns it. Failed or
    /// cancelled puts it back on the owner.
    Queue(MergeQueueState),
    /// The card's agent is stopped on a permission prompt or a plan approval.
    AgentBlocked,
    /// The card's agent reported an error.
    AgentError,
}

impl Attention {
    pub fn needs_you(self) -> bool {
        !matches!(
            self,
            Attention::Queue(MergeQueueState::Queued | MergeQueueState::Testing)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failed_or_dequeued_queue_runs_need_you() {
        for (attention, expected) in [
            (Attention::Queue(MergeQueueState::Queued), false),
            (Attention::Queue(MergeQueueState::Testing), false),
            (Attention::Queue(MergeQueueState::Failed), true),
            (Attention::Queue(MergeQueueState::Cancelled), true),
        ] {
            assert_eq!(attention.needs_you(), expected, "{attention:?}");
        }
    }
}
