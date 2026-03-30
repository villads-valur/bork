use std::collections::{HashMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::config::{AppConfig, AppState};
use crate::external::linear::LinearIssue;
use crate::types::{
    AgentKind, AgentMode, AgentStatus, AgentStatusInfo, Column, Issue, PrStatus, WorktreeStatus,
};

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Confirm,
    Dialog,
    Search,
    LinearPicker,
    Help,
}

#[derive(Debug)]
pub struct LinearPickerState {
    pub search: String,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    KillSession { session_name: String },
    DeleteIssue { issue_index: usize },
}

pub struct DialogState {
    pub title: String,
    pub prompt: String,
    pub agent_mode: AgentMode,
    pub agent_kind: AgentKind,
    pub focused_field: usize, // 0=title, 1=prompt, 2=mode
    pub editing_index: Option<usize>,
    pub target_column: Option<Column>,
}

impl DialogState {
    pub fn new(agent_kind: AgentKind) -> Self {
        DialogState {
            title: String::new(),
            prompt: String::new(),
            agent_mode: AgentMode::Plan,
            agent_kind,
            focused_field: 0,
            editing_index: None,
            target_column: None,
        }
    }

    pub fn from_issue(issue: &Issue, index: usize) -> Self {
        DialogState {
            title: issue.title.clone(),
            prompt: issue.prompt.clone().unwrap_or_default(),
            agent_mode: issue.agent_mode,
            agent_kind: issue.agent_kind,
            focused_field: 0,
            editing_index: Some(index),
            target_column: None,
        }
    }

