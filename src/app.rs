use std::collections::HashSet;

use crate::config::{AppConfig, AppState};
use crate::types::{AgentKind, AgentStatus, Column, Issue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Confirm,
}

pub struct App {
    pub issues: Vec<Issue>,
    pub selected_column: usize,
    pub selected_row: [usize; 4],
    pub active_sessions: HashSet<String>,
    pub input_mode: InputMode,
    pub confirm_message: Option<String>,
    pub confirm_session: Option<String>,
    pub should_quit: bool,
    pub message: Option<String>,
    pub busy_count: usize,
    pub spinner_tick: usize,
    pub config: AppConfig,
}

impl App {
    pub fn new(config: AppConfig, state: AppState) -> Self {
        let issues = if state.issues.is_empty() {
            sample_issues()
        } else {
            state.issues
        };

        App {
            issues,
            selected_column: 0,
            selected_row: [0; 4],
            active_sessions: HashSet::new(),
            input_mode: InputMode::Normal,
            confirm_message: None,
            confirm_session: None,
            should_quit: false,
            message: None,
            busy_count: 0,
            spinner_tick: 0,
            config,
        }
    }

    pub fn issues_in_column(&self, column: Column) -> Vec<(usize, &Issue)> {
        self.issues
            .iter()
            .enumerate()
            .filter(|(_, issue)| issue.column == column)
            .collect()
    }

    pub fn selected_issue(&self) -> Option<&Issue> {
        let column = Column::from_index(self.selected_column)?;
        let items = self.issues_in_column(column);
        let row = self.selected_row[self.selected_column];
        items.get(row).map(|(_, issue)| *issue)
    }

    pub fn selected_issue_index(&self) -> Option<usize> {
        let column = Column::from_index(self.selected_column)?;
        let items = self.issues_in_column(column);
        let row = self.selected_row[self.selected_column];
        items.get(row).map(|(idx, _)| *idx)
    }

    pub fn move_selection_up(&mut self) {
        let row = &mut self.selected_row[self.selected_column];
        if *row > 0 {
            *row -= 1;
        }
    }

    pub fn move_selection_down(&mut self) {
        let column = match Column::from_index(self.selected_column) {
            Some(c) => c,
            None => return,
        };
        let count = self.issues_in_column(column).len();
        let row = &mut self.selected_row[self.selected_column];
        if count > 0 && *row < count - 1 {
            *row += 1;
        }
    }

    /// Tab/Shift+Tab: jump to next/prev column, preserving row position
    pub fn jump_column_left(&mut self) {
        if self.selected_column > 0 {
            self.selected_column -= 1;
            self.clamp_row();
        }
    }

    pub fn jump_column_right(&mut self) {
        if self.selected_column < 3 {
            self.selected_column += 1;
            self.clamp_row();
        }
    }

    /// h/l: move to the adjacent issue in the flat reading order across all columns.
    /// At the bottom of a column, `l` moves to the top of the next column.
    /// At the top of a column, `h` moves to the bottom of the previous column.
    pub fn focus_left(&mut self) {
        let row = self.selected_row[self.selected_column];
        if row > 0 {
            self.selected_row[self.selected_column] = row - 1;
        } else {
            // Move to the bottom of the previous non-empty column
            let mut col = self.selected_column;
            while col > 0 {
                col -= 1;
                let count = self.column_count(col);
                if count > 0 {
                    self.selected_column = col;
                    self.selected_row[col] = count - 1;
                    return;
                }
            }
        }
    }

    pub fn focus_right(&mut self) {
        let column = match Column::from_index(self.selected_column) {
            Some(c) => c,
            None => return,
        };
        let count = self.issues_in_column(column).len();
        let row = self.selected_row[self.selected_column];

        if count > 0 && row < count - 1 {
            self.selected_row[self.selected_column] = row + 1;
        } else {
            // Move to the top of the next non-empty column
            let mut col = self.selected_column;
            while col < 3 {
                col += 1;
                let count = self.column_count(col);
                if count > 0 {
                    self.selected_column = col;
                    self.selected_row[col] = 0;
                    return;
                }
            }
        }
    }

