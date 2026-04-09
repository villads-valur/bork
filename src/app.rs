use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ratatui::style::{Modifier, Style};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::config::{AppConfig, AppState};
use crate::external::linear::LinearIssue;
use crate::types::{
    AgentKind, AgentMode, AgentStatus, AgentStatusInfo, Column, Issue, IssueKind, PrState,
    PrStatus, WorktreeStatus,
};
use crate::ui::styles;

pub type ProjectId = PathBuf;

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
    DebugInspector,
    Sidebar,
}

#[derive(Default)]
pub struct LiveState {
    pub active_sessions: HashSet<String>,
    pub agent_statuses: HashMap<String, AgentStatusInfo>,
    pub listening_ports: HashMap<String, Vec<u16>>,
    pub worktree_statuses: HashMap<String, WorktreeStatus>,
    pub worktree_branches: HashMap<String, String>,
    pub pr_statuses: HashMap<String, PrStatus>,
    pub frozen_worktree_statuses: HashMap<String, WorktreeStatus>,
    pub frozen_worktree_branches: HashMap<String, String>,
    pub linear_issues: Vec<LinearIssue>,
    pub user_prs: Vec<PrStatus>,
    pub github_user: Option<String>,
    pub git_poll_done: bool,
}

impl LiveState {
    pub fn has_github_prs(&self) -> bool {
        !self.pr_statuses.is_empty() || !self.user_prs.is_empty()
    }
}

pub struct Project {
    pub issues: Vec<Issue>,
    pub config: AppConfig,
    pub selected_column: usize,
    pub selected_row: [usize; 4],
    pub search_query: String,
    pub linear_available: bool,
    pub tuicr_available: bool,
    pub live: LiveState,
    pub state_dirty: bool,
    pub base_issues: Vec<Issue>,
    pub last_state_mtime: Option<SystemTime>,
}

impl Project {
    pub fn new(config: AppConfig, state: AppState) -> Self {
        let mut issues = state.issues;
        let now = unix_now();
        for issue in &mut issues {
            if issue.column == Column::Done && issue.done_at.is_none() {
                issue.done_at = Some(now);
            }
        }
        let base_issues = issues.clone();
        let last_state_mtime = crate::config::state_mtime(&config.project_root);
        Project {
            issues,
            config,
            selected_column: 0,
            selected_row: [0; 4],
            search_query: String::new(),
            linear_available: false,
            tuicr_available: false,
            live: LiveState::default(),
            state_dirty: false,
            base_issues,
            last_state_mtime,
        }
    }

    pub fn id(&self) -> ProjectId {
        std::fs::canonicalize(&self.config.project_root)
            .unwrap_or_else(|_| self.config.project_root.clone())
    }

    pub fn mark_dirty(&mut self) {
        self.state_dirty = true;
    }

    pub fn to_state(&self) -> AppState {
        AppState {
            issues: self.issues.clone(),
        }
    }

    pub fn update_base_snapshot(&mut self) {
        self.base_issues = self.issues.clone();
        self.last_state_mtime = crate::config::state_mtime(&self.config.project_root);
    }

