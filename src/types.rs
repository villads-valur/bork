use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Column {
    Planning,
    InProgress,
    CodeReview,
    Done,
}

impl Column {
    pub const ALL: [Column; 4] = [
        Column::Planning,
        Column::InProgress,
        Column::CodeReview,
        Column::Done,
    ];

    pub fn index(self) -> usize {
        match self {
            Column::Planning => 0,
            Column::InProgress => 1,
            Column::CodeReview => 2,
            Column::Done => 3,
        }
    }

    pub fn from_index(i: usize) -> Option<Column> {
        match i {
            0 => Some(Column::Planning),
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
            Column::Planning => write!(f, "Planning"),
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
pub enum AgentStatus {
    Idle,
    Busy,
    NeedsAttention,
    Stopped,
}

impl AgentStatus {
    pub fn symbol(self) -> &'static str {
        match self {
            AgentStatus::Idle => "○",
            AgentStatus::Busy => "●",
            AgentStatus::NeedsAttention => "◈",
            AgentStatus::Stopped => "◌",
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentStatus::Idle => write!(f, "idle"),
            AgentStatus::Busy => write!(f, "busy"),
            AgentStatus::NeedsAttention => write!(f, "attention"),
            AgentStatus::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub column: Column,
    pub branch: Option<String>,
    pub tmux_session: Option<String>,
    pub agent_kind: AgentKind,
    pub agent_status: AgentStatus,
}

impl Issue {
    pub fn session_name(&self) -> String {
        format!("bork-{}", self.id.to_lowercase())
    }
}
