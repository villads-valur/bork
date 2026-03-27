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
    pub tmux_session: Option<String>,
    pub agent_kind: AgentKind,
    pub agent_mode: AgentMode,
    pub agent_status: AgentStatus,
    pub prompt: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub done_at: Option<u64>,
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

    fn test_issue(id: &str, column: Column) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Test issue {}", id),
            column,
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_mode: AgentMode::Plan,
            agent_status: AgentStatus::Stopped,
            prompt: None,
            worktree: None,
            done_at: None,
        }
    }

    // --- PR types ---

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
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn test_review_decision_is_copy() {
        let a = ReviewDecision::Approved;
        let b = a;
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

    // --- WorktreeStatus ---

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

    // --- Column navigation ---

    #[test]
    fn column_next_from_todo() {
        assert_eq!(Column::Todo.next(), Some(Column::InProgress));
    }

    #[test]
    fn column_next_from_done_is_none() {
        assert_eq!(Column::Done.next(), None);
    }

    #[test]
    fn column_prev_from_todo_is_none() {
        assert_eq!(Column::Todo.prev(), None);
    }

    #[test]
    fn column_prev_from_done() {
        assert_eq!(Column::Done.prev(), Some(Column::CodeReview));
    }

    #[test]
    fn column_roundtrip_index() {
        for col in Column::ALL {
            assert_eq!(Column::from_index(col.index()), Some(col));
        }
    }

    #[test]
    fn column_from_index_out_of_range() {
        assert_eq!(Column::from_index(4), None);
        assert_eq!(Column::from_index(99), None);
    }

    // --- Issue session_name ---

    #[test]
    fn session_name_lowercases_id() {
        let issue = test_issue("BORK-3", Column::Todo);
        assert_eq!(issue.session_name(), "bork-bork-3");
    }

    // --- Issue serialization with done_at ---

    #[test]
    fn issue_serializes_done_at_when_set() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1700000000);
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"done_at\":1700000000"));
    }

    #[test]
    fn issue_deserializes_without_done_at_defaults_to_none() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "tmux_session": null,
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "agent_status": "Stopped",
            "prompt": null
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.done_at, None);
    }

    #[test]
    fn issue_deserializes_with_done_at() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Done",
            "tmux_session": null,
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "agent_status": "Stopped",
            "prompt": null,
            "done_at": 1700000000
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.done_at, Some(1700000000));
    }

    #[test]
    fn column_deserializes_planning_alias_as_todo() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Planning",
            "tmux_session": null,
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "agent_status": "Stopped",
            "prompt": null
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.column, Column::Todo);
    }

    #[test]
    fn issue_ignores_unknown_fields_from_old_state() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "branch": null,
            "worktree": "main",
            "tmux_session": null,
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "agent_status": "Stopped",
            "prompt": null
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.id, "bork-1");
    }

    // --- AgentStatus ---

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
