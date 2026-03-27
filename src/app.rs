use std::collections::{HashMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::config::{AppConfig, AppState};
use crate::types::{
    AgentMode, AgentStatus, AgentStatusInfo, Column, Issue, PrStatus, WorktreeStatus,
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
    pub focused_field: usize, // 0=title, 1=prompt, 2=mode
    pub editing_index: Option<usize>,
}

impl DialogState {
    pub fn new() -> Self {
        DialogState {
            title: String::new(),
            prompt: String::new(),
            agent_mode: AgentMode::Plan,
            focused_field: 0,
            editing_index: None,
        }
    }

    pub fn from_issue(issue: &Issue, index: usize) -> Self {
        DialogState {
            title: issue.title.clone(),
            prompt: issue.prompt.clone().unwrap_or_default(),
            agent_mode: issue.agent_mode,
            focused_field: 0,
            editing_index: Some(index),
        }
    }

    pub fn push_char(&mut self, c: char) {
        match self.focused_field {
            0 => self.title.push(c),
            1 => self.prompt.push(c),
            2 => {
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
    pub config: AppConfig,
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
                let wt = self.issues[idx].worktree.clone();
                self.issues[idx].column = next;
                if next == Column::Done {
                    self.issues[idx].done_at = Some(unix_now());
                    if let Some(w) = wt {
                        self.freeze_worktree_status(&w);
                    }
                }
            }
        }
    }

    pub fn move_issue_left(&mut self) {
        if let Some(idx) = self.selected_issue_index() {
            let was_done = self.issues[idx].column == Column::Done;
            if let Some(prev) = self.issues[idx].column.prev() {
                let wt = self.issues[idx].worktree.clone();
                self.issues[idx].column = prev;
                if was_done {
                    self.issues[idx].done_at = None;
                    if let Some(w) = wt {
                        self.unfreeze_worktree_status(&w);
                    }
                }
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
        let mut changed = false;
        for i in 0..self.issues.len() {
            if self.issues[i].worktree.is_some() {
                continue;
            }
            if let Some(wt) = self.detect_worktree(&self.issues[i].clone()) {
                self.issues[i].worktree = Some(wt.clone());
                if self.issues[i].column == Column::Done {
                    self.freeze_worktree_status(&wt);
                }
                changed = true;
            }
        }
        changed
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
                let session_name = issue.session_name();
                self.is_session_alive(&session_name)
            })
            .map(|(idx, _)| idx)
            .collect()
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
        }
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
}
