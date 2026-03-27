use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Column {
    #[serde(alias = "Planning")]
    Todo,
    InProgress,
    CodeReview,
    Done,
}

impl Column {
    pub const ALL: [Column; 4] = [
        Column::Todo,
        Column::InProgress,
        Column::CodeReview,
        Column::Done,
    ];

    pub fn index(self) -> usize {
        match self {
            Column::Todo => 0,
            Column::InProgress => 1,
            Column::CodeReview => 2,
            Column::Done => 3,
        }
    }

    pub fn from_index(i: usize) -> Option<Column> {
        match i {
            0 => Some(Column::Todo),
            1 => Some(Column::InProgress),
            2 => Some(Column::CodeReview),
            3 => Some(Column::Done),
            _ => None,
        }
    }

    pub fn next(self) -> Option<Column> {
        Column::from_index(self.index() + 1)
    }

    pub fn prev(self) -> Option<Column> {
        if self.index() == 0 {
            None
        } else {
            Column::from_index(self.index() - 1)
        }
    }
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Column::Todo => write!(f, "To Do"),
            Column::InProgress => write!(f, "In Progress"),
            Column::CodeReview => write!(f, "Code Review"),
            Column::Done => write!(f, "Done"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    OpenCode,
    Claude,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentKind::OpenCode => write!(f, "opencode"),
            AgentKind::Claude => write!(f, "claude"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMode {
    Plan,
    Build,
}

impl AgentMode {
    pub fn toggle(self) -> Self {
        match self {
            AgentMode::Plan => AgentMode::Build,
            AgentMode::Build => AgentMode::Plan,
        }
    }
}

impl fmt::Display for AgentMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentMode::Plan => write!(f, "plan"),
            AgentMode::Build => write!(f, "build"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Stopped,
    Idle,
    Busy,
    WaitingInput,
    WaitingPermission,
    WaitingApproval,
    Error,
}

impl AgentStatus {
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::WaitingInput | Self::WaitingPermission | Self::WaitingApproval
        )
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Stopped => "◌",
            Self::Idle => "○",
            Self::Busy => "●",
            Self::WaitingInput | Self::WaitingPermission | Self::WaitingApproval => "◈",
            Self::Error => "✗",
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped"),
            Self::Idle => write!(f, "idle"),
            Self::Busy => write!(f, "busy"),
            Self::WaitingInput => write!(f, "waiting for input"),
            Self::WaitingPermission => write!(f, "needs permission"),
            Self::WaitingApproval => write!(f, "needs approval"),
            Self::Error => write!(f, "error"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusInfo {
    pub status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone)]
pub struct WorktreeStatus {
    pub staged: usize,
    pub unstaged: usize,
}

impl WorktreeStatus {
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.unstaged == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub column: Column,
    pub branch: Option<String>,
    #[serde(
        default = "default_worktree",
        deserialize_with = "deserialize_worktree"
    )]
    pub worktree: Option<String>,
    pub tmux_session: Option<String>,
    pub agent_kind: AgentKind,
    pub agent_mode: AgentMode,
    pub agent_status: AgentStatus,
    pub prompt: Option<String>,
}

fn default_worktree() -> Option<String> {
    Some("main".into())
}

fn deserialize_worktree<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::deserialize(deserializer)?.or_else(default_worktree))
}

impl Issue {
    pub fn session_name(&self) -> String {
        format!("bork-{}", self.id.to_lowercase())
    }
}

// --- PR types (ephemeral, not persisted) ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

impl fmt::Display for PrState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrState::Open => write!(f, "open"),
            PrState::Closed => write!(f, "closed"),
            PrState::Merged => write!(f, "merged"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksStatus {
    Success,
    Failure,
    Pending,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone)]
pub struct PrStatus {
    pub number: u32,
    pub state: PrState,
    pub is_draft: bool,
    pub checks: Option<ChecksStatus>,
    pub review: Option<ReviewDecision>,
    pub additions: u32,
    pub deletions: u32,
    pub head_branch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pr_state_display() {
        assert_eq!(PrState::Open.to_string(), "open");
        assert_eq!(PrState::Closed.to_string(), "closed");
        assert_eq!(PrState::Merged.to_string(), "merged");
    }

    #[test]
    fn test_pr_state_equality() {
        assert_eq!(PrState::Open, PrState::Open);
        assert_ne!(PrState::Open, PrState::Closed);
    }

    #[test]
    fn test_checks_status_is_copy() {
        let a = ChecksStatus::Success;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn test_review_decision_is_copy() {
        let a = ReviewDecision::Approved;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn test_pr_status_clone() {
        let pr = PrStatus {
            number: 1,
            state: PrState::Open,
            is_draft: false,
            checks: Some(ChecksStatus::Success),
            review: Some(ReviewDecision::Approved),
            additions: 10,
            deletions: 5,
            head_branch: "main".into(),
        };
        let cloned = pr.clone();
        assert_eq!(cloned.number, 1);
        assert_eq!(cloned.state, PrState::Open);
        assert_eq!(cloned.checks, Some(ChecksStatus::Success));
        assert_eq!(cloned.review, Some(ReviewDecision::Approved));
        assert_eq!(cloned.head_branch, "main");
    }

    #[test]
    fn test_worktree_status_is_clean() {
        assert!(WorktreeStatus {
            staged: 0,
            unstaged: 0
        }
        .is_clean());
        assert!(!WorktreeStatus {
            staged: 1,
            unstaged: 0
        }
        .is_clean());
        assert!(!WorktreeStatus {
            staged: 0,
            unstaged: 1
        }
        .is_clean());
    }

    #[test]
    fn test_issue_session_name() {
        let issue = Issue {
            id: "BORK-42".into(),
            title: "test".into(),
            column: Column::Todo,
            branch: None,
            worktree: Some("main".into()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_mode: AgentMode::Plan,
            agent_status: AgentStatus::Stopped,
            prompt: None,
        };
        assert_eq!(issue.session_name(), "bork-bork-42");
    }

    #[test]
    fn test_column_navigation() {
        assert_eq!(Column::Todo.next(), Some(Column::InProgress));
        assert_eq!(Column::Done.next(), None);
        assert_eq!(Column::Todo.prev(), None);
        assert_eq!(Column::Done.prev(), Some(Column::CodeReview));
    }

    #[test]
    fn test_agent_status_symbol() {
        assert_eq!(AgentStatus::Stopped.symbol(), "◌");
        assert_eq!(AgentStatus::Idle.symbol(), "○");
        assert_eq!(AgentStatus::Busy.symbol(), "●");
        assert_eq!(AgentStatus::WaitingInput.symbol(), "◈");
        assert_eq!(AgentStatus::Error.symbol(), "✗");
    }

    #[test]
    fn test_agent_mode_toggle() {
        assert_eq!(AgentMode::Plan.toggle(), AgentMode::Build);
        assert_eq!(AgentMode::Build.toggle(), AgentMode::Plan);
    }
}