    fn column_count(&self, col_index: usize) -> usize {
        match Column::from_index(col_index) {
            Some(c) => self.issues_in_column(c).len(),
            None => 0,
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.selected_row[self.selected_column] = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        let column = match Column::from_index(self.selected_column) {
            Some(c) => c,
            None => return,
        };
        let count = self.issues_in_column(column).len();
        if count > 0 {
            self.selected_row[self.selected_column] = count - 1;
        }
    }

    pub fn move_issue_right(&mut self) {
        if let Some(idx) = self.selected_issue_index() {
            if let Some(next) = self.issues[idx].column.next() {
                self.issues[idx].column = next;
            }
        }
    }

    pub fn move_issue_left(&mut self) {
        if let Some(idx) = self.selected_issue_index() {
            if let Some(prev) = self.issues[idx].column.prev() {
                self.issues[idx].column = prev;
            }
        }
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
    }

    pub fn to_state(&self) -> AppState {
        AppState {
            issues: self.issues.clone(),
        }
    }

    pub fn spinner_frame(&self) -> &'static str {
        const FRAMES: &[&str] = &[
            "\u{28cb}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
            "\u{2827}", "\u{2807}", "\u{280f}",
        ];
        FRAMES[self.spinner_tick % FRAMES.len()]
    }

    pub fn is_session_alive(&self, session_name: &str) -> bool {
        self.active_sessions.contains(session_name)
    }

    fn clamp_row(&mut self) {
        let column = match Column::from_index(self.selected_column) {
            Some(c) => c,
            None => return,
        };
        let count = self.issues_in_column(column).len();
        let row = &mut self.selected_row[self.selected_column];
        if count == 0 {
            *row = 0;
        } else if *row >= count {
            *row = count - 1;
        }
    }
}

fn sample_issues() -> Vec<Issue> {
    vec![
        // Planning
        Issue {
            id: "BORK-1".to_string(),
            title: "Set up CI pipeline".to_string(),
            column: Column::Planning,
            branch: None,
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Stopped,
        },
        Issue {
            id: "BORK-5".to_string(),
            title: "Design database schema".to_string(),
            column: Column::Planning,
            branch: None,
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Stopped,
        },
        Issue {
            id: "BORK-9".to_string(),
            title: "Write API documentation".to_string(),
            column: Column::Planning,
            branch: None,
            tmux_session: None,
            agent_kind: AgentKind::Claude,
            agent_status: AgentStatus::Stopped,
        },
        // In Progress
        Issue {
            id: "BORK-2".to_string(),
            title: "Add user authentication".to_string(),
            column: Column::InProgress,
            branch: Some("feat/auth".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Busy,
        },
        Issue {
            id: "BORK-6".to_string(),
            title: "Implement search endpoint".to_string(),
            column: Column::InProgress,
            branch: Some("feat/search".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::Claude,
            agent_status: AgentStatus::NeedsAttention,
        },
        Issue {
            id: "BORK-10".to_string(),
            title: "Add rate limiting".to_string(),
            column: Column::InProgress,
            branch: Some("feat/rate-limit".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Idle,
        },
        Issue {
            id: "BORK-13".to_string(),
            title: "Refactor error handling".to_string(),
            column: Column::InProgress,
            branch: Some("refactor/errors".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Busy,
        },
        // Code Review
        Issue {
            id: "BORK-3".to_string(),
            title: "Fix navigation regression".to_string(),
            column: Column::CodeReview,
            branch: Some("fix/nav".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::Claude,
            agent_status: AgentStatus::Idle,
        },
        Issue {
            id: "BORK-7".to_string(),
            title: "Add WebSocket support".to_string(),
            column: Column::CodeReview,
            branch: Some("feat/ws".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Idle,
        },
        // Done
        Issue {
            id: "BORK-4".to_string(),
            title: "Update dependencies".to_string(),
            column: Column::Done,
            branch: Some("chore/deps".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Stopped,
        },
        Issue {
            id: "BORK-8".to_string(),
            title: "Fix memory leak in worker".to_string(),
            column: Column::Done,
            branch: Some("fix/mem-leak".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::Claude,
            agent_status: AgentStatus::Stopped,
        },
        Issue {
            id: "BORK-11".to_string(),
            title: "Set up monitoring".to_string(),
            column: Column::Done,
            branch: Some("feat/monitoring".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_status: AgentStatus::Stopped,
        },
        Issue {
            id: "BORK-12".to_string(),
            title: "Migrate to new ORM".to_string(),
            column: Column::Done,
            branch: Some("chore/orm".to_string()),
            tmux_session: None,
            agent_kind: AgentKind::Claude,
            agent_status: AgentStatus::Stopped,
        },
    ]
}