    pub fn push_char(&mut self, c: char) {
        match self.focused_field {
            0 => self.title.push(c),
            1 => self.prompt.push(c),
            2 => {
                if c == ' ' || c == 'h' || c == 'l' {
                    self.agent_mode = match self.agent_kind {
                        AgentKind::Claude => self.agent_mode.next_for_claude(),
                        AgentKind::OpenCode => self.agent_mode.toggle(),
                    };
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
            _ => {}
        }
    }
}

pub const DIALOG_FIELD_COUNT: usize = 3;

pub struct App {
    pub issues: Vec<Issue>,
    pub selected_column: usize,
    pub selected_row: [usize; 4],
    pub active_sessions: HashSet<String>,
    pub agent_statuses: HashMap<String, AgentStatusInfo>,
    pub worktree_statuses: HashMap<String, WorktreeStatus>,
    pub worktree_branches: HashMap<String, String>,
    pub pr_statuses: HashMap<String, PrStatus>,
    pub frozen_worktree_statuses: HashMap<String, WorktreeStatus>,
    pub frozen_worktree_branches: HashMap<String, String>,
    pub input_mode: InputMode,
    pub confirm_message: Option<String>,
    pub pending_confirm: Option<ConfirmAction>,
    pub dialog: Option<DialogState>,
    pub should_quit: bool,
    pub message: Option<String>,
    pub message_set_at: Option<Instant>,
    pub busy_count: usize,
    pub spinner_tick: usize,
    pub search_query: String,
    pub config: AppConfig,
    pub linear_available: bool,
    pub linear_issues: Vec<LinearIssue>,
    pub linear_picker: Option<LinearPickerState>,
}

impl App {
    pub fn new(config: AppConfig, state: AppState) -> Self {
        let mut issues = state.issues;
        let now = unix_now();
        for issue in &mut issues {
            if issue.column == Column::Done && issue.done_at.is_none() {
                issue.done_at = Some(now);
            }
        }

        App {
            issues,
            selected_column: 0,
            selected_row: [0; 4],
            active_sessions: HashSet::new(),
            agent_statuses: HashMap::new(),
            worktree_statuses: HashMap::new(),
            worktree_branches: HashMap::new(),
            pr_statuses: HashMap::new(),
            frozen_worktree_statuses: HashMap::new(),
            frozen_worktree_branches: HashMap::new(),
            input_mode: InputMode::Normal,
            confirm_message: None,
            pending_confirm: None,
            dialog: None,
            should_quit: false,
            message: None,
            message_set_at: None,
            busy_count: 0,
            spinner_tick: 0,
            search_query: String::new(),
            config,
            linear_available: false,
            linear_issues: Vec::new(),
            linear_picker: None,
        }
    }

    pub fn issues_in_column(&self, column: Column) -> Vec<(usize, &Issue)> {
        let query = self.search_query.to_lowercase();
        self.issues
            .iter()
            .enumerate()
            .filter(|(_, issue)| {
                if issue.column != column {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                issue.title.to_lowercase().contains(&query)
                    || issue.id.to_lowercase().contains(&query)
            })
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
        let Some(idx) = self.selected_issue_index() else {
            return;
        };
        let Some(next) = self.issues[idx].column.next() else {
            return;
        };
        self.move_issue_to_column(idx, next);
    }

    pub fn move_issue_left(&mut self) {
        let Some(idx) = self.selected_issue_index() else {
            return;
        };
        let Some(prev) = self.issues[idx].column.prev() else {
            return;
        };
        self.move_issue_to_column(idx, prev);
    }

    pub fn move_to_done(&mut self) {
        let Some(idx) = self.selected_issue_index() else {
            return;
        };
        self.move_issue_to_column(idx, Column::Done);
    }

    pub fn move_to_todo(&mut self) {
        let Some(idx) = self.selected_issue_index() else {
            return;
        };
        self.move_issue_to_column(idx, Column::Todo);
    }

    fn move_issue_to_column(&mut self, idx: usize, target: Column) {
        let issue = &mut self.issues[idx];
        if issue.column == target {
            return;
        }
        let was_done = issue.column == Column::Done;
        let wt = issue.worktree.clone();
        issue.column = target;

        if target == Column::Done {
            issue.done_at = Some(unix_now());
            if let Some(w) = wt {
                self.freeze_worktree_status(&w);
            }
        } else if was_done {
            issue.done_at = None;
            if let Some(w) = wt {
                self.unfreeze_worktree_status(&w);
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
        let session_name = issue.session_name(&self.config.project_name);

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
        let session_name = issue.session_name(&self.config.project_name);
        self.agent_statuses
            .get(&session_name)
            .and_then(|info| info.activity.as_deref())
    }

    // --- Worktree resolution ---

    /// Return the persisted worktree directory for an issue.
    pub fn worktree_for<'a>(&self, issue: &'a Issue) -> Option<&'a str> {
        issue.worktree.as_deref()
    }

    /// Auto-detect a worktree directory for an issue by matching its ID
    /// against known directory names from both live and frozen worktree maps.
    ///
    /// Matching rules:
    /// 1. Exact match (case-insensitive): dir name == issue ID
    /// 2. Prefix match: dir starts with "{issue_id}-"
    /// 3. Among prefix matches, shortest directory name wins
    fn detect_worktree(&self, issue: &Issue) -> Option<String> {
        let issue_id_lower = issue.id.to_lowercase();

        let mut best: Option<&str> = None;
        let all_keys = self
            .worktree_branches
            .keys()
            .chain(self.frozen_worktree_branches.keys());

        for dir_name in all_keys {
            let dir_lower = dir_name.to_lowercase();
            if dir_lower == issue_id_lower {
                return Some(dir_name.clone());
            }
            if let Some(rest) = dir_lower.strip_prefix(issue_id_lower.as_str()) {
                if rest.starts_with('-') && (best.is_none() || dir_name.len() < best.unwrap().len())
                {
                    best = Some(dir_name.as_str());
                }
            }
        }
        best.map(|s| s.to_string())
    }

    /// Auto-assign worktree directories for issues that don't have one.
    /// Returns true if any assignments were made (signals a state save).
    pub fn auto_assign_worktrees(&mut self) -> bool {
        // Collect assignments first to avoid borrow conflict with detect_worktree
        let assignments: Vec<(usize, String)> = (0..self.issues.len())
            .filter(|&i| self.issues[i].worktree.is_none())
            .filter_map(|i| self.detect_worktree(&self.issues[i]).map(|wt| (i, wt)))
            .collect();

        if assignments.is_empty() {
            return false;
        }

        for (i, wt) in assignments {
            self.issues[i].worktree = Some(wt.clone());
            if self.issues[i].column == Column::Done {
                self.freeze_worktree_status(&wt);
            }
        }
        true
    }

    /// Clear worktree assignments that point to directories that no longer exist.
    /// Returns true if any were cleared (signals a state save).
    pub fn clear_stale_worktrees(&mut self) -> bool {
        let mut changed = false;
        for issue in &mut self.issues {
            let Some(wt) = issue.worktree.as_ref() else {
                continue;
            };
            let exists = self.worktree_branches.contains_key(wt)
                || self.frozen_worktree_branches.contains_key(wt);
            if !exists {
                issue.worktree = None;
                changed = true;
            }
        }
        changed
    }

    pub fn worktree_status_for(&self, issue: &Issue) -> Option<&WorktreeStatus> {
        let wt = self.worktree_for(issue)?;
        if issue.column == Column::Done {
            if let Some(frozen) = self.frozen_worktree_statuses.get(wt) {
                return Some(frozen);
            }
        }
        self.worktree_statuses.get(wt)
    }

    pub fn branch_for(&self, issue: &Issue) -> Option<&str> {
        let wt = self.worktree_for(issue)?;
        if issue.column == Column::Done {
            if let Some(frozen) = self.frozen_worktree_branches.get(wt) {
                return Some(frozen.as_str());
            }
        }
        self.worktree_branches.get(wt).map(|s| s.as_str())
    }

    pub fn pr_for(&self, issue: &Issue) -> Option<&PrStatus> {
        let branch = self.branch_for(issue)?;
        self.pr_statuses.get(branch)
    }

    pub fn done_worktree_names(&self) -> HashSet<String> {
        self.issues
            .iter()
            .filter(|i| i.column == Column::Done)
            .filter_map(|i| i.worktree.clone())
            .collect()
    }

    pub fn freeze_worktree_status(&mut self, worktree: &str) {
        if let Some(status) = self.worktree_statuses.get(worktree) {
            self.frozen_worktree_statuses
                .insert(worktree.to_string(), status.clone());
        }
        if let Some(branch) = self.worktree_branches.get(worktree) {
            self.frozen_worktree_branches
                .insert(worktree.to_string(), branch.clone());
        }
    }

    pub fn unfreeze_worktree_status(&mut self, worktree: &str) {
        self.frozen_worktree_statuses.remove(worktree);
        self.frozen_worktree_branches.remove(worktree);
    }

    pub fn issues_needing_session_cleanup(&self, now: u64) -> Vec<usize> {
        self.issues
            .iter()
            .enumerate()
            .filter(|(_, issue)| {
                if issue.column != Column::Done {
                    return false;
                }
                let Some(done_at) = issue.done_at else {
                    return false;
                };
                if now.saturating_sub(done_at) < self.config.done_session_ttl {
                    return false;
                }
                let session_name = issue.session_name(&self.config.project_name);
                self.is_session_alive(&session_name)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    // --- Dialog ---

    pub fn open_dialog(&mut self) {
        self.open_dialog_in_column(Column::Todo);
    }

    pub fn open_dialog_in_column(&mut self, column: Column) {
        let mut state = DialogState::new(self.config.agent_kind);
        state.target_column = Some(column);
        self.dialog = Some(state);
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

    // --- Linear Picker ---

    pub fn open_linear_picker(&mut self) {
        if self.linear_issues.is_empty() {
            if self.linear_available {
                self.set_message("No Linear issues loaded yet");
            }
            return;
        }
        self.linear_picker = Some(LinearPickerState {
            search: String::new(),
            selected: 0,
        });
        self.input_mode = InputMode::LinearPicker;
    }

    pub fn close_linear_picker(&mut self) {
        self.linear_picker = None;
        self.input_mode = InputMode::Normal;
    }

    pub fn filtered_linear_issues(&self) -> Vec<&LinearIssue> {
        let picker = match &self.linear_picker {
            Some(p) => p,
            None => return Vec::new(),
        };

        let existing_linear_ids: HashSet<&str> = self
            .issues
            .iter()
            .filter_map(|i| i.linear_id.as_deref())
            .collect();

        let query = picker.search.to_lowercase();
        self.linear_issues
            .iter()
            .filter(|i| !existing_linear_ids.contains(i.id.as_str()))
            .filter(|i| {
                if query.is_empty() {
                    return true;
                }
                i.title.to_lowercase().contains(&query)
                    || i.identifier.to_lowercase().contains(&query)
                    || i.team_key.to_lowercase().contains(&query)
            })
            .collect()
    }

    // --- Help ---

    pub fn open_help(&mut self) {
        self.input_mode = InputMode::Help;
    }

    pub fn close_help(&mut self) {
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

    // --- Search ---

    pub fn start_search(&mut self) {
        self.input_mode = InputMode::Search;
    }

    pub fn search_push_char(&mut self, c: char) {
        self.search_query.push(c);
        self.clamp_all_rows();
        self.focus_first_match();
    }

    pub fn search_delete_char(&mut self) {
        if self.search_query.is_empty() {
            self.cancel_search();
            return;
        }
        self.search_query.pop();
        self.clamp_all_rows();
        self.focus_first_match();
    }

    pub fn confirm_search(&mut self) {
        self.input_mode = InputMode::Normal;
    }

    pub fn cancel_search(&mut self) {
        self.search_query.clear();
        self.input_mode = InputMode::Normal;
        self.clamp_all_rows();
    }

    pub fn clear_search(&mut self) {
        if !self.search_query.is_empty() {
            self.search_query.clear();
            self.clamp_all_rows();
        }
    }

    fn focus_first_match(&mut self) {
        for col in 0..4 {
            if self.column_count(col) > 0 {
                self.selected_column = col;
                self.selected_row[col] = 0;
                return;
            }
        }
    }

    pub fn has_active_search(&self) -> bool {
        !self.search_query.is_empty()
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
    use crate::config::DEFAULT_DONE_SESSION_TTL;
    use crate::types::{AgentKind, AgentMode, PrState, PrStatus};

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".into(),
            project_root: PathBuf::from("/tmp/test-bork"),
            agent_kind: AgentKind::OpenCode,
            default_prompt: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
        }
    }

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
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_state: None,
            linear_branch: None,
        }
    }

    fn test_issue_titled(id: &str, title: &str, column: Column) -> Issue {
        let mut issue = test_issue(id, column);
        issue.title = title.to_string();
        issue
    }

    fn test_issue_with_worktree(id: &str, column: Column, worktree: &str) -> Issue {
        let mut issue = test_issue(id, column);
        issue.worktree = Some(worktree.to_string());
        issue
    }

    fn test_app(issues: Vec<Issue>) -> App {
        let state = AppState { issues };
        App::new(test_config(), state)
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

    // ================================================================
    // detect_worktree (auto-detection logic)
    // ================================================================

    #[test]
    fn test_detect_worktree_exact_match() {
        let mut app = test_app(vec![test_issue("bork-8", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert_eq!(
            app.detect_worktree(&app.issues[0].clone()),
            Some("bork-8".into())
        );
    }

    #[test]
    fn test_detect_worktree_prefix_match() {
        let mut app = test_app(vec![test_issue("bork-12", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-12-pr-status".into(), "bork-12/pr-status".into());
        assert_eq!(
            app.detect_worktree(&app.issues[0].clone()),
            Some("bork-12-pr-status".into())
        );
    }

    #[test]
    fn test_detect_worktree_no_false_prefix() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-10".into(), "bork-10/something".into());
        assert_eq!(app.detect_worktree(&app.issues[0].clone()), None);
    }

    #[test]
    fn test_detect_worktree_no_match() {
        let mut app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert_eq!(app.detect_worktree(&app.issues[0].clone()), None);
    }

    #[test]
    fn test_detect_worktree_shortest_prefix_wins() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-1-abc".into(), "bork-1/abc".into());
        app.worktree_branches
            .insert("bork-1-a".into(), "bork-1/a".into());
        app.worktree_branches
            .insert("bork-1-abcdef".into(), "bork-1/abcdef".into());
        assert_eq!(
            app.detect_worktree(&app.issues[0].clone()),
            Some("bork-1-a".into())
        );
    }

    #[test]
    fn test_detect_worktree_exact_preferred_over_prefix() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        app.worktree_branches
            .insert("bork-1-extended".into(), "bork-1/extended".into());
        assert_eq!(
            app.detect_worktree(&app.issues[0].clone()),
            Some("bork-1".into())
        );
    }

    #[test]
    fn test_detect_worktree_case_insensitive() {
        let mut app = test_app(vec![test_issue("BORK-8", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-8".into(), "bork-8/feature".into());
        assert_eq!(
            app.detect_worktree(&app.issues[0].clone()),
            Some("bork-8".into())
        );
    }

    #[test]
    fn test_detect_worktree_searches_frozen_keys() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        // Not in worktree_branches (git worker skipped it)
        // But in frozen_worktree_branches
        app.frozen_worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        assert_eq!(
            app.detect_worktree(&app.issues[0].clone()),
            Some("bork-1".into())
        );
    }

    // ================================================================
    // auto_assign_worktrees / clear_stale_worktrees
    // ================================================================

    #[test]
    fn test_auto_assign_sets_worktree_on_none() {
        let mut app = test_app(vec![test_issue("bork-8", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert!(app.issues[0].worktree.is_none());
        let changed = app.auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.issues[0].worktree, Some("bork-8".into()));
    }

    #[test]
    fn test_auto_assign_skips_already_assigned() {
        let mut issue = test_issue("bork-8", Column::InProgress);
        issue.worktree = Some("bork-8".into());
        let mut app = test_app(vec![issue]);
        app.worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        let changed = app.auto_assign_worktrees();
        assert!(!changed);
    }

    #[test]
    fn test_auto_assign_returns_false_when_no_match() {
        let mut app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        let changed = app.auto_assign_worktrees();
        assert!(!changed);
        assert!(app.issues[0].worktree.is_none());
    }

    #[test]
    fn test_clear_stale_removes_missing_worktree() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1-deleted".into());
        let mut app = test_app(vec![issue]);
        // No entries in worktree_branches or frozen for "bork-1-deleted"
        let changed = app.clear_stale_worktrees();
        assert!(changed);
        assert!(app.issues[0].worktree.is_none());
    }

    #[test]
    fn test_clear_stale_keeps_valid_worktree() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        let changed = app.clear_stale_worktrees();
        assert!(!changed);
        assert_eq!(app.issues[0].worktree, Some("bork-1".into()));
    }

    #[test]
    fn test_clear_stale_keeps_frozen_worktree() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        // Not in worktree_branches, but in frozen
        app.frozen_worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        let changed = app.clear_stale_worktrees();
        assert!(!changed);
        assert_eq!(app.issues[0].worktree, Some("bork-1".into()));
    }

    #[test]
    fn test_auto_assign_freezes_done_issues() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        app.worktree_statuses.insert(
            "bork-1".into(),
            WorktreeStatus {
                staged: 3,
                unstaged: 1,
            },
        );
        let changed = app.auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.issues[0].worktree, Some("bork-1".into()));
        // Should have frozen the worktree data
        assert!(app.frozen_worktree_branches.contains_key("bork-1"));
        assert_eq!(
            app.frozen_worktree_branches.get("bork-1"),
            Some(&"bork-1/feature".into())
        );
        assert!(app.frozen_worktree_statuses.contains_key("bork-1"));
        assert_eq!(app.frozen_worktree_statuses["bork-1"].staged, 3);
    }

    #[test]
    fn test_auto_assign_does_not_freeze_non_done_issues() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        app.auto_assign_worktrees();
        assert!(app.frozen_worktree_branches.is_empty());
        assert!(app.frozen_worktree_statuses.is_empty());
    }

    #[test]
    fn test_auto_assign_uses_frozen_keys_for_done_issues() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        // Not in worktree_branches (git worker skips Done), but in frozen
        app.frozen_worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        let changed = app.auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.issues[0].worktree, Some("bork-1".into()));
    }

    #[test]
    fn test_auto_assign_multiple_issues() {
        let mut app = test_app(vec![
            test_issue("bork-1", Column::InProgress),
            test_issue("bork-2", Column::InProgress),
            test_issue("bork-99", Column::InProgress),
        ]);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/feat".into());
        app.worktree_branches
            .insert("bork-2".into(), "bork-2/feat".into());
        let changed = app.auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.issues[0].worktree, Some("bork-1".into()));
        assert_eq!(app.issues[1].worktree, Some("bork-2".into()));
        assert_eq!(app.issues[2].worktree, None); // no match for bork-99
    }

    #[test]
    fn test_clear_stale_does_not_touch_none() {
        let app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        // worktree is already None, should not count as changed
        assert!(!app.issues[0].worktree.is_some());
    }

    #[test]
    fn test_worktree_for_returns_persisted_value() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1-custom".into());
        let app = test_app(vec![issue]);
        assert_eq!(app.worktree_for(&app.issues[0]), Some("bork-1-custom"));
    }

    #[test]
    fn test_worktree_for_returns_none_when_unset() {
        let app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        assert_eq!(app.worktree_for(&app.issues[0]), None);
    }

    // ================================================================
    // branch_for / pr_for (use persisted worktree field)
    // ================================================================

    #[test]
    fn test_branch_for_with_persisted_worktree() {
        let mut issue = test_issue("bork-8", Column::InProgress);
        issue.worktree = Some("bork-8".into());
        let mut app = test_app(vec![issue]);
        app.worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert_eq!(
            app.branch_for(&app.issues[0].clone()),
            Some("bork-8/init-cli")
        );
    }

    #[test]
    fn test_branch_for_no_worktree_assigned() {
        let app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        assert_eq!(app.branch_for(&app.issues[0]), None);
    }

    #[test]
    fn test_pr_for_with_persisted_worktree() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        app.worktree_branches
            .insert("bork-1".into(), "bork-1/my-feature".into());
        app.pr_statuses
            .insert("bork-1/my-feature".into(), test_pr(42, "bork-1/my-feature"));
        let pr = app.pr_for(&app.issues[0].clone()).unwrap();
        assert_eq!(pr.number, 42);
    }

    #[test]
    fn test_pr_for_no_worktree_returns_none() {
        let app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        assert!(app.pr_for(&app.issues[0]).is_none());
    }

    #[test]
    fn test_pr_for_different_issues_get_correct_prs() {
        let mut issue1 = test_issue("bork-1", Column::InProgress);
        issue1.worktree = Some("bork-1".into());
        let mut issue2 = test_issue("bork-2", Column::InProgress);
        issue2.worktree = Some("bork-2".into());
        let mut app = test_app(vec![issue1, issue2]);
        app.worktree_branches
            .insert("bork-1".into(), "branch-a".into());
        app.worktree_branches
            .insert("bork-2".into(), "branch-b".into());
        app.pr_statuses
            .insert("branch-a".into(), test_pr(10, "branch-a"));
        app.pr_statuses
            .insert("branch-b".into(), test_pr(20, "branch-b"));
        let issues = app.issues.clone();
        assert_eq!(app.pr_for(&issues[0]).unwrap().number, 10);
        assert_eq!(app.pr_for(&issues[1]).unwrap().number, 20);
    }

    // ================================================================
    // DialogState: mode cycling (field 2 = mode in our 3-field dialog)
    // ================================================================

    fn claude_dialog() -> DialogState {
        DialogState::new(crate::types::AgentKind::Claude)
    }

    fn opencode_dialog() -> DialogState {
        DialogState::new(crate::types::AgentKind::OpenCode)
    }

    #[test]
    fn dialog_claude_mode_cycles_plan_build_yolo() {
        let mut d = claude_dialog();
        assert_eq!(d.agent_mode, crate::types::AgentMode::Plan);
        d.focused_field = 2;
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Plan);
    }

    #[test]
    fn dialog_opencode_mode_cycles_plan_build_only() {
        let mut d = opencode_dialog();
        d.focused_field = 2;
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Plan);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
    }

