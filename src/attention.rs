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
    /// Own PR: approved and green, but in conflict with its base. Merely
    /// being behind is not this: a merge queue rebases on enqueue.
    NeedsRebase,
    /// Own PR: approved and green, but GitHub blocks the merge, typically on
    /// an unresolved conversation.
    Blocked,
    /// Own PR: in the merge queue. Shown on the card, but left out of the
    /// needs-you count and filter: the queue owns it until it merges or gets
    /// ejected.
    Queued,
    /// The card's agent is stopped on a permission prompt or a plan approval.
    AgentBlocked,
    /// The card's agent reported an error.
    AgentError,
}

impl Attention {
    pub fn needs_you(self) -> bool {
        !matches!(self, Attention::Queued)
    }
}
