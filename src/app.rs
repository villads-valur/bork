use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::config::{AppConfig, AppState};
use crate::types::{
    AgentMode, AgentStatus, AgentStatusInfo, Column, Issue, PrStatus, WorktreeStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Confirm,
    Dialog,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    KillSession { session_name: String },
    DeleteIssue { issue_index: usize },
}

pub struct DialogState {
    pub title: String,
    pub prompt: String,
    pub worktree: String,
    pub agent_mode: AgentMode,
    pub focused_field: usize, // 0=title, 1=prompt, 2=worktree, 3=mode
    pub editing_index: Option<usize>,
}

impl DialogState {
    pub fn new() -> Self {
        DialogState {
            title: String::new(),
            prompt: String::new(),
            worktree: "main".into(),
            agent_mode: AgentMode::Plan,
            focused_field: 0,
            editing_index: None,
        }
    }

    pub fn from_issue(issue: &Issue, index: usize) -> Self {
        DialogState {
            title: issue.title.clone(),
            prompt: issue.prompt.clone().unwrap_or_default(),
            worktree: issue.worktree.clone().unwrap_or_default(),
            agent_mode: issue.agent_mode,
            focused_field: 0,
            editing_index: Some(index),
        }
    }

    pub fn push_char(&mut self, c: char) {
        match self.focused_field {
            0 => self.title.push(c),
            1 => self.prompt.push(c),
            2 => self.worktree.push(c),
            3 => {
                // On mode field: space/h/l toggle
                if c == ' ' || c == 'h' || c == 'l' {
                    self.agent_mode = self.agent_mode.toggle();
                }
            }
            _ => {}
        }
    }

    pub fn delete_char(&mut self) {
        match self.focused_field {
            0 => {
                self.title.pop();
            }
            1 => {
                self.prompt.pop();
            }
            2 => {
                self.worktree.pop();
            }
            _ => {}
        }
    }
}

pub const DIALOG_FIELD_COUNT: usize = 4;

pub struct App {
    pub issues: Vec<Issue>,
    pub selected_column: usize,
    pub selected_row: [usize; 4],
    pub active_sessions: HashSet<String>,
    pub agent_statuses: HashMap<String, AgentStatusInfo>,
    pub worktree_statuses: HashMap<String, WorktreeStatus>,
    pub worktree_branches: HashMap<String, String>,
    pub pr_statuses: HashMap<String, PrStatus>,
    pub input_mode: InputMode,
    pub confirm_message: Option<String>,
    pub pending_confirm: Option<ConfirmAction>,
    pub dialog: Option<DialogState>,
    pub should_quit: bool,
    pub message: Option<String>,
    pub message_set_at: Option<Instant>,
    pub busy_count: usize,
    pub spinner_tick: usize,
    pub config: AppConfig,
}