    #[test]
    fn dialog_mode_toggle_with_h_and_l_keys() {
        let mut d = claude_dialog();
        d.focused_field = 2;
        d.push_char('l');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
        d.push_char('h');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
    }

    #[test]
    fn dialog_new_uses_config_agent_kind() {
        let config = test_config();
        let d = DialogState::new(config.agent_kind);
        assert_eq!(d.agent_kind, crate::types::AgentKind::OpenCode);
    }

    #[test]
    fn dialog_from_issue_preserves_agent_kind() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.agent_kind = crate::types::AgentKind::Claude;
        issue.agent_mode = crate::types::AgentMode::Yolo;
        let d = DialogState::from_issue(&issue, 0);
        assert_eq!(d.agent_kind, crate::types::AgentKind::Claude);
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
    }

    // ================================================================
    // Column movement + done_at
    // ================================================================

    #[test]
    fn move_issue_right_from_todo_goes_to_in_progress() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        app.selected_column = 0;
        app.move_issue_right();
        assert_eq!(app.issues[0].column, Column::InProgress);
    }

    #[test]
    fn move_issue_right_from_done_stays_in_done() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        app.selected_column = 3;
        app.move_issue_right();
        assert_eq!(app.issues[0].column, Column::Done);
    }

    #[test]
    fn move_issue_left_from_in_progress_goes_to_todo() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.selected_column = 1;
        app.move_issue_left();
        assert_eq!(app.issues[0].column, Column::Todo);
    }

    #[test]
    fn move_issue_to_done_sets_done_at() {
        let mut app = test_app(vec![test_issue("bork-1", Column::CodeReview)]);
        app.selected_column = 2;
        app.move_issue_right();
        assert_eq!(app.issues[0].column, Column::Done);
        assert!(app.issues[0].done_at.is_some());
    }

    #[test]
    fn move_issue_out_of_done_clears_done_at() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1700000000);
        let mut app = test_app(vec![issue]);
        app.selected_column = 3;
        app.move_issue_left();
        assert_eq!(app.issues[0].column, Column::CodeReview);
        assert_eq!(app.issues[0].done_at, None);
    }

    #[test]
    fn move_issue_within_non_done_columns_keeps_done_at_none() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        app.selected_column = 0;
        app.move_issue_right(); // Todo -> InProgress
        assert_eq!(app.issues[0].done_at, None);
        app.selected_column = 1;
        app.move_issue_right(); // InProgress -> CodeReview
        assert_eq!(app.issues[0].done_at, None);
    }

    #[test]
    fn backfill_done_at_on_startup() {
        // Legacy issues in Done without done_at should get backfilled on App::new
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = None;
        let state = AppState {
            issues: vec![issue],
        };
        let app = App::new(test_config(), state);
        assert!(
            app.issues[0].done_at.is_some(),
            "Done issue with no done_at should be backfilled on startup"
        );
    }

    #[test]
    fn backfill_does_not_overwrite_existing_done_at() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1000);
        let state = AppState {
            issues: vec![issue],
        };
        let app = App::new(test_config(), state);
        assert_eq!(app.issues[0].done_at, Some(1000));
    }

    #[test]
    fn backfill_skips_non_done_issues() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.done_at = None;
        let state = AppState {
            issues: vec![issue],
        };
        let app = App::new(test_config(), state);
        assert_eq!(app.issues[0].done_at, None);
    }

    #[test]
    fn done_at_timestamp_is_recent() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut app = test_app(vec![test_issue("bork-1", Column::CodeReview)]);
        app.selected_column = 2;

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        app.move_issue_right(); // -> Done
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let done_at = app.issues[0].done_at.unwrap();
        assert!(
            done_at >= before && done_at <= after,
            "done_at should be a current timestamp"
        );
    }

    // ================================================================
    // Feature 2: Done session TTL - cleanup logic
    // ================================================================

    #[test]
    fn issues_needing_cleanup_with_expired_ttl() {
        // Issue in Done with done_at 600 seconds ago, TTL is 300s, session alive
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1000);
        issue.tmux_session = Some("bork-bork-1".to_string());

        let mut app = test_app(vec![issue]);
        app.config.done_session_ttl = 300;
        app.active_sessions.insert("bork-bork-1".to_string());

        let now = 1600; // 600 seconds after done_at
        let cleanup = app.issues_needing_session_cleanup(now);
        assert_eq!(
            cleanup,
            vec![0],
            "Issue with expired TTL should be in cleanup list"
        );
    }

    #[test]
    fn issues_needing_cleanup_with_non_expired_ttl() {
        // Issue in Done with done_at 100 seconds ago, TTL is 300s
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1500);
        issue.tmux_session = Some("bork-bork-1".to_string());

        let mut app = test_app(vec![issue]);
        app.config.done_session_ttl = 300;
        app.active_sessions.insert("bork-bork-1".to_string());

        let now = 1600; // 100 seconds after done_at (< 300 TTL)
        let cleanup = app.issues_needing_session_cleanup(now);
        assert!(
            cleanup.is_empty(),
            "Issue within TTL should not be in cleanup list"
        );
    }

    #[test]
    fn issues_needing_cleanup_no_session() {
        // Issue in Done with expired TTL but no active session
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1000);

        let mut app = test_app(vec![issue]);
        app.config.done_session_ttl = 300;
        // No active sessions

        let now = 1600;
        let cleanup = app.issues_needing_session_cleanup(now);
        assert!(
            cleanup.is_empty(),
            "Issue with no active session should not need cleanup"
        );
    }

    #[test]
    fn issues_needing_cleanup_not_in_done() {
        // Issue in InProgress should never be in cleanup list
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.tmux_session = Some("bork-bork-1".to_string());

        let mut app = test_app(vec![issue]);
        app.active_sessions.insert("bork-bork-1".to_string());

        let now = 9999999;
        let cleanup = app.issues_needing_session_cleanup(now);
        assert!(cleanup.is_empty());
    }

    #[test]
    fn issues_needing_cleanup_no_done_at() {
        // Issue in Done but done_at is None (legacy data)
        let mut issue = test_issue("bork-1", Column::Done);
        issue.tmux_session = Some("bork-bork-1".to_string());

        let mut app = test_app(vec![issue]);
        app.active_sessions.insert("bork-bork-1".to_string());

        let now = 9999999;
        let cleanup = app.issues_needing_session_cleanup(now);
        assert!(
            cleanup.is_empty(),
            "Issues without done_at should not be cleaned up"
        );
    }

    #[test]
    fn issues_needing_cleanup_multiple_issues() {
        let mut expired = test_issue("bork-1", Column::Done);
        expired.done_at = Some(1000);
        expired.tmux_session = Some("bork-bork-1".to_string());

        let mut not_expired = test_issue("bork-2", Column::Done);
        not_expired.done_at = Some(1500);
        not_expired.tmux_session = Some("bork-bork-2".to_string());

        let in_progress = test_issue("bork-3", Column::InProgress);

        let mut app = test_app(vec![expired, not_expired, in_progress]);
        app.config.done_session_ttl = 300;
        app.active_sessions.insert("bork-bork-1".to_string());
        app.active_sessions.insert("bork-bork-2".to_string());

        let now = 1600;
        let cleanup = app.issues_needing_session_cleanup(now);
        assert_eq!(
            cleanup,
            vec![0],
            "Only expired issue should be in cleanup list"
        );
    }

    // ================================================================
    // Feature 3: Git polling - done_worktree_names
    // ================================================================

    #[test]
    fn done_worktree_names_returns_done_issue_worktrees() {
        let mut issue1 = test_issue("bork-1", Column::Done);
        issue1.worktree = Some("bork-1".into());
        let mut issue2 = test_issue("bork-2", Column::InProgress);
        issue2.worktree = Some("bork-2".into());
        let mut issue3 = test_issue("bork-3", Column::Done);
        issue3.worktree = Some("bork-3".into());
        let app = test_app(vec![issue1, issue2, issue3]);
        let names = app.done_worktree_names();
        assert!(names.contains("bork-1"));
        assert!(!names.contains("bork-2"));
        assert!(names.contains("bork-3"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn done_worktree_names_empty_when_no_done_issues() {
        let mut issue1 = test_issue("bork-1", Column::Todo);
        issue1.worktree = Some("bork-1".into());
        let app = test_app(vec![issue1]);
        let names = app.done_worktree_names();
        assert!(names.is_empty());
    }

    #[test]
    fn done_worktree_names_skips_issues_without_worktree() {
        let app = test_app(vec![test_issue("bork-99", Column::Done)]);
        let names = app.done_worktree_names();
        assert!(names.is_empty());
    }

    // ================================================================
    // Feature 3: Git polling - freeze/unfreeze worktree status
    // ================================================================

    #[test]
    fn freeze_worktree_copies_current_status() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        app.worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 3,
                unstaged: 5,
            },
        );
        app.worktree_branches
            .insert("bork-1".to_string(), "feature/test".to_string());

        app.freeze_worktree_status("bork-1");

        assert!(app.frozen_worktree_statuses.contains_key("bork-1"));
        let frozen = &app.frozen_worktree_statuses["bork-1"];
        assert_eq!(frozen.staged, 3);
        assert_eq!(frozen.unstaged, 5);
        assert_eq!(
            app.frozen_worktree_branches.get("bork-1"),
            Some(&"feature/test".to_string())
        );
    }

    #[test]
    fn unfreeze_worktree_removes_from_frozen() {
        let mut app = test_app(vec![]);
        app.frozen_worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 1,
                unstaged: 2,
            },
        );
        app.frozen_worktree_branches
            .insert("bork-1".to_string(), "main".to_string());

        app.unfreeze_worktree_status("bork-1");

        assert!(!app.frozen_worktree_statuses.contains_key("bork-1"));
        assert!(!app.frozen_worktree_branches.contains_key("bork-1"));
    }

    #[test]
    fn worktree_status_for_done_issue_uses_frozen() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);

        app.frozen_worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 2,
                unstaged: 4,
            },
        );

        let status = app.worktree_status_for(&app.issues[0].clone());
        assert!(status.is_some(), "Done issue should get frozen status");
        let status = status.unwrap();
        assert_eq!(status.staged, 2);
        assert_eq!(status.unstaged, 4);
    }

    #[test]
    fn worktree_status_for_non_done_issue_uses_live() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);

        app.worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 1,
                unstaged: 0,
            },
        );
        app.frozen_worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 99,
                unstaged: 99,
            },
        );

        let status = app.worktree_status_for(&app.issues[0].clone());
        assert!(status.is_some());
        assert_eq!(
            status.unwrap().staged,
            1,
            "Should use live status, not frozen"
        );
    }

    #[test]
    fn branch_for_done_issue_uses_frozen() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);

        app.frozen_worktree_branches
            .insert("bork-1".to_string(), "feature/done".to_string());

        let branch = app.branch_for(&app.issues[0].clone());
        assert_eq!(
            branch,
            Some("feature/done"),
            "Done issue should get frozen branch"
        );
    }

    // ================================================================
    // Existing logic: resolved_agent_status
    // ================================================================

    #[test]
    fn resolved_status_alive_with_status_file() {
        let issue = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![issue.clone()]);
        app.active_sessions.insert("bork-bork-1".to_string());
        app.agent_statuses.insert(
            "bork-bork-1".to_string(),
            AgentStatusInfo {
                status: AgentStatus::Busy,
                activity: Some("Edit".to_string()),
                updated_at: 0,
            },
        );
        assert_eq!(app.resolved_agent_status(&issue), AgentStatus::Busy);
    }

    #[test]
    fn resolved_status_dead_with_stale_status_file() {
        let issue = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![issue.clone()]);
        // Status file says Busy but session is not alive
        app.agent_statuses.insert(
            "bork-bork-1".to_string(),
            AgentStatusInfo {
                status: AgentStatus::Busy,
                activity: None,
                updated_at: 0,
            },
        );
        assert_eq!(app.resolved_agent_status(&issue), AgentStatus::Stopped);
    }

    #[test]
    fn resolved_status_alive_no_status_file() {
        let issue = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![issue.clone()]);
        app.active_sessions.insert("bork-bork-1".to_string());
        assert_eq!(app.resolved_agent_status(&issue), AgentStatus::Idle);
    }

    #[test]
    fn resolved_status_dead_no_status_file() {
        let issue = test_issue("bork-1", Column::InProgress);
        let app = test_app(vec![issue.clone()]);
        assert_eq!(app.resolved_agent_status(&issue), AgentStatus::Stopped);
    }

    // ================================================================
    // Existing logic: next_issue_id
    // ================================================================

    #[test]
    fn next_issue_id_increments() {
        let app = test_app(vec![
            test_issue("bork-1", Column::Todo),
            test_issue("bork-3", Column::InProgress),
        ]);
        assert_eq!(app.next_issue_id(), "bork-4");
    }

    #[test]
    fn next_issue_id_starts_at_one() {
        let app = test_app(vec![]);
        assert_eq!(app.next_issue_id(), "bork-1");
    }

    // ================================================================
    // Existing logic: issues_in_column
    // ================================================================

    #[test]
    fn issues_in_column_filters_correctly() {
        let app = test_app(vec![
            test_issue("bork-1", Column::Todo),
            test_issue("bork-2", Column::InProgress),
            test_issue("bork-3", Column::Todo),
        ]);
        let todo = app.issues_in_column(Column::Todo);
        assert_eq!(todo.len(), 2);
        assert_eq!(todo[0].1.id, "bork-1");
        assert_eq!(todo[1].1.id, "bork-3");
    }

    // ================================================================
    // Search: issues_in_column filtering
    // ================================================================

    #[test]
    fn search_filters_issues_by_title() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login bug", Column::Todo),
            test_issue_titled("bork-2", "Add dark mode", Column::Todo),
            test_issue_titled("bork-3", "Fix logout crash", Column::Todo),
        ]);
        app.search_query = "fix".to_string();
        let results = app.issues_in_column(Column::Todo);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1.id, "bork-1");
        assert_eq!(results[1].1.id, "bork-3");
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut app = test_app(vec![test_issue_titled(
            "bork-1",
            "Fix Login Bug",
            Column::Todo,
        )]);
        app.search_query = "fix login".to_string();
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);

        app.search_query = "FIX LOGIN".to_string();
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);
    }

    #[test]
    fn search_matches_issue_id() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login bug", Column::Todo),
            test_issue_titled("bork-2", "Add dark mode", Column::Todo),
        ]);
        app.search_query = "bork-2".to_string();
        let results = app.issues_in_column(Column::Todo);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.id, "bork-2");
    }

    #[test]
    fn search_matches_partial_id() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-12", "Add feature", Column::Todo),
        ]);
        app.search_query = "bork-1".to_string();
        let results = app.issues_in_column(Column::Todo);
        assert_eq!(results.len(), 2, "bork-1 and bork-12 both contain 'bork-1'");
    }

    #[test]
    fn search_empty_query_returns_all() {
        let mut app = test_app(vec![
            test_issue("bork-1", Column::Todo),
            test_issue("bork-2", Column::Todo),
        ]);
        app.search_query = String::new();
        assert_eq!(app.issues_in_column(Column::Todo).len(), 2);
    }

    #[test]
    fn search_no_matches_returns_empty() {
        let mut app = test_app(vec![test_issue_titled(
            "bork-1",
            "Fix login bug",
            Column::Todo,
        )]);
        app.search_query = "zzzzz".to_string();
        assert_eq!(app.issues_in_column(Column::Todo).len(), 0);
    }

    #[test]
    fn search_filters_across_columns() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Fix crash", Column::InProgress),
            test_issue_titled("bork-3", "Add feature", Column::Todo),
            test_issue_titled("bork-4", "Fix timeout", Column::Done),
        ]);
        app.search_query = "fix".to_string();
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);
        assert_eq!(app.issues_in_column(Column::InProgress).len(), 1);
        assert_eq!(app.issues_in_column(Column::CodeReview).len(), 0);
        assert_eq!(app.issues_in_column(Column::Done).len(), 1);
    }

    #[test]
    fn search_preserves_global_indices() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::Todo),
            test_issue_titled("bork-3", "Fix crash", Column::Todo),
        ]);
        app.search_query = "fix".to_string();
        let results = app.issues_in_column(Column::Todo);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 1, "global index of bork-2 should be 1");
        assert_eq!(results[1].0, 2, "global index of bork-3 should be 2");
    }

    // ================================================================
    // Search: start_search
    // ================================================================

    #[test]
    fn start_search_enters_search_mode() {
        let mut app = test_app(vec![]);
        assert_eq!(app.input_mode, InputMode::Normal);
        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);
    }

    #[test]
    fn start_search_preserves_existing_query() {
        let mut app = test_app(vec![]);
        app.search_query = "fix".to_string();
        app.confirm_search();
        assert_eq!(app.input_mode, InputMode::Normal);

        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);
        assert_eq!(app.search_query, "fix", "/ should preserve existing query");
    }

    // ================================================================
    // Search: confirm_search
    // ================================================================

    #[test]
    fn confirm_search_returns_to_normal_with_filter_active() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix login", Column::Todo)]);
        app.start_search();
        app.search_push_char('f');
        app.confirm_search();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.search_query, "f", "filter should remain after confirm");
    }

    // ================================================================
    // Search: cancel_search
    // ================================================================

    #[test]
    fn cancel_search_clears_query_and_returns_to_normal() {
        let mut app = test_app(vec![]);
        app.start_search();
        app.search_push_char('f');
        app.search_push_char('i');
        app.cancel_search();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.search_query.is_empty());
    }

    // ================================================================
    // Search: clear_search (Esc in normal mode)
    // ================================================================

    #[test]
    fn clear_search_removes_active_filter() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Add feature", Column::Todo),
        ]);
        app.search_query = "fix".to_string();
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);

        app.clear_search();
        assert!(app.search_query.is_empty());
        assert_eq!(app.issues_in_column(Column::Todo).len(), 2);
    }

    #[test]
    fn clear_search_noop_when_no_filter() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        app.clear_search();
        assert!(app.search_query.is_empty());
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);
    }

    // ================================================================
    // Search: has_active_search
    // ================================================================

    #[test]
    fn has_active_search_false_when_empty() {
        let app = test_app(vec![]);
        assert!(!app.has_active_search());
    }

    #[test]
    fn has_active_search_true_when_query_set() {
        let mut app = test_app(vec![]);
        app.search_query = "test".to_string();
        assert!(app.has_active_search());
    }

    // ================================================================
    // Search: search_push_char + auto-focus first match
    // ================================================================

    #[test]
    fn search_push_char_appends_to_query() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix bug", Column::Todo)]);
        app.start_search();
        app.search_push_char('f');
        assert_eq!(app.search_query, "f");
        app.search_push_char('i');
        assert_eq!(app.search_query, "fi");
        app.search_push_char('x');
        assert_eq!(app.search_query, "fix");
    }

    #[test]
    fn search_auto_focuses_first_match_column() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::InProgress),
        ]);
        app.selected_column = 0;
        app.start_search();
        app.search_push_char('f');
        app.search_push_char('i');
        app.search_push_char('x');

        assert_eq!(
            app.selected_column, 1,
            "should focus InProgress where the match is"
        );
        assert_eq!(app.selected_row[1], 0);
    }

    #[test]
    fn search_auto_focus_skips_empty_columns() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Deploy fix", Column::Done),
        ]);
        app.selected_column = 0;
        app.start_search();
        app.search_push_char('d');
        app.search_push_char('e');

        assert_eq!(
            app.selected_column, 3,
            "should skip empty columns and focus Done"
        );
    }

    #[test]
    fn search_auto_focus_stays_when_current_column_has_matches() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Fix crash", Column::InProgress),
        ]);
        app.selected_column = 0;
        app.start_search();
        app.search_push_char('f');

        assert_eq!(
            app.selected_column, 0,
            "Todo has a match so focus should be on first column with matches"
        );
    }

    // ================================================================
    // Search: search_delete_char
    // ================================================================

    #[test]
    fn search_delete_char_removes_last_char() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix bug", Column::Todo)]);
        app.start_search();
        app.search_push_char('f');
        app.search_push_char('i');
        app.search_push_char('x');
        app.search_delete_char();
        assert_eq!(app.search_query, "fi");
    }

    #[test]
    fn search_backspace_on_empty_cancels_search() {
        let mut app = test_app(vec![]);
        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);

        app.search_delete_char();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.search_query.is_empty());
    }

    #[test]
    fn search_backspace_on_single_char_stays_in_search() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix bug", Column::Todo)]);
        app.start_search();
        app.search_push_char('f');
        app.search_delete_char();

        assert_eq!(app.input_mode, InputMode::Search);
        assert!(app.search_query.is_empty());
    }

    #[test]
    fn search_delete_char_refocuses_first_match() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Add dark mode", Column::InProgress),
        ]);
        app.start_search();
        // Type "add f" — only matches "Add feature" in Todo
        for c in "add f".chars() {
            app.search_push_char(c);
        }
        assert_eq!(app.selected_column, 0);

        // Delete "f" — now "add" matches both columns
        app.search_delete_char();
        assert_eq!(app.selected_column, 0, "first match is still in Todo");
    }

    // ================================================================
    // Search: clamp_all_rows during search
    // ================================================================

    #[test]
    fn search_clamps_row_when_filtered_list_shrinks() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Fix crash", Column::Todo),
            test_issue_titled("bork-3", "Add feature", Column::Todo),
        ]);
        app.selected_column = 0;
        app.selected_row[0] = 2; // selecting "Add feature"

        app.start_search();
        app.search_push_char('f');
        app.search_push_char('i');
        app.search_push_char('x');

        // Only 2 results remain (bork-1 and bork-2), row 2 is out of bounds
        let count = app.issues_in_column(Column::Todo).len();
        assert_eq!(count, 2);
        assert!(
            app.selected_row[0] < count,
            "row should be clamped to valid range"
        );
    }

    #[test]
    fn search_clamps_row_to_zero_when_column_empty() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix login", Column::Todo)]);
        app.selected_column = 0;
        app.selected_row[0] = 0;

        app.start_search();
        app.search_push_char('z');
        app.search_push_char('z');

        assert_eq!(app.issues_in_column(Column::Todo).len(), 0);
        assert_eq!(app.selected_row[0], 0);
    }

    // ================================================================
    // Search: full interaction flow
    // ================================================================

    #[test]
    fn search_full_flow_type_confirm_clear() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Add feature", Column::InProgress),
        ]);

        // Start search
        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);

        // Type query
        app.search_push_char('f');
        app.search_push_char('i');
        app.search_push_char('x');
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);
        assert_eq!(app.issues_in_column(Column::InProgress).len(), 0);

        // Confirm — filter stays, back to normal
        app.confirm_search();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.search_query, "fix");
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);

        // Clear — all issues visible again
        app.clear_search();
        assert!(app.search_query.is_empty());
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);
        assert_eq!(app.issues_in_column(Column::InProgress).len(), 1);
    }

    #[test]
    fn search_full_flow_type_cancel() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Add feature", Column::Todo),
        ]);

        app.start_search();
        app.search_push_char('f');
        app.search_push_char('i');
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);

        // Cancel — clears query, all issues back
        app.cancel_search();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.search_query.is_empty());
        assert_eq!(app.issues_in_column(Column::Todo).len(), 2);
    }

    #[test]
    fn search_reenter_preserves_and_refines_query() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login bug", Column::Todo),
            test_issue_titled("bork-2", "Fix logout crash", Column::Todo),
        ]);

        // First search: "fix"
        app.start_search();
        app.search_push_char('f');
        app.search_push_char('i');
        app.search_push_char('x');
        app.confirm_search();
        assert_eq!(app.issues_in_column(Column::Todo).len(), 2);

        // Re-enter: query still "fix", refine to "fix log"
        app.start_search();
        assert_eq!(app.search_query, "fix");
        app.search_push_char(' ');
        app.search_push_char('l');
        app.search_push_char('o');
        app.search_push_char('g');
        assert_eq!(app.issues_in_column(Column::Todo).len(), 2);

        // Refine further to "fix login"
        app.search_push_char('i');
        app.search_push_char('n');
        assert_eq!(app.issues_in_column(Column::Todo).len(), 1);
        assert_eq!(app.issues_in_column(Column::Todo)[0].1.id, "bork-1");
    }

    #[test]
    fn search_selected_issue_works_with_filter() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::Todo),
            test_issue_titled("bork-3", "Fix crash", Column::Todo),
        ]);
        app.selected_column = 0;

        app.search_query = "fix".to_string();
        app.clamp_all_rows();
        app.selected_row[0] = 0;

        let issue = app.selected_issue().expect("should have selected issue");
        assert_eq!(issue.id, "bork-2", "first filtered result should be bork-2");

        app.selected_row[0] = 1;
        let issue = app.selected_issue().expect("should have selected issue");
        assert_eq!(
            issue.id, "bork-3",
            "second filtered result should be bork-3"
        );
    }

    #[test]
    fn search_selected_issue_index_returns_global_index() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::Todo),
        ]);
        app.selected_column = 0;

        app.search_query = "fix".to_string();
        app.clamp_all_rows();
        app.selected_row[0] = 0;

        let idx = app.selected_issue_index().expect("should have index");
        assert_eq!(idx, 1, "global index of 'Fix bug' is 1, not 0");
    }

    // ================================================================
    // Linear picker
    // ================================================================

    fn test_linear_issue(id: &str, identifier: &str, title: &str) -> LinearIssue {
        LinearIssue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            title: title.to_string(),
            url: format!("https://linear.app/test/issue/{}", identifier),
            branch_name: format!("{}-slug", identifier.to_lowercase()),
            priority: 2,
            state_name: "In Progress".to_string(),
            team_key: "TEST".to_string(),
        }
    }

    #[test]
    fn open_linear_picker_requires_issues() {
        let mut app = test_app(vec![]);
        app.linear_available = true;
        app.linear_issues = vec![];

        app.open_linear_picker();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.linear_picker.is_none());
    }

    #[test]
    fn open_linear_picker_with_issues() {
        let mut app = test_app(vec![]);
        app.linear_available = true;
        app.linear_issues = vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        app.open_linear_picker();
        assert_eq!(app.input_mode, InputMode::LinearPicker);
        assert!(app.linear_picker.is_some());
    }

    #[test]
    fn close_linear_picker_restores_normal_mode() {
        let mut app = test_app(vec![]);
        app.linear_available = true;
        app.linear_issues = vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        app.open_linear_picker();
        app.close_linear_picker();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.linear_picker.is_none());
    }

    #[test]
    fn filtered_linear_issues_excludes_already_imported() {
        let mut issue = test_issue("test-1", Column::Todo);
        issue.linear_id = Some("uuid-1".to_string());
        let mut app = test_app(vec![issue]);

        app.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "Already imported"),
            test_linear_issue("uuid-2", "TEST-2", "Not imported"),
        ];
        app.open_linear_picker();

        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].identifier, "TEST-2");
    }

    #[test]
    fn filtered_linear_issues_filters_by_search() {
        let mut app = test_app(vec![]);
        app.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "Add login page"),
            test_linear_issue("uuid-2", "TEST-2", "Fix dashboard bug"),
            test_linear_issue("uuid-3", "TEST-3", "Add logout button"),
        ];
        app.open_linear_picker();

        if let Some(ref mut picker) = app.linear_picker {
            picker.search = "add".to_string();
        }

        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].identifier, "TEST-1");
        assert_eq!(filtered[1].identifier, "TEST-3");
    }

    #[test]
    fn filtered_linear_issues_matches_identifier() {
        let mut app = test_app(vec![]);
        app.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "First"),
            test_linear_issue("uuid-2", "DOC-99", "Second"),
        ];
        app.open_linear_picker();

        if let Some(ref mut picker) = app.linear_picker {
            picker.search = "doc".to_string();
        }

        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].identifier, "DOC-99");
    }
}