    pub fn merge_external_state(&mut self, file_state: AppState) {
        let file_issues = file_state.issues;

        if !self.state_dirty {
            // No local changes pending, safe to fully replace
            self.issues = file_issues.clone();
            self.base_issues = file_issues;
            self.clamp_all_rows();
            return;
        }

        // 3-way merge: base (last known disk state) vs memory vs file
        let file_ids: HashSet<String> = file_issues.iter().map(|i| i.id.clone()).collect();
        let memory_ids: HashSet<String> = self.issues.iter().map(|i| i.id.clone()).collect();

        // Remove issues that were deleted externally
        self.issues.retain(|i| file_ids.contains(&i.id));

        // Add issues that were created externally
        for file_issue in &file_issues {
            if !memory_ids.contains(&file_issue.id) {
                self.issues.push(file_issue.clone());
            }
        }

        // Field-level merge for issues present in both memory and file
        for issue in &mut self.issues {
            let Some(file_issue) = file_issues.iter().find(|f| f.id == issue.id) else {
                continue;
            };
            let Some(base_issue) = self.base_issues.iter().find(|b| b.id == issue.id) else {
                // No base means this issue was added after last snapshot; keep memory version
                continue;
            };
            merge_issue_fields(issue, base_issue, file_issue);
        }

        self.base_issues = file_issues;
        self.clamp_all_rows();
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

    pub fn is_session_alive(&self, session_name: &str) -> bool {
        self.live.active_sessions.contains(session_name)
    }

    pub fn resolved_agent_status(&self, issue: &Issue) -> AgentStatus {
        let session_name = issue.session_name(&self.config.project_name);
        let live = &self.live;

        if let Some(info) = live.agent_statuses.get(&session_name) {
            if !live.active_sessions.contains(&session_name) {
                return AgentStatus::Stopped;
            }
            return info.status;
        }

        if live.active_sessions.contains(&session_name) {
            return AgentStatus::Idle;
        }

        AgentStatus::Stopped
    }

    pub fn resolved_activity(&self, issue: &Issue) -> Option<&str> {
        let session_name = issue.session_name(&self.config.project_name);
        self.live
            .agent_statuses
            .get(&session_name)
            .and_then(|info| info.activity.as_deref())
    }

    pub fn listening_ports_for(&self, issue: &Issue) -> Option<&Vec<u16>> {
        let session_name = issue.session_name(&self.config.project_name);
        self.live.listening_ports.get(&session_name)
    }

    pub fn worktree_for<'a>(&self, issue: &'a Issue) -> Option<&'a str> {
        issue.worktree.as_deref()
    }

    /// Finds a worktree directory by dash-bounded substring match of the issue ID.
    /// Shortest match wins (e.g. `bork-1` preferred over `bork-1-extended`).
    pub(crate) fn detect_worktree(&self, issue: &Issue) -> Option<String> {
        let issue_id_lower = issue.id.to_lowercase();
        let live = &self.live;

        let mut best: Option<&str> = None;
        let all_keys = live
            .worktree_branches
            .keys()
            .chain(live.frozen_worktree_branches.keys());

        for dir_name in all_keys {
            let dir_lower = dir_name.to_lowercase();
            let Some(pos) = dir_lower.find(&issue_id_lower) else {
                continue;
            };
            let before_ok = pos == 0 || dir_lower.as_bytes()[pos - 1] == b'-';
            let end = pos + issue_id_lower.len();
            let after_ok = end == dir_lower.len() || dir_lower.as_bytes()[end] == b'-';
            if before_ok && after_ok && (best.is_none() || dir_name.len() < best.unwrap().len()) {
                best = Some(dir_name.as_str());
            }
        }
        best.map(|s| s.to_string())
    }

    pub fn auto_assign_worktrees(&mut self) -> bool {
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

    pub fn clear_stale_worktrees(&mut self) -> bool {
        let live = &self.live;
        let live_branches = &live.worktree_branches;
        let frozen_branches = &live.frozen_worktree_branches;
        let stale: Vec<usize> = self
            .issues
            .iter()
            .enumerate()
            .filter_map(|(i, issue)| {
                let wt = issue.worktree.as_ref()?;
                let exists = live_branches.contains_key(wt) || frozen_branches.contains_key(wt);
                if !exists {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();

        if stale.is_empty() {
            return false;
        }

        for i in stale {
            self.issues[i].worktree = None;
        }
        true
    }

    pub fn worktree_status_for(&self, issue: &Issue) -> Option<&WorktreeStatus> {
        let wt = self.worktree_for(issue)?;
        let live = &self.live;
        if issue.column == Column::Done {
            if let Some(frozen) = live.frozen_worktree_statuses.get(wt) {
                return Some(frozen);
            }
        }
        live.worktree_statuses.get(wt)
    }

    pub fn branch_for(&self, issue: &Issue) -> Option<&str> {
        let live = &self.live;
        if let Some(wt) = self.worktree_for(issue) {
            if issue.column == Column::Done {
                if let Some(frozen) = live.frozen_worktree_branches.get(wt) {
                    return Some(frozen.as_str());
                }
            }
            if let Some(branch) = live.worktree_branches.get(wt) {
                return Some(branch.as_str());
            }
        }

        if let Some(pr_num) = issue.pr_number {
            if let Some(pr) = live.user_prs.iter().find(|p| p.number == pr_num) {
                return Some(pr.head_branch.as_str());
            }
        }

        None
    }

    pub fn pr_for(&self, issue: &Issue) -> Option<&PrStatus> {
        let live = &self.live;
        if let Some(branch) = self.branch_for(issue) {
            if let Some(pr) = live.pr_statuses.get(branch) {
                return Some(pr);
            }
        }
        let pr_num = issue.pr_number?;
        live.pr_statuses
            .values()
            .chain(live.user_prs.iter())
            .find(|p| p.number == pr_num)
    }

    pub fn sync_prs_as_issues(&mut self) -> (bool, Option<String>) {
        if self.live.user_prs.is_empty() {
            return (false, None);
        }

        let claimed_branches: HashSet<String> = self
            .issues
            .iter()
            .filter_map(|issue| self.branch_for(issue).map(|b| b.to_string()))
            .collect();

        let claimed_pr_numbers: HashSet<u32> = self
            .issues
            .iter()
            .filter_map(|issue| issue.pr_number)
            .collect();

        let issue_ids: Vec<String> = self.issues.iter().map(|i| i.id.to_lowercase()).collect();

        let mut new_issues: Vec<Issue> = Vec::new();

        for pr in &self.live.user_prs {
            if pr.state != PrState::Open {
                continue;
            }
            if pr.is_draft {
                continue;
            }

            let branch = &pr.head_branch;
            if branch == "main" || branch == "master" {
                continue;
            }

            if claimed_branches.contains(branch) {
                continue;
            }

            let branch_lower = branch.to_lowercase();
            let has_prefix_match = issue_ids.iter().any(|id| {
                branch_lower.starts_with(&format!("{}/", id))
                    || branch_lower.starts_with(&format!("{}-", id))
            });
            if has_prefix_match {
                continue;
            }

            if claimed_pr_numbers.contains(&pr.number) {
                continue;
            }

            let id = self.next_issue_id_after(new_issues.len() as u32);
            new_issues.push(Issue {
                id,
                title: pr.title.clone(),
                kind: IssueKind::Agentic,
                column: Column::CodeReview,
                agent_kind: self.config.agent_kind,
                agent_mode: AgentMode::Plan,
                prompt: None,
                worktree: None,
                done_at: None,
                session_id: None,
                linear_id: None,
                linear_identifier: None,
                linear_url: None,
                linear_imported: false,
                pr_number: Some(pr.number),
                pr_imported: true,
            });
        }

        if new_issues.is_empty() {
            return (false, None);
        }

        let count = new_issues.len();
        self.issues.append(&mut new_issues);
        let msg = format!(
            "Imported {} PR{} from GitHub",
            count,
            if count == 1 { "" } else { "s" }
        );
        (true, Some(msg))
    }

    pub fn done_worktree_names(&self) -> HashSet<String> {
        self.issues
            .iter()
            .filter(|i| i.column == Column::Done)
            .filter_map(|i| i.worktree.clone())
            .collect()
    }

    pub fn freeze_worktree_status(&mut self, worktree: &str) {
        if let Some(status) = self.live.worktree_statuses.get(worktree) {
            self.live
                .frozen_worktree_statuses
                .insert(worktree.to_string(), status.clone());
        }
        if let Some(branch) = self.live.worktree_branches.get(worktree).cloned() {
            self.live
                .frozen_worktree_branches
                .insert(worktree.to_string(), branch);
        }
    }

    pub fn unfreeze_worktree_status(&mut self, worktree: &str) {
        self.live.frozen_worktree_statuses.remove(worktree);
        self.live.frozen_worktree_branches.remove(worktree);
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

    pub fn has_github_prs(&self) -> bool {
        self.live.has_github_prs()
    }

    pub fn filtered_linear_issues<'a>(
        &'a self,
        picker: &LinearPickerState,
    ) -> Vec<&'a LinearIssue> {
        let query = picker.search.to_lowercase();
        self.live
            .linear_issues
            .iter()
            .filter(|i| {
                query.is_empty()
                    || i.title.to_lowercase().contains(&query)
                    || i.identifier.to_lowercase().contains(&query)
                    || i.team_key.to_lowercase().contains(&query)
            })
            .collect()
    }

    pub fn filtered_github_prs<'a>(&'a self, picker: &LinearPickerState) -> Vec<&'a PrStatus> {
        let query = picker.search.to_lowercase();
        let live = &self.live;

        let mut seen: HashSet<u32> = HashSet::new();
        let mut prs: Vec<&PrStatus> = Vec::new();

        for pr in live.pr_statuses.values() {
            if seen.insert(pr.number) {
                prs.push(pr);
            }
        }
        for pr in &live.user_prs {
            if seen.insert(pr.number) {
                prs.push(pr);
            }
        }

        prs.retain(|pr| {
            query.is_empty()
                || pr.title.to_lowercase().contains(&query)
                || pr.number.to_string().contains(&query)
                || pr.author.to_lowercase().contains(&query)
                || pr.head_branch.to_lowercase().contains(&query)
        });

        prs.sort_by(|a, b| {
            let a_open = a.state == PrState::Open;
            let b_open = b.state == PrState::Open;
            b_open.cmp(&a_open).then(b.number.cmp(&a.number))
        });
        prs
    }

    pub fn next_issue_id(&self) -> String {
        self.next_issue_id_after(0)
    }

    fn next_issue_id_after(&self, offset: u32) -> String {
        let prefix = &self.config.project_name;
        let max_num = self
            .issues
            .iter()
            .filter_map(|issue| {
                issue
                    .id
                    .strip_prefix(&format!("{}-", prefix))
                    .and_then(|s| s.parse::<u32>().ok())
            })
            .max()
            .unwrap_or(0);

        format!("{}-{}", prefix, max_num + 1 + offset)
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

    fn focus_first_match(&mut self) {
        for col in 0..4 {
            if self.column_count(col) > 0 {
                self.selected_column = col;
                self.selected_row[col] = 0;
                return;
            }
        }
    }
}

pub struct SidebarState {
    pub visible: bool,
    pub focused: bool,
    pub selected: usize,
    pub activity: HashMap<ProjectId, bool>,
    pub swimlanes: Vec<ProjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSize {
    Full,
    Medium,
    Compact,
}

#[derive(Debug, Clone)]
pub struct ActionContext {
    pub project_id: ProjectId,
}

#[derive(Debug)]
pub struct LinearPickerState {
    pub search: String,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    KillSession {
        session_name: String,
        project_id: ProjectId,
    },
    DeleteIssue {
        issue_index: usize,
        project_id: ProjectId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearPickerContext {
    Import,
    Attach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    Linear,
    GitHub,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogField {
    Kind,
    Mode,
    Linear,
    GithubPr,
    Title,
    Prompt,
}

pub struct DialogState {
    pub kind: IssueKind,
    pub title: String,
    pub title_cursor: usize,
    pub prompt: TextArea<'static>,
    pub agent_mode: AgentMode,
    pub agent_kind: AgentKind,
    pub focused_field: usize,
    pub editing_index: Option<usize>,
    pub target_column: Option<Column>,
    pub linear_issue: Option<LinearIssue>,
    pub linear_detached: bool,
    pub linear_available: bool,
    pub github_pr: Option<PrStatus>,
    pub github_pr_cleared: bool,
    pub github_available: bool,
}

fn make_prompt_textarea(text: &str) -> TextArea<'static> {
    let mut ta = TextArea::from(text.lines());
    ta.set_cursor_line_style(Style::default());
    ta.set_cursor_style(
        Style::default()
            .fg(styles::ACCENT)
            .add_modifier(Modifier::REVERSED),
    );
    ta.set_block(ratatui::widgets::Block::default());
    ta.set_wrap_mode(WrapMode::Word);
    ta
}

impl DialogState {
    pub fn new(agent_kind: AgentKind, linear_available: bool, github_available: bool) -> Self {
        let kind = IssueKind::Agentic;
        let title_idx = Self::compute_title_index(kind, linear_available, github_available);
        DialogState {
            kind,
            title: String::new(),
            title_cursor: 0,
            prompt: make_prompt_textarea(""),
            agent_mode: AgentMode::Plan,
            agent_kind,
            focused_field: title_idx,
            editing_index: None,
            target_column: None,
            linear_issue: None,
            linear_detached: false,
            linear_available,
            github_pr: None,
            github_pr_cleared: false,
            github_available,
        }
    }

    pub fn from_issue(
        issue: &Issue,
        index: usize,
        linear_available: bool,
        github_available: bool,
        all_prs: &HashMap<String, PrStatus>,
        user_prs: &[PrStatus],
    ) -> Self {
        let prompt_text = issue.prompt.as_deref().unwrap_or("");
        let linear_issue = issue.linear_id.as_ref().map(|lid| LinearIssue {
            id: lid.clone(),
            identifier: issue.linear_identifier.clone().unwrap_or_default(),
            title: issue.title.clone(),
            url: issue.linear_url.clone().unwrap_or_default(),
            branch_name: String::new(),
            priority: 0,
            state_name: String::new(),
            team_key: String::new(),
        });

        let github_pr = issue.pr_number.and_then(|num| {
            all_prs
                .values()
                .chain(user_prs.iter())
                .find(|pr| pr.number == num)
                .cloned()
        });

        let mut prompt = make_prompt_textarea(prompt_text);
        prompt.move_cursor(CursorMove::Bottom);
        prompt.move_cursor(CursorMove::End);

        let title_idx = Self::compute_title_index(issue.kind, linear_available, github_available);
        DialogState {
            kind: issue.kind,
            title: issue.title.clone(),
            title_cursor: issue.title.chars().count(),
            prompt,
            agent_mode: issue.agent_mode,
            agent_kind: issue.agent_kind,
            focused_field: title_idx,
            editing_index: Some(index),
            target_column: None,
            linear_issue,
            linear_detached: false,
            linear_available,
            github_pr,
            github_pr_cleared: false,
            github_available,
        }
    }

    pub fn prompt_text(&self) -> String {
        self.prompt.lines().join("\n")
    }

    pub fn set_prompt_text(&mut self, text: &str) {
        self.prompt = make_prompt_textarea(text);
        self.prompt.move_cursor(CursorMove::Bottom);
        self.prompt.move_cursor(CursorMove::End);
    }

    pub fn current_field(&self) -> DialogField {
        let fields = self.ordered_fields();
        fields[self.focused_field.min(fields.len() - 1)]
    }

    fn ordered_fields(&self) -> Vec<DialogField> {
        let mut fields = vec![DialogField::Kind];
        if self.kind == IssueKind::Agentic {
            fields.push(DialogField::Mode);
        }
        if self.linear_available {
            fields.push(DialogField::Linear);
        }
        if self.github_available {
            fields.push(DialogField::GithubPr);
        }
        fields.push(DialogField::Title);
        fields.push(DialogField::Prompt);
        fields
    }

    pub fn active_field_count(&self) -> usize {
        self.ordered_fields().len()
    }

    fn compute_title_index(
        kind: IssueKind,
        linear_available: bool,
        github_available: bool,
    ) -> usize {
        let mut idx = 1; // after kind
        if kind == IssueKind::Agentic {
            idx += 1;
        }
        if linear_available {
            idx += 1;
        }
        if github_available {
            idx += 1;
        }
        idx
    }

    pub fn is_on_linear_field(&self) -> bool {
        self.current_field() == DialogField::Linear
    }

    pub fn is_on_github_field(&self) -> bool {
        self.current_field() == DialogField::GithubPr
    }

    pub fn next_field(&mut self) {
        self.focused_field = (self.focused_field + 1) % self.active_field_count();
    }

    pub fn prev_field(&mut self) {
        if self.focused_field > 0 {
            self.focused_field -= 1;
        } else {
            self.focused_field = self.active_field_count() - 1;
        }
    }

    fn clamp_focused_field(&mut self) {
        let max = self.active_field_count() - 1;
        if self.focused_field > max {
            self.focused_field = max;
        }
    }

    pub fn push_char(&mut self, c: char) {
        match self.current_field() {
            DialogField::Kind => {
                match c {
                    ' ' => {
                        self.kind = match self.kind {
                            IssueKind::Agentic => IssueKind::NonAgentic,
                            IssueKind::NonAgentic => IssueKind::Agentic,
                        };
                    }
                    'h' => self.kind = IssueKind::Agentic,
                    'l' => self.kind = IssueKind::NonAgentic,
                    _ => {}
                }
                self.clamp_focused_field();
            }
            DialogField::Mode => {
                if c == ' ' || c == 'h' || c == 'l' {
                    self.agent_mode = match self.agent_kind {
                        AgentKind::Claude => self.agent_mode.next_for_claude(),
                        AgentKind::OpenCode => self.agent_mode.toggle(),
                    };
                }
            }
            DialogField::Linear | DialogField::GithubPr => {}
            DialogField::Title => insert_char(&mut self.title, &mut self.title_cursor, c),
            DialogField::Prompt => self.prompt.insert_char(c),
        }
    }

    pub fn delete_char(&mut self) {
        match self.current_field() {
            DialogField::Title => {
                delete_char_before_cursor(&mut self.title, &mut self.title_cursor)
            }
            DialogField::Prompt => {
                self.prompt.delete_char();
            }
            _ => {}
        }
    }

    pub fn delete_char_forward(&mut self) {
        match self.current_field() {
            DialogField::Title => delete_char_at_cursor(&mut self.title, self.title_cursor),
            DialogField::Prompt => {
                self.prompt.delete_next_char();
            }
            _ => {}
        }
    }

    pub fn move_cursor_left(&mut self) {
        match self.current_field() {
            DialogField::Title => self.title_cursor = self.title_cursor.saturating_sub(1),
            DialogField::Prompt => self.prompt.move_cursor(CursorMove::Back),
            _ => {}
        }
    }

    pub fn move_cursor_right(&mut self) {
        match self.current_field() {
            DialogField::Title => {
                self.title_cursor = (self.title_cursor + 1).min(self.title.chars().count())
            }
            DialogField::Prompt => self.prompt.move_cursor(CursorMove::Forward),
            _ => {}
        }
    }

    pub fn move_cursor_start(&mut self) {
        match self.current_field() {
            DialogField::Title => self.title_cursor = 0,
            DialogField::Prompt => self.prompt.move_cursor(CursorMove::Head),
            _ => {}
        }
    }

    pub fn move_cursor_end(&mut self) {
        match self.current_field() {
            DialogField::Title => self.title_cursor = self.title.chars().count(),
            DialogField::Prompt => self.prompt.move_cursor(CursorMove::End),
            _ => {}
        }
    }

    pub fn delete_word_backward(&mut self) {
        match self.current_field() {
            DialogField::Title => delete_word_backward(&mut self.title, &mut self.title_cursor),
            DialogField::Prompt => {
                self.prompt.delete_word();
            }
            _ => {}
        }
    }

    pub fn clear_to_start(&mut self) {
        match self.current_field() {
            DialogField::Title => clear_to_start(&mut self.title, &mut self.title_cursor),
            DialogField::Prompt => {
                self.prompt.delete_line_by_head();
            }
            _ => {}
        }
    }
}

fn char_to_byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| s.len())
}

fn insert_char(text: &mut String, cursor: &mut usize, c: char) {
    let byte_index = char_to_byte_index(text, *cursor);
    text.insert(byte_index, c);
    *cursor += 1;
}

fn delete_char_before_cursor(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let end = char_to_byte_index(text, *cursor);
    let start = char_to_byte_index(text, *cursor - 1);
    text.drain(start..end);
    *cursor -= 1;
}

fn delete_char_at_cursor(text: &mut String, cursor: usize) {
    if cursor >= text.chars().count() {
        return;
    }
    let start = char_to_byte_index(text, cursor);
    let end = char_to_byte_index(text, cursor + 1);
    text.drain(start..end);
}

fn delete_word_backward(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut start = *cursor;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let start_byte = char_to_byte_index(text, start);
    let end_byte = char_to_byte_index(text, *cursor);
    text.drain(start_byte..end_byte);
    *cursor = start;
}

fn clear_to_start(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let end_byte = char_to_byte_index(text, *cursor);
    text.drain(0..end_byte);
    *cursor = 0;
}

/// 3-way field merge: for each field, if file diverged from base but memory didn't,
/// take the file value. If both diverged, memory wins.
fn merge_issue_fields(memory: &mut Issue, base: &Issue, file: &Issue) {
    macro_rules! merge_field {
        ($field:ident) => {
            if memory.$field == base.$field && file.$field != base.$field {
                memory.$field = file.$field.clone();
            }
        };
    }
    merge_field!(title);
    merge_field!(kind);
    merge_field!(column);
    merge_field!(agent_kind);
    merge_field!(agent_mode);
    merge_field!(prompt);
    merge_field!(worktree);
    merge_field!(done_at);
    merge_field!(session_id);
    merge_field!(linear_id);
    merge_field!(linear_identifier);
    merge_field!(linear_url);
    merge_field!(linear_imported);
    merge_field!(pr_number);
    merge_field!(pr_imported);
}

pub struct App {
    pub projects: Vec<Project>,
    pub focused_project: ProjectId,
    pub focused_swimlane: usize,
    pub sidebar: Option<SidebarState>,
    pub input_mode: InputMode,
    pub confirm_message: Option<String>,
    pub pending_confirm: Option<ConfirmAction>,
    pub dialog: Option<DialogState>,
    pub should_quit: bool,
    pub message: Option<String>,
    pub message_set_at: Option<Instant>,
    pub busy_count: usize,
    pub spinner_tick: usize,
    pub linear_picker: Option<LinearPickerState>,
    pub linear_picker_context: LinearPickerContext,
    pub picker_tab: ImportSource,
    pub debug_inspector_json: Option<String>,
    pub debug_inspector_scroll: usize,
}

impl App {
    pub fn new(config: AppConfig, state: AppState) -> Self {
        let project = Project::new(config, state);
        let focused_id = project.id();
        App {
            projects: vec![project],
            focused_project: focused_id,
            focused_swimlane: 0,
            sidebar: None,
            input_mode: InputMode::Normal,
            confirm_message: None,
            pending_confirm: None,
            dialog: None,
            should_quit: false,
            message: None,
            message_set_at: None,
            busy_count: 0,
            spinner_tick: 0,
            linear_picker: None,
            linear_picker_context: LinearPickerContext::Import,
            picker_tab: ImportSource::Linear,
            debug_inspector_json: None,
            debug_inspector_scroll: 0,
        }
    }

    pub fn add_background_project(&mut self, config: AppConfig, state: AppState) {
        self.projects.push(Project::new(config, state));
    }

    pub fn enable_sidebar(&mut self) {
        if self.projects.len() > 1 {
            self.sidebar = Some(SidebarState {
                visible: false,
                focused: false,
                selected: 0,
                activity: HashMap::new(),
                swimlanes: vec![self.focused_project.clone()],
            });
        }
    }

    pub fn reload_projects(&mut self) {
        crate::global_config::prune_stale_projects();

        let known_roots: HashSet<ProjectId> = self.projects.iter().map(|p| p.id()).collect();

        for entry in &crate::global_config::load_global_config().projects {
            if !entry.path.join(".bork").join("config.toml").exists() {
                continue;
            }
            let canonical =
                std::fs::canonicalize(&entry.path).unwrap_or_else(|_| entry.path.clone());
            if known_roots.contains(&canonical) {
                continue;
            }
            let proj_config = crate::config::load_config_from(&entry.path);
            let proj_state = crate::config::load_state(&entry.path);
            self.add_background_project(proj_config, proj_state);
        }

        if self.projects.len() > 1 && self.sidebar.is_none() {
            self.enable_sidebar();
        }
    }

    pub fn find_project(&self, id: &ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id() == *id)
    }

    pub fn find_project_mut(&mut self, id: &ProjectId) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id() == *id)
    }

    pub(crate) fn project_index(&self, id: &ProjectId) -> Option<usize> {
        self.projects.iter().position(|p| p.id() == *id)
    }

    pub fn project(&self) -> &Project {
        self.find_project(&self.focused_project)
            .expect("focused project not found")
    }

    pub fn project_mut(&mut self) -> &mut Project {
        let id = self.focused_project.clone();
        self.find_project_mut(&id)
            .expect("focused project not found")
    }

    pub fn active_project_id(&self) -> ProjectId {
        let lanes = self.visible_swimlanes();
        let id = lanes
            .get(self.focused_swimlane)
            .cloned()
            .unwrap_or_else(|| self.focused_project.clone());
        debug_assert!(
            self.find_project(&id).is_some(),
            "active project {:?} not found",
            id
        );
        id
    }

    pub fn active_project(&self) -> &Project {
        let id = self.active_project_id();
        self.find_project(&id).expect("active project not found")
    }

    pub fn active_project_mut(&mut self) -> &mut Project {
        let id = self.active_project_id();
        self.find_project_mut(&id)
            .expect("active project not found")
    }

    pub fn action_context(&self) -> ActionContext {
        ActionContext {
            project_id: self.active_project_id(),
        }
    }

    pub fn context_project(&self, ctx: &ActionContext) -> &Project {
        self.find_project(&ctx.project_id)
            .unwrap_or_else(|| self.project())
    }

    pub fn context_project_mut(&mut self, ctx: &ActionContext) -> &mut Project {
        let id = ctx.project_id.clone();
        let has_project = self.find_project(&id).is_some();
        if has_project {
            self.find_project_mut(&id).unwrap()
        } else {
            self.project_mut()
        }
    }

    pub fn visible_swimlanes(&self) -> Vec<ProjectId> {
        if let Some(ref sidebar) = self.sidebar {
            if !sidebar.swimlanes.is_empty() {
                return sidebar
                    .swimlanes
                    .iter()
                    .filter(|id| self.find_project(id).is_some())
                    .cloned()
                    .collect();
            }
        }
        vec![self.focused_project.clone()]
    }

    pub fn visible_swimlane_count(&self) -> usize {
        if let Some(ref sidebar) = self.sidebar {
            if !sidebar.swimlanes.is_empty() {
                return sidebar
                    .swimlanes
                    .iter()
                    .filter(|id| self.find_project(id).is_some())
                    .count();
            }
        }
        1
    }

    pub fn card_size(&self) -> CardSize {
        match self.visible_swimlane_count() {
            2 => CardSize::Medium,
            n if n >= 3 => CardSize::Compact,
            _ => CardSize::Full,
        }
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
        self.message_set_at = Some(Instant::now());
    }

    pub fn clear_expired_message(&mut self) -> bool {
        if let Some(set_at) = self.message_set_at {
            if set_at.elapsed().as_secs() >= 3 {
                self.message = None;
                self.message_set_at = None;
                return true;
            }
        }
        false
    }

    pub fn spinner_frame(&self) -> &'static str {
        const FRAMES: &[&str] = &[
            "\u{28cb}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
            "\u{2827}", "\u{2807}", "\u{280f}",
        ];
        FRAMES[self.spinner_tick % FRAMES.len()]
    }

    pub fn open_dialog(&mut self, ctx: &ActionContext) {
        self.open_dialog_in_column(Column::Todo, ctx);
    }

    pub fn open_dialog_in_column(&mut self, column: Column, ctx: &ActionContext) {
        let p = self.context_project(ctx);
        let github_available = p.has_github_prs();
        let mut state = DialogState::new(p.config.agent_kind, p.linear_available, github_available);
        state.target_column = Some(column);
        self.dialog = Some(state);
        self.input_mode = InputMode::Dialog;
    }

    pub fn open_edit_dialog(&mut self, issue: &Issue, index: usize, ctx: &ActionContext) {
        let p = self.context_project(ctx);
        let github_available = p.has_github_prs();
        let live = &p.live;
        self.dialog = Some(DialogState::from_issue(
            issue,
            index,
            p.linear_available,
            github_available,
            &live.pr_statuses,
            &live.user_prs,
        ));
        self.input_mode = InputMode::Dialog;
    }

    pub fn close_dialog(&mut self) {
        self.dialog = None;
        self.input_mode = InputMode::Normal;
    }

    pub fn open_import_picker(&mut self, ctx: &ActionContext) {
        self.open_import_picker_with_context(LinearPickerContext::Import, ctx);
    }

    pub fn open_import_picker_with_context(
        &mut self,
        context: LinearPickerContext,
        ctx: &ActionContext,
    ) {
        let p = self.context_project(ctx);
        let has_linear = !p.live.linear_issues.is_empty();
        let has_github = p.has_github_prs();

        if !has_linear && !has_github {
            if p.linear_available {
                self.set_message("No issues loaded yet");
            } else {
                self.set_message("No import sources available");
            }
            return;
        }

        if self.picker_tab == ImportSource::Linear && !has_linear {
            self.picker_tab = ImportSource::GitHub;
        } else if self.picker_tab == ImportSource::GitHub && !has_github {
            self.picker_tab = ImportSource::Linear;
        }

        self.linear_picker_context = context;
        self.linear_picker = Some(LinearPickerState {
            search: String::new(),
            selected: 0,
        });
        self.input_mode = InputMode::LinearPicker;
    }

    #[cfg(test)]
    pub fn open_linear_picker(&mut self, ctx: &ActionContext) {
        self.open_import_picker_with_context(LinearPickerContext::Import, ctx);
    }

    pub fn open_linear_picker_with_context(
        &mut self,
        context: LinearPickerContext,
        ctx: &ActionContext,
    ) {
        self.open_import_picker_with_context(context, ctx);
    }

    pub fn close_linear_picker(&mut self) {
        self.linear_picker = None;
        if self.linear_picker_context == LinearPickerContext::Attach && self.dialog.is_some() {
            self.input_mode = InputMode::Dialog;
        } else {
            self.input_mode = InputMode::Normal;
        }
        self.linear_picker_context = LinearPickerContext::Import;
    }

    pub fn filtered_linear_issues(&self) -> Vec<&LinearIssue> {
        let picker = match &self.linear_picker {
            Some(p) => p,
            None => return Vec::new(),
        };
        self.active_project().filtered_linear_issues(picker)
    }

    pub fn filtered_github_prs(&self) -> Vec<&PrStatus> {
        let picker = match &self.linear_picker {
            Some(p) => p,
            None => return Vec::new(),
        };
        self.active_project().filtered_github_prs(picker)
    }

    pub fn open_help(&mut self) {
        self.input_mode = InputMode::Help;
    }

    pub fn close_help(&mut self) {
        self.input_mode = InputMode::Normal;
    }

    pub fn open_debug_inspector(&mut self, json: String) {
        self.debug_inspector_json = Some(json);
        self.debug_inspector_scroll = 0;
        self.input_mode = InputMode::DebugInspector;
    }

    pub fn close_debug_inspector(&mut self) {
        self.debug_inspector_json = None;
        self.debug_inspector_scroll = 0;
        self.input_mode = InputMode::Normal;
    }

    pub fn debug_inspector_line_count(&self) -> usize {
        self.debug_inspector_json
            .as_ref()
            .map(|j| j.lines().count())
            .unwrap_or(0)
    }

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

    pub fn start_search(&mut self) {
        self.input_mode = InputMode::Search;
    }

    pub fn search_push_char(&mut self, c: char, ctx: &ActionContext) {
        let p = self.context_project_mut(ctx);
        p.search_query.push(c);
        p.clamp_all_rows();
        p.focus_first_match();
    }

    pub fn search_delete_char(&mut self, ctx: &ActionContext) {
        if self.context_project(ctx).search_query.is_empty() {
            self.cancel_search(ctx);
            return;
        }
        let p = self.context_project_mut(ctx);
        p.search_query.pop();
        p.clamp_all_rows();
        p.focus_first_match();
    }

    pub fn confirm_search(&mut self) {
        self.input_mode = InputMode::Normal;
    }

    pub fn cancel_search(&mut self, ctx: &ActionContext) {
        let p = self.context_project_mut(ctx);
        p.search_query.clear();
        p.clamp_all_rows();
        self.input_mode = InputMode::Normal;
    }

    pub fn clear_search(&mut self, ctx: &ActionContext) {
        let p = self.context_project_mut(ctx);
        if !p.search_query.is_empty() {
            p.search_query.clear();
            p.clamp_all_rows();
        }
    }

    pub fn has_active_search(&self) -> bool {
        self.active_project().has_active_search()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::DEFAULT_DONE_SESSION_TTL;
    use crate::types::{AgentKind, AgentMode, IssueKind, PrState, PrStatus};

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".into(),
            project_root: PathBuf::from("/tmp/test-bork"),
            agent_kind: AgentKind::OpenCode,
            default_prompt: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
        }
    }

    fn test_issue(id: &str, column: Column) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Test issue {}", id),
            kind: IssueKind::Agentic,
            column,
            agent_kind: AgentKind::OpenCode,
            agent_mode: AgentMode::Plan,
            prompt: None,
            worktree: None,
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
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
            title: format!("PR #{}", number),
            url: format!("https://github.com/test/repo/pull/{}", number),
            author: "testuser".into(),
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
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-8".into())
        );
    }

    #[test]
    fn test_detect_worktree_prefix_match() {
        let mut app = test_app(vec![test_issue("bork-12", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-12-pr-status".into(), "bork-12/pr-status".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-12-pr-status".into())
        );
    }

    #[test]
    fn test_detect_worktree_no_false_prefix() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-10".into(), "bork-10/something".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            None
        );
    }

    #[test]
    fn test_detect_worktree_no_match() {
        let mut app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            None
        );
    }

    #[test]
    fn test_detect_worktree_shortest_prefix_wins() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1-abc".into(), "bork-1/abc".into());
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1-a".into(), "bork-1/a".into());
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1-abcdef".into(), "bork-1/abcdef".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-1-a".into())
        );
    }

    #[test]
    fn test_detect_worktree_exact_preferred_over_prefix() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1-extended".into(), "bork-1/extended".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-1".into())
        );
    }

    #[test]
    fn test_detect_worktree_case_insensitive() {
        let mut app = test_app(vec![test_issue("BORK-8", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/feature".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-8".into())
        );
    }

    #[test]
    fn test_detect_worktree_searches_frozen_keys() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        app.project_mut()
            .live
            .frozen_worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-1".into())
        );
    }

    #[test]
    fn test_detect_worktree_project_prefixed_dir() {
        let mut app = test_app(vec![test_issue("doc-1929", Column::InProgress)]);
        app.project_mut().live.worktree_branches.insert(
            "legora-doc-1929-show-hidden-data".into(),
            "doc-1929/show-hidden-data".into(),
        );
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("legora-doc-1929-show-hidden-data".into())
        );
    }

    #[test]
    fn test_detect_worktree_project_prefixed_no_slug() {
        let mut app = test_app(vec![test_issue("doc-1929", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("legora-doc-1929".into(), "doc-1929/feature".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("legora-doc-1929".into())
        );
    }

    #[test]
    fn test_detect_worktree_no_false_substring_match() {
        let mut app = test_app(vec![test_issue("doc-1", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("legora-doc-12-something".into(), "doc-12/something".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            None
        );
    }

    #[test]
    fn test_detect_worktree_exact_preferred_over_substring() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        app.project_mut()
            .live
            .worktree_branches
            .insert("legora-bork-1-extended".into(), "bork-1/extended".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-1".into())
        );
    }

    #[test]
    fn test_detect_worktree_id_with_slug_suffix() {
        let mut app = test_app(vec![test_issue("bork-14", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-14-fix-auth".into(), "bork-14/fix-auth".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("bork-14-fix-auth".into())
        );
    }

    #[test]
    fn test_detect_worktree_linear_id_with_slug_suffix() {
        let mut app = test_app(vec![test_issue("vil-123", Column::InProgress)]);
        app.project_mut().live.worktree_branches.insert(
            "vil-123-fix-auth-flow".into(),
            "vil-123/fix-auth-flow".into(),
        );
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            Some("vil-123-fix-auth-flow".into())
        );
    }

    #[test]
    fn test_detect_worktree_slug_suffix_no_false_positive() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-14-fix-auth".into(), "bork-14/fix-auth".into());
        assert_eq!(
            app.project()
                .detect_worktree(&app.project().issues[0].clone()),
            None
        );
    }

    // ================================================================
    // auto_assign_worktrees / clear_stale_worktrees
    // ================================================================

    #[test]
    fn test_auto_assign_sets_worktree_on_none() {
        let mut app = test_app(vec![test_issue("bork-8", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert!(app.project().issues[0].worktree.is_none());
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.project().issues[0].worktree, Some("bork-8".into()));
    }

    #[test]
    fn test_auto_assign_skips_already_assigned() {
        let mut issue = test_issue("bork-8", Column::InProgress);
        issue.worktree = Some("bork-8".into());
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(!changed);
    }

    #[test]
    fn test_auto_assign_returns_false_when_no_match() {
        let mut app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(!changed);
        assert!(app.project().issues[0].worktree.is_none());
    }

    #[test]
    fn test_clear_stale_removes_missing_worktree() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1-deleted".into());
        let mut app = test_app(vec![issue]);
        // No entries in worktree_branches or frozen for "bork-1-deleted"
        let changed = app.project_mut().clear_stale_worktrees();
        assert!(changed);
        assert!(app.project().issues[0].worktree.is_none());
    }

    #[test]
    fn test_clear_stale_keeps_valid_worktree() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        let changed = app.project_mut().clear_stale_worktrees();
        assert!(!changed);
        assert_eq!(app.project().issues[0].worktree, Some("bork-1".into()));
    }

    #[test]
    fn test_clear_stale_keeps_frozen_worktree() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        // Not in worktree_branches, but in frozen
        app.project_mut()
            .live
            .frozen_worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        let changed = app.project_mut().clear_stale_worktrees();
        assert!(!changed);
        assert_eq!(app.project().issues[0].worktree, Some("bork-1".into()));
    }

    #[test]
    fn test_auto_assign_freezes_done_issues() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        app.project_mut().live.worktree_statuses.insert(
            "bork-1".into(),
            WorktreeStatus {
                staged: 3,
                unstaged: 1,
            },
        );
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.project().issues[0].worktree, Some("bork-1".into()));
        // Should have frozen the worktree data
        assert!(app
            .project()
            .live
            .frozen_worktree_branches
            .contains_key("bork-1"));
        assert_eq!(
            app.project_mut()
                .live
                .frozen_worktree_branches
                .get("bork-1"),
            Some(&"bork-1/feature".into())
        );
        assert!(app
            .project()
            .live
            .frozen_worktree_statuses
            .contains_key("bork-1"));
        assert_eq!(
            app.project().live.frozen_worktree_statuses["bork-1"].staged,
            3
        );
    }

    #[test]
    fn test_auto_assign_does_not_freeze_non_done_issues() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        app.project_mut().auto_assign_worktrees();
        assert!(app.project().live.frozen_worktree_branches.is_empty());
        assert!(app.project().live.frozen_worktree_statuses.is_empty());
    }

    #[test]
    fn test_auto_assign_uses_frozen_keys_for_done_issues() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        // Not in worktree_branches (git worker skips Done), but in frozen
        app.project_mut()
            .live
            .frozen_worktree_branches
            .insert("bork-1".into(), "bork-1/feature".into());
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.project().issues[0].worktree, Some("bork-1".into()));
    }

    #[test]
    fn test_auto_assign_multiple_issues() {
        let mut app = test_app(vec![
            test_issue("bork-1", Column::InProgress),
            test_issue("bork-2", Column::InProgress),
            test_issue("bork-99", Column::InProgress),
        ]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "bork-1/feat".into());
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-2".into(), "bork-2/feat".into());
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.project().issues[0].worktree, Some("bork-1".into()));
        assert_eq!(app.project().issues[1].worktree, Some("bork-2".into()));
        assert_eq!(app.project().issues[2].worktree, None); // no match for bork-99
    }

    #[test]
    fn test_clear_stale_does_not_touch_none() {
        let app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        // worktree is already None, should not count as changed
        assert!(!app.project().issues[0].worktree.is_some());
    }

    #[test]
    fn test_worktree_for_returns_persisted_value() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1-custom".into());
        let app = test_app(vec![issue]);
        assert_eq!(
            app.project().worktree_for(&app.project().issues[0]),
            Some("bork-1-custom")
        );
    }

    #[test]
    fn test_worktree_for_returns_none_when_unset() {
        let app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        assert_eq!(app.project().worktree_for(&app.project().issues[0]), None);
    }

    // ================================================================
    // branch_for / pr_for (use persisted worktree field)
    // ================================================================

    #[test]
    fn test_branch_for_with_persisted_worktree() {
        let mut issue = test_issue("bork-8", Column::InProgress);
        issue.worktree = Some("bork-8".into());
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        assert_eq!(
            app.project().branch_for(&app.project().issues[0].clone()),
            Some("bork-8/init-cli")
        );
    }

    #[test]
    fn test_branch_for_no_worktree_assigned() {
        let app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        assert_eq!(app.project().branch_for(&app.project().issues[0]), None);
    }

    #[test]
    fn test_pr_for_with_persisted_worktree() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "bork-1/my-feature".into());
        app.project_mut()
            .live
            .pr_statuses
            .insert("bork-1/my-feature".into(), test_pr(42, "bork-1/my-feature"));
        let pr = app
            .project()
            .pr_for(&app.project().issues[0].clone())
            .unwrap();
        assert_eq!(pr.number, 42);
    }

    #[test]
    fn test_pr_for_no_worktree_returns_none() {
        let app = test_app(vec![test_issue("bork-99", Column::InProgress)]);
        assert!(app.project().pr_for(&app.project().issues[0]).is_none());
    }

    #[test]
    fn test_pr_for_different_issues_get_correct_prs() {
        let mut issue1 = test_issue("bork-1", Column::InProgress);
        issue1.worktree = Some("bork-1".into());
        let mut issue2 = test_issue("bork-2", Column::InProgress);
        issue2.worktree = Some("bork-2".into());
        let mut app = test_app(vec![issue1, issue2]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "branch-a".into());
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-2".into(), "branch-b".into());
        app.project_mut()
            .live
            .pr_statuses
            .insert("branch-a".into(), test_pr(10, "branch-a"));
        app.project_mut()
            .live
            .pr_statuses
            .insert("branch-b".into(), test_pr(20, "branch-b"));
        let issues = app.project_mut().issues.clone();
        assert_eq!(app.project().pr_for(&issues[0]).unwrap().number, 10);
        assert_eq!(app.project().pr_for(&issues[1]).unwrap().number, 20);
    }

    // ================================================================
    // sync_prs_as_issues (auto-import PRs)
    // ================================================================

    #[test]
    fn sync_prs_imports_open_pr_as_issue() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![test_pr(1, "feature/new")];

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert_eq!(app.project().issues[0].title, "PR #1");
        assert_eq!(app.project().issues[0].column, Column::CodeReview);
        assert_eq!(app.project().issues[0].pr_number, Some(1));
    }

    #[test]
    fn sync_prs_skips_when_no_user_prs() {
        let mut app = test_app(vec![]);
        // user_prs is empty by default
        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_draft_prs() {
        let mut app = test_app(vec![]);
        let mut pr = test_pr(1, "feature/new");
        pr.is_draft = true;
        app.project_mut().live.user_prs = vec![pr];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_closed_prs() {
        let mut app = test_app(vec![]);
        let mut pr = test_pr(1, "feature/new");
        pr.state = PrState::Closed;
        app.project_mut().live.user_prs = vec![pr];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_merged_prs() {
        let mut app = test_app(vec![]);
        let mut pr = test_pr(1, "feature/new");
        pr.state = PrState::Merged;
        app.project_mut().live.user_prs = vec![pr];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_main_branch() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![test_pr(1, "main")];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_dedup_by_branch_match() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".into(), "feature/thing".into());
        app.project_mut().live.user_prs = vec![test_pr(1, "feature/thing")];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_dedup_by_issue_id_prefix() {
        let issue = test_issue("bork-5", Column::InProgress);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(1, "bork-5/follow-up")];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_dedup_by_pr_number() {
        let mut issue = test_issue("bork-1", Column::CodeReview);
        issue.pr_number = Some(42);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(42, "some/branch")];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_reimports_after_delete() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![test_pr(42, "feature/new")];

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert_eq!(app.project().issues[0].pr_number, Some(42));

        app.project_mut().issues.clear();

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert_eq!(app.project().issues[0].pr_number, Some(42));
    }

    #[test]
    fn sync_prs_multiple_prs_get_unique_ids() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![
            test_pr(1, "feature/a"),
            test_pr(2, "feature/b"),
            test_pr(3, "feature/c"),
        ];

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 3);

        let ids: HashSet<&str> = app
            .project_mut()
            .issues
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn sync_prs_no_duplicate_on_second_call() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![test_pr(1, "feature/new")];

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_dedup_by_issue_id_prefix_with_dash() {
        let issue = test_issue("doc-1917", Column::InProgress);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(1, "DOC-1917-attachment-selection-search")];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_prefix_no_false_match_on_similar_ids() {
        // Issue "bork-1" should NOT match branch "bork-10/something"
        let issue = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(1, "bork-10/something")];

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 2); // new issue created
    }

    #[test]
    fn sync_prs_prefix_match_is_case_insensitive() {
        let issue = test_issue("BORK-5", Column::InProgress);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(1, "bork-5/fix")];

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    // ================================================================
    // DialogState: mode cycling (field 3 = mode)
    // ================================================================

    fn claude_dialog() -> DialogState {
        DialogState::new(crate::types::AgentKind::Claude, false, false)
    }

    fn opencode_dialog() -> DialogState {
        DialogState::new(crate::types::AgentKind::OpenCode, false, false)
    }

    #[test]
    fn dialog_claude_mode_cycles_plan_build_yolo() {
        let mut d = claude_dialog();
        assert_eq!(d.agent_mode, crate::types::AgentMode::Plan);
        d.focused_field = 1; // Mode field (Agentic, no linear)
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
        d.focused_field = 1; // Mode field (Agentic, no linear)
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
        d.focused_field = 1; // Mode field (Agentic, no linear)
        d.push_char('l');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
        d.push_char('h');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
    }

    #[test]
    fn dialog_new_uses_config_agent_kind() {
        let config = test_config();
        let d = DialogState::new(config.agent_kind, false, false);
        assert_eq!(d.agent_kind, crate::types::AgentKind::OpenCode);
    }

    #[test]
    fn dialog_from_issue_preserves_agent_kind() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.agent_kind = crate::types::AgentKind::Claude;
        issue.agent_mode = crate::types::AgentMode::Yolo;
        let d = DialogState::from_issue(
            &issue,
            0,
            false,
            false,
            &std::collections::HashMap::new(),
            &[],
        );
        assert_eq!(d.agent_kind, crate::types::AgentKind::Claude);
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
    }

    #[test]
    fn dialog_new_defaults_to_agentic_with_title_focused() {
        let d = DialogState::new(crate::types::AgentKind::OpenCode, false, false);
        assert_eq!(d.kind, IssueKind::Agentic);
        // Agentic, no linear: Kind(0), Mode(1), Title(2)
        assert_eq!(d.focused_field, 2);
    }

    #[test]
    fn dialog_prompt_supports_normal_edit_commands() {
        let mut d = DialogState::new(crate::types::AgentKind::OpenCode, false, false);
        // Agentic, no linear: Kind(0), Mode(1), Title(2), Prompt(3)
        d.focused_field = 3;

        for c in "todo note".chars() {
            d.push_char(c);
        }
        assert_eq!(d.prompt_text(), "todo note");

        d.move_cursor_left();
        d.move_cursor_left();
        d.delete_char();
        assert_eq!(d.prompt_text(), "todo nte");

        d.move_cursor_start();
        d.delete_char_forward();
        assert_eq!(d.prompt_text(), "odo nte");

        d.move_cursor_end();
        d.delete_word_backward();
        assert_eq!(d.prompt_text(), "odo ");

        d.clear_to_start();
        assert_eq!(d.prompt_text(), "");
    }

    // ================================================================
    // Column movement + done_at
    // ================================================================

    #[test]
    fn move_issue_right_from_todo_goes_to_in_progress() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        app.project_mut().selected_column = 0;
        app.project_mut().move_issue_right();
        assert_eq!(app.project().issues[0].column, Column::InProgress);
    }

    #[test]
    fn move_issue_right_from_done_stays_in_done() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        app.project_mut().selected_column = 3;
        app.project_mut().move_issue_right();
        assert_eq!(app.project().issues[0].column, Column::Done);
    }

    #[test]
    fn move_issue_left_from_in_progress_goes_to_todo() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut().selected_column = 1;
        app.project_mut().move_issue_left();
        assert_eq!(app.project().issues[0].column, Column::Todo);
    }

    #[test]
    fn move_issue_to_done_sets_done_at() {
        let mut app = test_app(vec![test_issue("bork-1", Column::CodeReview)]);
        app.project_mut().selected_column = 2;
        app.project_mut().move_issue_right();
        assert_eq!(app.project().issues[0].column, Column::Done);
        assert!(app.project().issues[0].done_at.is_some());
    }

    #[test]
    fn move_issue_out_of_done_clears_done_at() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1700000000);
        let mut app = test_app(vec![issue]);
        app.project_mut().selected_column = 3;
        app.project_mut().move_issue_left();
        assert_eq!(app.project().issues[0].column, Column::CodeReview);
        assert_eq!(app.project().issues[0].done_at, None);
    }

    #[test]
    fn move_issue_within_non_done_columns_keeps_done_at_none() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        app.project_mut().selected_column = 0;
        app.project_mut().move_issue_right(); // Todo -> InProgress
        assert_eq!(app.project().issues[0].done_at, None);
        app.project_mut().selected_column = 1;
        app.project_mut().move_issue_right(); // InProgress -> CodeReview
        assert_eq!(app.project().issues[0].done_at, None);
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
            app.project().issues[0].done_at.is_some(),
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
        assert_eq!(app.project().issues[0].done_at, Some(1000));
    }

    #[test]
    fn backfill_skips_non_done_issues() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.done_at = None;
        let state = AppState {
            issues: vec![issue],
        };
        let app = App::new(test_config(), state);
        assert_eq!(app.project().issues[0].done_at, None);
    }

    #[test]
    fn done_at_timestamp_is_recent() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut app = test_app(vec![test_issue("bork-1", Column::CodeReview)]);
        app.project_mut().selected_column = 2;

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        app.project_mut().move_issue_right(); // -> Done
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let done_at = app.project().issues[0].done_at.unwrap();
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

        let mut app = test_app(vec![issue]);
        app.project_mut().config.done_session_ttl = 300;
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());

        let now = 1600; // 600 seconds after done_at
        let cleanup = app.project().issues_needing_session_cleanup(now);
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

        let mut app = test_app(vec![issue]);
        app.project_mut().config.done_session_ttl = 300;
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());

        let now = 1600; // 100 seconds after done_at (< 300 TTL)
        let cleanup = app.project().issues_needing_session_cleanup(now);
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
        app.project_mut().config.done_session_ttl = 300;
        // No active sessions

        let now = 1600;
        let cleanup = app.project().issues_needing_session_cleanup(now);
        assert!(
            cleanup.is_empty(),
            "Issue with no active session should not need cleanup"
        );
    }

    #[test]
    fn issues_needing_cleanup_not_in_done() {
        // Issue in InProgress should never be in cleanup list
        let issue = test_issue("bork-1", Column::InProgress);

        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());

        let now = 9999999;
        let cleanup = app.project().issues_needing_session_cleanup(now);
        assert!(cleanup.is_empty());
    }

    #[test]
    fn issues_needing_cleanup_no_done_at() {
        // Issue in Done but done_at is None (legacy data)
        let issue = test_issue("bork-1", Column::Done);

        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());

        let now = 9999999;
        let cleanup = app.project().issues_needing_session_cleanup(now);
        assert!(
            cleanup.is_empty(),
            "Issues without done_at should not be cleaned up"
        );
    }

    #[test]
    fn issues_needing_cleanup_multiple_issues() {
        let mut expired = test_issue("bork-1", Column::Done);
        expired.done_at = Some(1000);

        let mut not_expired = test_issue("bork-2", Column::Done);
        not_expired.done_at = Some(1500);

        let in_progress = test_issue("bork-3", Column::InProgress);

        let mut app = test_app(vec![expired, not_expired, in_progress]);
        app.project_mut().config.done_session_ttl = 300;
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-2".to_string());

        let now = 1600;
        let cleanup = app.project().issues_needing_session_cleanup(now);
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
        let names = app.project().done_worktree_names();
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
        let names = app.project().done_worktree_names();
        assert!(names.is_empty());
    }

    #[test]
    fn done_worktree_names_skips_issues_without_worktree() {
        let app = test_app(vec![test_issue("bork-99", Column::Done)]);
        let names = app.project().done_worktree_names();
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
        app.project_mut().live.worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 3,
                unstaged: 5,
            },
        );
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1".to_string(), "feature/test".to_string());

        app.project_mut().freeze_worktree_status("bork-1");

        assert!(app
            .project()
            .live
            .frozen_worktree_statuses
            .contains_key("bork-1"));
        let frozen = &app.project().live.frozen_worktree_statuses["bork-1"];
        assert_eq!(frozen.staged, 3);
        assert_eq!(frozen.unstaged, 5);
        assert_eq!(
            app.project_mut()
                .live
                .frozen_worktree_branches
                .get("bork-1"),
            Some(&"feature/test".to_string())
        );
    }

    #[test]
    fn unfreeze_worktree_removes_from_frozen() {
        let mut app = test_app(vec![]);
        app.project_mut().live.frozen_worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 1,
                unstaged: 2,
            },
        );
        app.project_mut()
            .live
            .frozen_worktree_branches
            .insert("bork-1".to_string(), "main".to_string());

        app.project_mut().unfreeze_worktree_status("bork-1");

        assert!(!app
            .project()
            .live
            .frozen_worktree_statuses
            .contains_key("bork-1"));
        assert!(!app
            .project()
            .live
            .frozen_worktree_branches
            .contains_key("bork-1"));
    }

    #[test]
    fn worktree_status_for_done_issue_uses_frozen() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.worktree = Some("bork-1".into());
        let mut app = test_app(vec![issue]);

        app.project_mut().live.frozen_worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 2,
                unstaged: 4,
            },
        );

        let status = app
            .project()
            .worktree_status_for(&app.project().issues[0].clone());
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

        app.project_mut().live.worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 1,
                unstaged: 0,
            },
        );
        app.project_mut().live.frozen_worktree_statuses.insert(
            "bork-1".to_string(),
            WorktreeStatus {
                staged: 99,
                unstaged: 99,
            },
        );

        let status = app
            .project()
            .worktree_status_for(&app.project().issues[0].clone());
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

        app.project_mut()
            .live
            .frozen_worktree_branches
            .insert("bork-1".to_string(), "feature/done".to_string());

        let branch = app.project().branch_for(&app.project().issues[0].clone());
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
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());
        app.project_mut().live.agent_statuses.insert(
            "bork-bork-1".to_string(),
            AgentStatusInfo {
                status: AgentStatus::Busy,
                activity: Some("Edit".to_string()),
                updated_at: 0,
            },
        );
        assert_eq!(
            app.project().resolved_agent_status(&issue),
            AgentStatus::Busy
        );
    }

    #[test]
    fn resolved_status_dead_with_stale_status_file() {
        let issue = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![issue.clone()]);
        // Status file says Busy but session is not alive
        app.project_mut().live.agent_statuses.insert(
            "bork-bork-1".to_string(),
            AgentStatusInfo {
                status: AgentStatus::Busy,
                activity: None,
                updated_at: 0,
            },
        );
        assert_eq!(
            app.project().resolved_agent_status(&issue),
            AgentStatus::Stopped
        );
    }

    #[test]
    fn resolved_status_alive_no_status_file() {
        let issue = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![issue.clone()]);
        app.project_mut()
            .live
            .active_sessions
            .insert("bork-bork-1".to_string());
        assert_eq!(
            app.project().resolved_agent_status(&issue),
            AgentStatus::Idle
        );
    }

    #[test]
    fn resolved_status_dead_no_status_file() {
        let issue = test_issue("bork-1", Column::InProgress);
        let app = test_app(vec![issue.clone()]);
        assert_eq!(
            app.project().resolved_agent_status(&issue),
            AgentStatus::Stopped
        );
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
        assert_eq!(app.project().next_issue_id(), "bork-4");
    }

    #[test]
    fn next_issue_id_starts_at_one() {
        let app = test_app(vec![]);
        assert_eq!(app.project().next_issue_id(), "bork-1");
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
        let todo = app.project().issues_in_column(Column::Todo);
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
        app.project_mut().search_query = "fix".to_string();
        let results = app.project().issues_in_column(Column::Todo);
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
        app.project_mut().search_query = "fix login".to_string();
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);

        app.project_mut().search_query = "FIX LOGIN".to_string();
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);
    }

    #[test]
    fn search_matches_issue_id() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login bug", Column::Todo),
            test_issue_titled("bork-2", "Add dark mode", Column::Todo),
        ]);
        app.project_mut().search_query = "bork-2".to_string();
        let results = app.project().issues_in_column(Column::Todo);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.id, "bork-2");
    }

    #[test]
    fn search_matches_partial_id() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-12", "Add feature", Column::Todo),
        ]);
        app.project_mut().search_query = "bork-1".to_string();
        let results = app.project().issues_in_column(Column::Todo);
        assert_eq!(results.len(), 2, "bork-1 and bork-12 both contain 'bork-1'");
    }

    #[test]
    fn search_empty_query_returns_all() {
        let mut app = test_app(vec![
            test_issue("bork-1", Column::Todo),
            test_issue("bork-2", Column::Todo),
        ]);
        app.project_mut().search_query = String::new();
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 2);
    }

    #[test]
    fn search_no_matches_returns_empty() {
        let mut app = test_app(vec![test_issue_titled(
            "bork-1",
            "Fix login bug",
            Column::Todo,
        )]);
        app.project_mut().search_query = "zzzzz".to_string();
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 0);
    }

    #[test]
    fn search_filters_across_columns() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Fix crash", Column::InProgress),
            test_issue_titled("bork-3", "Add feature", Column::Todo),
            test_issue_titled("bork-4", "Fix timeout", Column::Done),
        ]);
        app.project_mut().search_query = "fix".to_string();
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);
        assert_eq!(app.project().issues_in_column(Column::InProgress).len(), 1);
        assert_eq!(app.project().issues_in_column(Column::CodeReview).len(), 0);
        assert_eq!(app.project().issues_in_column(Column::Done).len(), 1);
    }

    #[test]
    fn search_preserves_global_indices() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::Todo),
            test_issue_titled("bork-3", "Fix crash", Column::Todo),
        ]);
        app.project_mut().search_query = "fix".to_string();
        let results = app.project().issues_in_column(Column::Todo);
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
        app.project_mut().search_query = "fix".to_string();
        app.confirm_search();
        assert_eq!(app.input_mode, InputMode::Normal);

        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);
        assert_eq!(
            app.project_mut().search_query,
            "fix",
            "/ should preserve existing query"
        );
    }

    // ================================================================
    // Search: confirm_search
    // ================================================================

    #[test]
    fn confirm_search_returns_to_normal_with_filter_active() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix login", Column::Todo)]);
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.confirm_search();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(
            app.project_mut().search_query,
            "f",
            "filter should remain after confirm"
        );
    }

    // ================================================================
    // Search: cancel_search
    // ================================================================

    #[test]
    fn cancel_search_clears_query_and_returns_to_normal() {
        let mut app = test_app(vec![]);
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.cancel_search(&ctx);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.project_mut().search_query.is_empty());
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
        app.project_mut().search_query = "fix".to_string();
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);

        let ctx = app.action_context();
        app.clear_search(&ctx);
        assert!(app.project_mut().search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 2);
    }

    #[test]
    fn clear_search_noop_when_no_filter() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        let ctx = app.action_context();
        app.clear_search(&ctx);
        assert!(app.project_mut().search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);
    }

    // ================================================================
    // Search: has_active_search
    // ================================================================

    #[test]
    fn has_active_search_false_when_empty() {
        let app = test_app(vec![]);
        let ctx = app.action_context();
        assert!(!app.has_active_search());
    }

    #[test]
    fn has_active_search_true_when_query_set() {
        let mut app = test_app(vec![]);
        app.project_mut().search_query = "test".to_string();
        let ctx = app.action_context();
        assert!(app.has_active_search());
    }

    // ================================================================
    // Search: search_push_char + auto-focus first match
    // ================================================================

    #[test]
    fn search_push_char_appends_to_query() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix bug", Column::Todo)]);
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        assert_eq!(app.project_mut().search_query, "f");
        app.search_push_char('i', &ctx);
        assert_eq!(app.project_mut().search_query, "fi");
        app.search_push_char('x', &ctx);
        assert_eq!(app.project_mut().search_query, "fix");
    }

    #[test]
    fn search_auto_focuses_first_match_column() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::InProgress),
        ]);
        let ctx = app.action_context();
        app.project_mut().selected_column = 0;
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.search_push_char('x', &ctx);

        assert_eq!(
            app.project_mut().selected_column,
            1,
            "should focus InProgress where the match is"
        );
        assert_eq!(app.project().selected_row[1], 0);
    }

    #[test]
    fn search_auto_focus_skips_empty_columns() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Deploy fix", Column::Done),
        ]);
        let ctx = app.action_context();
        app.project_mut().selected_column = 0;
        app.start_search();
        app.search_push_char('d', &ctx);
        app.search_push_char('e', &ctx);

        assert_eq!(
            app.project_mut().selected_column,
            3,
            "should skip empty columns and focus Done"
        );
    }

    #[test]
    fn search_auto_focus_stays_when_current_column_has_matches() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Fix crash", Column::InProgress),
        ]);
        let ctx = app.action_context();
        app.project_mut().selected_column = 0;
        app.start_search();
        app.search_push_char('f', &ctx);

        assert_eq!(
            app.project_mut().selected_column,
            0,
            "Todo has a match so focus should be on first column with matches"
        );
    }

    // ================================================================
    // Search: search_delete_char
    // ================================================================

    #[test]
    fn search_delete_char_removes_last_char() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix bug", Column::Todo)]);
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.search_push_char('x', &ctx);
        app.search_delete_char(&ctx);
        assert_eq!(app.project_mut().search_query, "fi");
    }

    #[test]
    fn search_backspace_on_empty_cancels_search() {
        let mut app = test_app(vec![]);
        let ctx = app.action_context();
        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);

        app.search_delete_char(&ctx);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.project_mut().search_query.is_empty());
    }

    #[test]
    fn search_backspace_on_single_char_stays_in_search() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix bug", Column::Todo)]);
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_delete_char(&ctx);

        assert_eq!(app.input_mode, InputMode::Search);
        assert!(app.project_mut().search_query.is_empty());
    }

    #[test]
    fn search_delete_char_refocuses_first_match() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Add dark mode", Column::InProgress),
        ]);
        let ctx = app.action_context();
        app.start_search();
        // Type "add f" — only matches "Add feature" in Todo
        for c in "add f".chars() {
            app.search_push_char(c, &ctx);
        }
        assert_eq!(app.project().selected_column, 0);

        // Delete "f" — now "add" matches both columns
        app.search_delete_char(&ctx);
        assert_eq!(
            app.project().selected_column,
            0,
            "first match is still in Todo"
        );
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
        let ctx = app.action_context();
        app.project_mut().selected_column = 0;
        app.project_mut().selected_row[0] = 2; // selecting "Add feature"

        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.search_push_char('x', &ctx);

        // Only 2 results remain (bork-1 and bork-2), row 2 is out of bounds
        let count = app.project().issues_in_column(Column::Todo).len();
        assert_eq!(count, 2);
        assert!(
            app.project_mut().selected_row[0] < count,
            "row should be clamped to valid range"
        );
    }

    #[test]
    fn search_clamps_row_to_zero_when_column_empty() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix login", Column::Todo)]);
        let ctx = app.action_context();
        app.project_mut().selected_column = 0;
        app.project_mut().selected_row[0] = 0;

        app.start_search();
        app.search_push_char('z', &ctx);
        app.search_push_char('z', &ctx);

        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 0);
        assert_eq!(app.project().selected_row[0], 0);
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
        let ctx = app.action_context();

        // Start search
        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);

        // Type query
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.search_push_char('x', &ctx);
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);
        assert_eq!(app.project().issues_in_column(Column::InProgress).len(), 0);

        // Confirm — filter stays, back to normal
        app.confirm_search();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.project_mut().search_query, "fix");
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);

        // Clear — all issues visible again
        app.clear_search(&ctx);
        assert!(app.project_mut().search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);
        assert_eq!(app.project().issues_in_column(Column::InProgress).len(), 1);
    }

    #[test]
    fn search_full_flow_type_cancel() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Add feature", Column::Todo),
        ]);
        let ctx = app.action_context();

        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);

        // Cancel — clears query, all issues back
        app.cancel_search(&ctx);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.project_mut().search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 2);
    }

    #[test]
    fn search_reenter_preserves_and_refines_query() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login bug", Column::Todo),
            test_issue_titled("bork-2", "Fix logout crash", Column::Todo),
        ]);
        let ctx = app.action_context();

        // First search: "fix"
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.search_push_char('x', &ctx);
        app.confirm_search();
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 2);

        // Re-enter: query still "fix", refine to "fix log"
        app.start_search();
        assert_eq!(app.project_mut().search_query, "fix");
        app.search_push_char(' ', &ctx);
        app.search_push_char('l', &ctx);
        app.search_push_char('o', &ctx);
        app.search_push_char('g', &ctx);
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 2);

        // Refine further to "fix login"
        app.search_push_char('i', &ctx);
        app.search_push_char('n', &ctx);
        assert_eq!(app.project().issues_in_column(Column::Todo).len(), 1);
        assert_eq!(
            app.project().issues_in_column(Column::Todo)[0].1.id,
            "bork-1"
        );
    }

    #[test]
    fn search_selected_issue_works_with_filter() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::Todo),
            test_issue_titled("bork-3", "Fix crash", Column::Todo),
        ]);
        app.project_mut().selected_column = 0;

        app.project_mut().search_query = "fix".to_string();
        app.project_mut().clamp_all_rows();
        app.project_mut().selected_row[0] = 0;

        let issue = app
            .project()
            .selected_issue()
            .expect("should have selected issue");
        assert_eq!(issue.id, "bork-2", "first filtered result should be bork-2");

        app.project_mut().selected_row[0] = 1;
        let issue = app
            .project()
            .selected_issue()
            .expect("should have selected issue");
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
        app.project_mut().selected_column = 0;

        app.project_mut().search_query = "fix".to_string();
        app.project_mut().clamp_all_rows();
        app.project_mut().selected_row[0] = 0;

        let idx = app
            .project()
            .selected_issue_index()
            .expect("should have index");
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
        app.project_mut().linear_available = true;
        app.project_mut().live.linear_issues = vec![];

        let ctx = app.action_context();
        app.open_linear_picker(&ctx);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.linear_picker.is_none());
    }

    #[test]
    fn open_linear_picker_with_issues() {
        let mut app = test_app(vec![]);
        app.project_mut().linear_available = true;
        app.project_mut().live.linear_issues =
            vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        let ctx = app.action_context();
        app.open_linear_picker(&ctx);
        assert_eq!(app.input_mode, InputMode::LinearPicker);
        assert!(app.linear_picker.is_some());
    }

    #[test]
    fn close_linear_picker_restores_normal_mode() {
        let mut app = test_app(vec![]);
        app.project_mut().linear_available = true;
        app.project_mut().live.linear_issues =
            vec![test_linear_issue("uuid-1", "TEST-1", "First issue")];

        let ctx = app.action_context();
        app.open_linear_picker(&ctx);
        app.close_linear_picker();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.linear_picker.is_none());
    }

    #[test]
    fn filtered_linear_issues_includes_already_imported() {
        let mut issue = test_issue("test-1", Column::Todo);
        issue.linear_id = Some("uuid-1".to_string());
        let mut app = test_app(vec![issue]);

        app.project_mut().live.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "Already imported"),
            test_linear_issue("uuid-2", "TEST-2", "Not imported"),
        ];
        let ctx = app.action_context();
        app.open_linear_picker(&ctx);

        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filtered_linear_issues_filters_by_search() {
        let mut app = test_app(vec![]);
        app.project_mut().live.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "Add login page"),
            test_linear_issue("uuid-2", "TEST-2", "Fix dashboard bug"),
            test_linear_issue("uuid-3", "TEST-3", "Add logout button"),
        ];
        let ctx = app.action_context();
        app.open_linear_picker(&ctx);

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
        app.project_mut().live.linear_issues = vec![
            test_linear_issue("uuid-1", "TEST-1", "First"),
            test_linear_issue("uuid-2", "DOC-99", "Second"),
        ];
        let ctx = app.action_context();
        app.open_linear_picker(&ctx);

        if let Some(ref mut picker) = app.linear_picker {
            picker.search = "doc".to_string();
        }

        let filtered = app.filtered_linear_issues();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].identifier, "DOC-99");
    }

    // --- Multi-project tests ---

    fn test_config_named(name: &str) -> AppConfig {
        AppConfig {
            project_name: name.into(),
            project_root: PathBuf::from(format!("/tmp/test-{}", name)),
            agent_kind: AgentKind::OpenCode,
            default_prompt: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
        }
    }

    fn test_multi_app() -> App {
        let mut app = App::new(
            test_config_named("alpha"),
            AppState {
                issues: vec![test_issue("alpha-1", Column::Todo)],
            },
        );
        app.add_background_project(
            test_config_named("beta"),
            AppState {
                issues: vec![
                    test_issue("beta-1", Column::Todo),
                    test_issue("beta-2", Column::InProgress),
                ],
            },
        );
        app.add_background_project(
            test_config_named("gamma"),
            AppState {
                issues: vec![test_issue("gamma-1", Column::CodeReview)],
            },
        );
        app.enable_sidebar();
        app
    }

    #[test]
    fn visible_swimlanes_default_single() {
        let app = test_multi_app();
        let lanes = app.visible_swimlanes();
        assert_eq!(lanes, vec![app.projects[0].id()]);
    }

    #[test]
    fn visible_swimlanes_with_ids() {
        let mut app = test_multi_app();
        let ids: Vec<ProjectId> = vec![app.projects[0].id(), app.projects[1].id()];
        app.sidebar.as_mut().unwrap().swimlanes = ids.clone();
        let lanes = app.visible_swimlanes();
        assert_eq!(lanes, ids);
    }

    #[test]
    fn visible_swimlanes_filters_invalid() {
        let mut app = test_multi_app();
        let valid_id = app.projects[0].id();
        let bogus_id = PathBuf::from("/tmp/nonexistent");
        app.sidebar.as_mut().unwrap().swimlanes = vec![valid_id.clone(), bogus_id];
        let lanes = app.visible_swimlanes();
        assert_eq!(lanes, vec![valid_id]);
    }

    #[test]
    fn visible_swimlane_count_matches_vec() {
        let mut app = test_multi_app();
        assert_eq!(app.visible_swimlane_count(), app.visible_swimlanes().len());
        app.sidebar.as_mut().unwrap().swimlanes = vec![app.projects[0].id(), app.projects[2].id()];
        assert_eq!(app.visible_swimlane_count(), app.visible_swimlanes().len());
        app.sidebar.as_mut().unwrap().swimlanes = vec![
            app.projects[0].id(),
            app.projects[1].id(),
            app.projects[2].id(),
        ];
        assert_eq!(app.visible_swimlane_count(), app.visible_swimlanes().len());
    }

    #[test]
    fn active_project_id_default() {
        let app = test_multi_app();
        assert_eq!(app.active_project_id(), app.projects[0].id());
        assert_eq!(app.active_project().config.project_name, "alpha");
    }

    #[test]
    fn active_project_id_with_swimlanes() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().swimlanes = vec![
            app.projects[0].id(),
            app.projects[1].id(),
            app.projects[2].id(),
        ];
        app.focused_swimlane = 0;
        assert_eq!(app.active_project_id(), app.projects[0].id());
        app.focused_swimlane = 1;
        assert_eq!(app.active_project_id(), app.projects[1].id());
        assert_eq!(app.active_project().config.project_name, "beta");
        app.focused_swimlane = 2;
        assert_eq!(app.active_project_id(), app.projects[2].id());
        assert_eq!(app.active_project().config.project_name, "gamma");
    }

    #[test]
    fn active_project_id_out_of_range_fallback() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().swimlanes = vec![app.projects[0].id()];
        app.focused_swimlane = 5;
        assert_eq!(app.active_project_id(), app.focused_project);
    }

    #[test]
    fn card_size_by_swimlane_count() {
        let mut app = test_multi_app();
        assert_eq!(app.card_size(), CardSize::Full);

        app.sidebar.as_mut().unwrap().swimlanes = vec![app.projects[0].id(), app.projects[1].id()];
        assert_eq!(app.card_size(), CardSize::Medium);

        app.sidebar.as_mut().unwrap().swimlanes = vec![
            app.projects[0].id(),
            app.projects[1].id(),
            app.projects[2].id(),
        ];
        assert_eq!(app.card_size(), CardSize::Compact);
    }

    #[test]
    fn add_background_project() {
        let mut app = test_app(vec![test_issue("a-1", Column::Todo)]);
        assert_eq!(app.projects.len(), 1);
        app.add_background_project(test_config_named("other"), AppState { issues: vec![] });
        assert_eq!(app.projects.len(), 2);
        assert_eq!(app.projects[1].config.project_name, "other");
    }

    #[test]
    fn enable_sidebar_needs_two_projects() {
        let mut app = test_app(vec![]);
        app.enable_sidebar();
        assert!(app.sidebar.is_none());

        app.add_background_project(test_config_named("b"), AppState { issues: vec![] });
        app.enable_sidebar();
        assert!(app.sidebar.is_some());
    }

    #[test]
    fn project_switch_updates_focused() {
        let mut app = test_multi_app();
        assert_eq!(app.focused_project, app.projects[0].id());
        app.focused_project = app.projects[2].id();
        assert_eq!(app.project().config.project_name, "gamma");
    }

    #[test]
    fn active_project_mut_modifies_correct_project() {
        let mut app = test_multi_app();
        app.sidebar.as_mut().unwrap().swimlanes = vec![app.projects[0].id(), app.projects[1].id()];
        app.focused_swimlane = 1;
        app.active_project_mut()
            .issues
            .push(test_issue("beta-3", Column::Todo));
        assert_eq!(app.projects[1].issues.len(), 3);
        assert_eq!(app.projects[0].issues.len(), 1);
    }

    // --- High-impact multi-project tests ---

    #[test]
    fn action_context_survives_swimlane_switch() {
        let mut app = test_multi_app();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes =
            vec![app.projects[0].id(), beta_id.clone(), app.projects[2].id()];
        app.focused_swimlane = 1;

        let ctx = app.action_context();
        assert_eq!(ctx.project_id, beta_id);

        app.focused_swimlane = 0;
        assert_eq!(
            ctx.project_id, beta_id,
            "context should still point to beta after swimlane switch"
        );

        let resolved = app.context_project(&ctx);
        assert_eq!(resolved.config.project_name, "beta");
    }

    #[test]
    fn find_project_with_unknown_id_returns_none() {
        let app = test_multi_app();
        let bogus = PathBuf::from("/nonexistent/path");
        assert!(app.find_project(&bogus).is_none());
    }

    #[test]
    fn focused_project_id_stable_after_adding_projects() {
        let mut app = test_multi_app();
        let original_focused = app.focused_project.clone();

        app.add_background_project(test_config_named("delta"), AppState { issues: vec![] });

        assert_eq!(app.focused_project, original_focused);
        assert_eq!(app.project().config.project_name, "alpha");
    }

    #[test]
    fn visible_swimlanes_filters_bogus_ids() {
        let mut app = test_multi_app();
        let bogus = PathBuf::from("/nonexistent/path");
        app.sidebar.as_mut().unwrap().swimlanes =
            vec![app.projects[0].id(), bogus, app.projects[1].id()];

        let lanes = app.visible_swimlanes();
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0], app.projects[0].id());
        assert_eq!(lanes[1], app.projects[1].id());
    }

    #[test]
    fn swimlane_toggle_roundtrip() {
        let mut app = test_multi_app();
        let beta_id = app.projects[1].id();

        assert_eq!(app.sidebar.as_ref().unwrap().swimlanes.len(), 1);

        app.sidebar
            .as_mut()
            .unwrap()
            .swimlanes
            .push(beta_id.clone());
        assert_eq!(app.visible_swimlane_count(), 2);

        let pos = app
            .sidebar
            .as_ref()
            .unwrap()
            .swimlanes
            .iter()
            .position(|id| *id == beta_id)
            .unwrap();
        app.sidebar.as_mut().unwrap().swimlanes.remove(pos);
        assert_eq!(app.visible_swimlane_count(), 1);

        app.sidebar
            .as_mut()
            .unwrap()
            .swimlanes
            .push(beta_id.clone());
        assert_eq!(app.visible_swimlane_count(), 2);
        assert!(app.find_project(&beta_id).is_some());
    }

    #[test]
    fn search_is_per_project() {
        let mut app = test_multi_app();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone(), beta_id.clone()];

        app.focused_swimlane = 0;
        let ctx = app.action_context();
        app.search_push_char('x', &ctx);
        assert_eq!(app.projects[0].search_query, "x");
        assert_eq!(app.projects[1].search_query, "");

        app.focused_swimlane = 1;
        let ctx = app.action_context();
        app.search_push_char('y', &ctx);
        assert_eq!(app.projects[0].search_query, "x");
        assert_eq!(app.projects[1].search_query, "y");
    }

    #[test]
    fn context_project_mut_falls_back_to_focused() {
        let mut app = test_multi_app();
        let bogus_ctx = ActionContext {
            project_id: PathBuf::from("/nonexistent"),
        };
        let project = app.context_project_mut(&bogus_ctx);
        assert_eq!(project.config.project_name, "alpha");
    }
}