impl App {
    pub fn new(config: AppConfig, state: AppState) -> Self {
        App {
            issues: state.issues,
            selected_column: 0,
            selected_row: [0; 4],
            active_sessions: HashSet::new(),
            agent_statuses: HashMap::new(),
            worktree_statuses: HashMap::new(),
            worktree_branches: HashMap::new(),
            pr_statuses: HashMap::new(),
            input_mode: InputMode::Normal,
            confirm_message: None,
            pending_confirm: None,
            dialog: None,
            should_quit: false,
            message: None,
            message_set_at: None,
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

    pub fn focus_left(&mut self) {
        let row = self.selected_row[self.selected_column];
        if row > 0 {
            self.selected_row[self.selected_column] = row - 1;
        } else {
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
        self.message_set_at = Some(Instant::now());
    }

    pub fn clear_expired_message(&mut self) {
        if let Some(set_at) = self.message_set_at {
            if set_at.elapsed().as_secs() >= 3 {
                self.message = None;
                self.message_set_at = None;
            }
        }
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

    pub fn resolved_agent_status(&self, issue: &Issue) -> AgentStatus {
        let session_name = issue.session_name();

        if let Some(info) = self.agent_statuses.get(&session_name) {
            // Cross-reference with session liveness: if session is dead but
            // status file says Busy/Idle, override to Stopped (stale file)
            if !self.is_session_alive(&session_name) {
                return AgentStatus::Stopped;
            }
            return info.status;
        }

        if self.is_session_alive(&session_name) {
            return AgentStatus::Idle;
        }

        AgentStatus::Stopped
    }

    pub fn resolved_activity(&self, issue: &Issue) -> Option<&str> {
        let session_name = issue.session_name();
        self.agent_statuses
            .get(&session_name)
            .and_then(|info| info.activity.as_deref())
    }

    pub fn worktree_status_for(&self, issue: &Issue) -> Option<&WorktreeStatus> {
        issue
            .worktree
            .as_ref()
            .and_then(|w| self.worktree_statuses.get(w))
    }

    pub fn branch_for(&self, issue: &Issue) -> Option<&str> {
        issue
            .worktree
            .as_ref()
            .and_then(|w| self.worktree_branches.get(w))
            .map(|s| s.as_str())
    }

    pub fn pr_for(&self, issue: &Issue) -> Option<&PrStatus> {
        let branch = self.branch_for(issue)?;
        self.pr_statuses.get(branch)
    }

    // --- Dialog ---

    pub fn open_dialog(&mut self) {
        self.dialog = Some(DialogState::new());
        self.input_mode = InputMode::Dialog;
    }

    pub fn open_edit_dialog(&mut self, issue: &Issue, index: usize) {
        self.dialog = Some(DialogState::from_issue(issue, index));
        self.input_mode = InputMode::Dialog;
    }

    pub fn close_dialog(&mut self) {
        self.dialog = None;
        self.input_mode = InputMode::Normal;
    }

    // --- Issue ID generation ---

    pub fn next_issue_id(&self) -> String {
        let prefix = &self.config.project_name;
        let max_num = self
            .issues
            .iter()
            .filter_map(|issue| {
                let id = &issue.id;
                if let Some(suffix) = id.strip_prefix(&format!("{}-", prefix)) {
                    suffix.parse::<u32>().ok()
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0);

        format!("{}-{}", prefix, max_num + 1)
    }

    // --- Confirm ---

    pub fn start_confirm(&mut self, message: String, action: ConfirmAction) {
        self.input_mode = InputMode::Confirm;
        self.confirm_message = Some(message);
        self.pending_confirm = Some(action);
    }

    pub fn cancel_confirm(&mut self) {
        self.input_mode = InputMode::Normal;
        self.confirm_message = None;
        self.pending_confirm = None;
    }

    pub fn take_confirm_action(&mut self) -> Option<ConfirmAction> {
        self.input_mode = InputMode::Normal;
        self.confirm_message = None;
        self.pending_confirm.take()
    }

    pub fn clamp_all_rows(&mut self) {
        for col in 0..4 {
            let count = self.column_count(col);
            if count == 0 {
                self.selected_row[col] = 0;
            } else if self.selected_row[col] >= count {
                self.selected_row[col] = count - 1;
            }
        }
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{AppConfig, AppState};
    use crate::types::{AgentKind, AgentMode, AgentStatus, PrState, PrStatus};

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "test".into(),
            project_root: PathBuf::from("/tmp/test"),
            agent_kind: AgentKind::OpenCode,
            default_prompt: None,
        }
    }

    fn test_issue(id: &str, worktree: Option<&str>) -> Issue {
        Issue {
            id: id.into(),
            title: format!("Issue {id}"),
            column: Column::InProgress,
            branch: None,
            worktree: worktree.map(|s| s.into()),
            tmux_session: None,
            agent_kind: AgentKind::OpenCode,
            agent_mode: AgentMode::Plan,
            agent_status: AgentStatus::Stopped,
            prompt: None,
        }
    }

    fn test_pr(number: u32, branch: &str) -> PrStatus {
        PrStatus {
            number,
            state: PrState::Open,
            is_draft: false,
            checks: Some(crate::types::ChecksStatus::Success),
            review: Some(crate::types::ReviewDecision::Approved),
            additions: 10,
            deletions: 5,
            head_branch: branch.into(),
        }
    }

    #[test]
    fn test_pr_for_with_matching_branch() {
        let state = AppState {
            issues: vec![test_issue("test-1", Some("bork-1"))],
        };
        let mut app = App::new(test_config(), state);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/my-feature".into());
        app.pr_statuses
            .insert("bork-1/my-feature".into(), test_pr(42, "bork-1/my-feature"));

        let pr = app.pr_for(&app.issues[0].clone()).unwrap();
        assert_eq!(pr.number, 42);
    }

    #[test]
    fn test_pr_for_no_worktree() {
        let state = AppState {
            issues: vec![test_issue("test-1", None)],
        };
        let app = App::new(test_config(), state);
        assert!(app.pr_for(&app.issues[0]).is_none());
    }

    #[test]
    fn test_pr_for_no_branch_in_map() {
        let state = AppState {
            issues: vec![test_issue("test-1", Some("bork-1"))],
        };
        let app = App::new(test_config(), state);
        // worktree_branches is empty, so no branch for "bork-1"
        assert!(app.pr_for(&app.issues[0]).is_none());
    }

    #[test]
    fn test_pr_for_no_matching_pr() {
        let state = AppState {
            issues: vec![test_issue("test-1", Some("bork-1"))],
        };
        let mut app = App::new(test_config(), state);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/my-feature".into());
        // pr_statuses is empty
        assert!(app.pr_for(&app.issues[0].clone()).is_none());
    }

    #[test]
    fn test_pr_for_different_branches_get_correct_prs() {
        let state = AppState {
            issues: vec![
                test_issue("test-1", Some("wt-a")),
                test_issue("test-2", Some("wt-b")),
            ],
        };
        let mut app = App::new(test_config(), state);
        app.worktree_branches
            .insert("wt-a".into(), "branch-a".into());
        app.worktree_branches
            .insert("wt-b".into(), "branch-b".into());
        app.pr_statuses
            .insert("branch-a".into(), test_pr(10, "branch-a"));
        app.pr_statuses
            .insert("branch-b".into(), test_pr(20, "branch-b"));

        let issues = app.issues.clone();
        assert_eq!(app.pr_for(&issues[0]).unwrap().number, 10);
        assert_eq!(app.pr_for(&issues[1]).unwrap().number, 20);
    }

    #[test]
    fn test_branch_for_with_worktree() {
        let state = AppState {
            issues: vec![test_issue("test-1", Some("main"))],
        };
        let mut app = App::new(test_config(), state);
        app.worktree_branches.insert("main".into(), "main".into());
        assert_eq!(app.branch_for(&app.issues[0]), Some("main"));
    }

    #[test]
    fn test_branch_for_no_worktree() {
        let state = AppState {
            issues: vec![test_issue("test-1", None)],
        };
        let app = App::new(test_config(), state);
        assert_eq!(app.branch_for(&app.issues[0]), None);
    }
}
