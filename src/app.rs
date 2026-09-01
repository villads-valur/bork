use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Minimum on-screen time for the busy spinner. Holds the spinner visible
/// for this long after the last in-flight action finishes so very fast
/// actions don't appear as a single-frame flash.
const BUSY_MIN_VISIBLE: Duration = Duration::from_millis(250);

use crate::config::{AppConfig, AppState, DEFAULT_REVIEW_PROMPT};
use crate::external::linear::LinearIssue;
use crate::prune::{PruneAction, PruneCandidate};
use crate::types::{
    AgentKind, AgentStatus, AgentStatusInfo, Column, GithubStack, Issue, IssueKind, LinkedGithubPr,
    PrImportSource, PrState, PrStatus, WorktreeStatus,
};

pub type ProjectId = PathBuf;

pub(crate) fn unix_now() -> u64 {
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
    LinkPicker,
    Help,
    DebugInspector,
    Sidebar,
    PruneDialog,
}

#[derive(Default)]
pub struct LiveState {
    pub active_sessions: HashSet<String>,
    pub agent_statuses: HashMap<String, AgentStatusInfo>,
    pub listening_ports: HashMap<String, Vec<u16>>,
    pub worktree_statuses: HashMap<String, WorktreeStatus>,
    pub worktree_branches: HashMap<String, String>,
    pub pr_statuses: HashMap<String, PrStatus>,
    pub pr_statuses_by_number: HashMap<u32, PrStatus>,
    pub github_stacks: Vec<GithubStack>,
    pub frozen_worktree_statuses: HashMap<String, WorktreeStatus>,
    pub frozen_worktree_branches: HashMap<String, String>,
    pub linear_issues: Vec<LinearIssue>,
    pub user_prs: Vec<PrStatus>,
    pub review_requested_prs: Vec<PrStatus>,
    pub github_user: Option<String>,
    pub git_poll_done: bool,
    pub pr_poll_done: bool,
}

impl LiveState {
    pub fn has_github_prs(&self) -> bool {
        !self.pr_statuses.is_empty()
            || !self.user_prs.is_empty()
            || !self.review_requested_prs.is_empty()
    }
}

pub struct Project {
    /// Canonicalized project root, computed once at construction. `id()` is
    /// called in per-frame render paths, so it must not hit the filesystem.
    id: ProjectId,
    pub issues: Vec<Issue>,
    pub config: AppConfig,
    pub available_agents: Vec<AgentKind>,
    pub selected_column: usize,
    pub selected_row: [usize; 4],
    pub marked_issues: HashSet<String>,
    /// When set, the board shows only the connected component of links that
    /// contains this issue id (the anchor itself plus everything reachable
    /// through `linked_issues`). Cleared with the same key or Esc.
    pub link_filter: Option<String>,
    pub linear_available: bool,
    pub tuicr_available: bool,
    pub live: LiveState,
    pub state_dirty: bool,
    pub base_issues: Vec<Issue>,
    pub last_state_mtime: Option<SystemTime>,
    /// Unix timestamp of the last completed prune, persisted in state.json.
    pub last_prune_at: Option<u64>,
    /// Most recent time we ran the auto-prune threshold check for this
    /// project. Ephemeral; throttles both the check and its toast.
    pub last_auto_prune_check: Option<Instant>,
    pub last_config_mtime: Option<SystemTime>,
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
        let id = std::fs::canonicalize(&config.project_root)
            .unwrap_or_else(|_| config.project_root.clone());
        let last_config_mtime = crate::config::config_mtime(&config.project_root);
        Project {
            id,
            issues,
            config,
            available_agents: AgentKind::ALL.to_vec(),
            selected_column: 0,
            selected_row: [0; 4],
            marked_issues: HashSet::new(),
            link_filter: None,
            linear_available: false,
            tuicr_available: false,
            live: LiveState::default(),
            state_dirty: false,
            base_issues,
            last_state_mtime,
            last_prune_at: state.last_prune_at,
            last_auto_prune_check: None,
            last_config_mtime,
        }
    }

    /// Re-read the layered config from disk, replacing `self.config`. Used to
    /// pick up `bork config set` edits without a TUI restart. Leaves
    /// `available_agents` (resolved at startup) untouched.
    pub fn reload_config(&mut self) {
        self.config = crate::config::load_config_from(&self.config.project_root);
        self.last_config_mtime = crate::config::config_mtime(&self.config.project_root);
    }

    pub fn id(&self) -> ProjectId {
        self.id.clone()
    }

    pub fn set_available_agents(
        &mut self,
        available_agents: Vec<AgentKind>,
        default_agent: Option<AgentKind>,
    ) {
        self.available_agents = available_agents;
        let Some(default_agent) = default_agent else {
            return;
        };
        let Some(index) = self
            .available_agents
            .iter()
            .position(|kind| *kind == default_agent)
        else {
            return;
        };
        if index > 0 {
            let kind = self.available_agents.remove(index);
            self.available_agents.insert(0, kind);
        }
    }

    pub fn dialog_default_agent(&self) -> AgentKind {
        if self.available_agents.contains(&self.config.agent_kind) {
            return self.config.agent_kind;
        }
        self.available_agents
            .first()
            .copied()
            .unwrap_or(self.config.agent_kind)
    }

    pub fn mark_dirty(&mut self) {
        self.state_dirty = true;
    }

    pub fn to_state(&self) -> AppState {
        AppState {
            issues: self.issues.clone(),
            last_prune_at: self.last_prune_at,
        }
    }

    pub fn update_base_snapshot(&mut self) {
        self.base_issues = self.issues.clone();
        self.last_state_mtime = crate::config::state_mtime(&self.config.project_root);
    }

    pub fn merge_external_state(&mut self, file_state: AppState) {
        // Timestamps only move forward, so the later value wins regardless
        // of whether it came from memory or an external writer.
        self.last_prune_at = self.last_prune_at.max(file_state.last_prune_at);
        let file_issues = file_state.issues;

        if !self.state_dirty {
            // No local changes pending, safe to fully replace
            self.issues = file_issues.clone();
            self.base_issues = file_issues;
            self.clear_stale_link_filter();
            self.clear_stale_marks();
            self.clamp_all_rows("");
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

        // Field-level merge for issues present in both memory and file.
        // Indexed by ID to avoid an O(issues^2) scan per merge.
        let file_by_id: HashMap<&str, &Issue> =
            file_issues.iter().map(|i| (i.id.as_str(), i)).collect();
        let base_by_id: HashMap<&str, &Issue> = self
            .base_issues
            .iter()
            .map(|i| (i.id.as_str(), i))
            .collect();
        for issue in &mut self.issues {
            let Some(file_issue) = file_by_id.get(issue.id.as_str()).copied() else {
                continue;
            };
            let Some(base_issue) = base_by_id.get(issue.id.as_str()).copied() else {
                // No base means this issue was added after last snapshot; keep memory version
                continue;
            };
            merge_issue_fields(issue, base_issue, file_issue);
        }

        self.base_issues = file_issues;
        self.clear_stale_link_filter();
        self.clear_stale_marks();
        self.clamp_all_rows("");
    }

    /// Drop the link filter when its anchor issue no longer exists (e.g. it was
    /// deleted via the CLI while the filter was active), so the board doesn't
    /// get stuck showing nothing.
    fn clear_stale_link_filter(&mut self) {
        let stale = self.link_filter.as_deref().is_some_and(|anchor| {
            !self
                .issues
                .iter()
                .any(|i| i.id.eq_ignore_ascii_case(anchor))
        });
        if stale {
            self.link_filter = None;
        }
    }

    fn clear_stale_marks(&mut self) {
        let ids: HashSet<String> = self.issues.iter().map(|i| i.id.to_lowercase()).collect();
        self.marked_issues.retain(|id| ids.contains(id));
    }

    pub fn issues_in_column(&self, column: Column, query: &str) -> Vec<(usize, &Issue)> {
        let query = query.to_lowercase();
        let component = self
            .link_filter
            .as_deref()
            .map(|anchor| self.linked_component(anchor));
        self.issues
            .iter()
            .enumerate()
            .filter(|(_, issue)| {
                issue.column == column
                    && (query.is_empty() || self.issue_matches(issue, &query))
                    && component
                        .as_ref()
                        .is_none_or(|c| c.contains(&issue.id.to_lowercase()))
            })
            .collect()
    }

    /// Connected component of the link graph containing `anchor` (BFS over
    /// `linked_issues`). Returns lowercased ids, including the anchor itself.
    pub fn linked_component(&self, anchor: &str) -> HashSet<String> {
        crate::ops::linked_component(&self.issues, anchor)
    }

    fn issue_matches(&self, issue: &Issue, query: &str) -> bool {
        issue.title.to_lowercase().contains(query)
            || issue.id.to_lowercase().contains(query)
            || issue
                .linear_links
                .iter()
                .any(|l| l.identifier.to_lowercase().contains(query))
            || issue
                .github_pr_links
                .iter()
                .any(|link| format!("#{}", link.number).contains(query))
            || self
                .branch_for(issue)
                .is_some_and(|b| b.to_lowercase().contains(query))
            || self
                .pr_for(issue)
                .is_some_and(|pr| pr.title.to_lowercase().contains(query))
    }

    pub fn selected_issue(&self, query: &str) -> Option<&Issue> {
        let column = Column::from_index(self.selected_column)?;
        let items = self.issues_in_column(column, query);
        let row = self.selected_row[self.selected_column];
        items.get(row).map(|(_, issue)| *issue)
    }

    pub fn selected_issue_index(&self, query: &str) -> Option<usize> {
        let column = Column::from_index(self.selected_column)?;
        let items = self.issues_in_column(column, query);
        let row = self.selected_row[self.selected_column];
        items.get(row).map(|(idx, _)| *idx)
    }

    pub fn move_selection_up(&mut self) {
        let row = &mut self.selected_row[self.selected_column];
        if *row > 0 {
            *row -= 1;
        }
    }

    pub fn move_selection_down(&mut self, query: &str) {
        let Some(column) = Column::from_index(self.selected_column) else {
            return;
        };
        let count = self.issues_in_column(column, query).len();
        let row = &mut self.selected_row[self.selected_column];
        if count > 0 && *row < count - 1 {
            *row += 1;
        }
    }

    pub fn jump_column_left(&mut self, query: &str) {
        if self.selected_column > 0 {
            self.selected_column -= 1;
            self.clamp_row(query);
        }
    }

    pub fn jump_column_right(&mut self, query: &str) {
        if self.selected_column < 3 {
            self.selected_column += 1;
            self.clamp_row(query);
        }
    }

    pub fn focus_left(&mut self, query: &str) {
        let row = self.selected_row[self.selected_column];
        if row > 0 {
            self.selected_row[self.selected_column] = row - 1;
        } else {
            let mut col = self.selected_column;
            while col > 0 {
                col -= 1;
                let count = self.column_count(col, query);
                if count > 0 {
                    self.selected_column = col;
                    self.selected_row[col] = count - 1;
                    return;
                }
            }
        }
    }

    pub fn focus_right(&mut self, query: &str) {
        let Some(column) = Column::from_index(self.selected_column) else {
            return;
        };
        let count = self.issues_in_column(column, query).len();
        let row = self.selected_row[self.selected_column];

        if count > 0 && row < count - 1 {
            self.selected_row[self.selected_column] = row + 1;
        } else {
            let mut col = self.selected_column;
            while col < 3 {
                col += 1;
                let count = self.column_count(col, query);
                if count > 0 {
                    self.selected_column = col;
                    self.selected_row[col] = 0;
                    return;
                }
            }
        }
    }

    fn column_count(&self, col_index: usize, query: &str) -> usize {
        match Column::from_index(col_index) {
            Some(c) => self.issues_in_column(c, query).len(),
            None => 0,
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.selected_row[self.selected_column] = 0;
    }

    pub fn scroll_to_bottom(&mut self, query: &str) {
        let Some(column) = Column::from_index(self.selected_column) else {
            return;
        };
        let count = self.issues_in_column(column, query).len();
        if count > 0 {
            self.selected_row[self.selected_column] = count - 1;
        }
    }

    pub fn toggle_mark(&mut self, query: &str) -> Option<usize> {
        let issue_id = self.selected_issue(query)?.id.to_lowercase();
        if !self.marked_issues.insert(issue_id.clone()) {
            self.marked_issues.remove(&issue_id);
        }
        Some(self.marked_issues.len())
    }

    pub fn mark_linked_component(&mut self, query: &str) -> Option<usize> {
        let issue_id = self.selected_issue(query)?.id.clone();
        for linked_id in self.linked_component(&issue_id) {
            self.marked_issues.insert(linked_id);
        }
        Some(self.marked_issues.len())
    }

    pub fn clear_marks(&mut self) {
        self.marked_issues.clear();
    }

    fn marked_issue_indices(&self) -> Vec<usize> {
        self.issues
            .iter()
            .enumerate()
            .filter(|(_, issue)| self.marked_issues.contains(&issue.id.to_lowercase()))
            .map(|(idx, _)| idx)
            .collect()
    }

    pub fn move_issue_right(&mut self, query: &str) -> usize {
        if !self.marked_issues.is_empty() {
            return self.move_marked(query, |column| column.next());
        }
        self.move_selected(query, |column| column.next())
    }

    pub fn move_issue_left(&mut self, query: &str) -> usize {
        if !self.marked_issues.is_empty() {
            return self.move_marked(query, |column| column.prev());
        }
        self.move_selected(query, |column| column.prev())
    }

    pub fn move_to_done(&mut self, query: &str) -> usize {
        if !self.marked_issues.is_empty() {
            return self.move_marked(query, |_| Some(Column::Done));
        }
        self.move_selected(query, |_| Some(Column::Done))
    }

    pub fn move_to_todo(&mut self, query: &str) -> usize {
        if !self.marked_issues.is_empty() {
            return self.move_marked(query, |_| Some(Column::Todo));
        }
        self.move_selected(query, |_| Some(Column::Todo))
    }

    /// Move the selected issue to the column produced by `target`. Returns the
    /// number of issues moved (0 or 1).
    fn move_selected(&mut self, query: &str, target: impl Fn(Column) -> Option<Column>) -> usize {
        let Some(idx) = self.selected_issue_index(query) else {
            return 0;
        };
        let Some(target) = target(self.issues[idx].column) else {
            return 0;
        };
        self.move_issue_to_column(idx, target);
        1
    }

    /// Move every marked issue to the column produced by `target`, skipping any
    /// that have no valid target. Clears the marks afterward and returns the
    /// number of issues actually moved.
    fn move_marked(&mut self, query: &str, target: impl Fn(Column) -> Option<Column>) -> usize {
        let mut moved = 0;
        for idx in self.marked_issue_indices() {
            let column = self.issues[idx].column;
            let Some(target) = target(column).filter(|t| *t != column) else {
                continue;
            };
            self.move_issue_to_column(idx, target);
            moved += 1;
        }
        self.clear_marks();
        self.clamp_all_rows(query);
        moved
    }

    pub fn move_issue_up(&mut self, query: &str) {
        self.reorder_issue(query, -1);
    }

    pub fn move_issue_down(&mut self, query: &str) {
        self.reorder_issue(query, 1);
    }

    fn reorder_issue(&mut self, query: &str, direction: isize) {
        let Some(column) = Column::from_index(self.selected_column) else {
            return;
        };
        let items = self.issues_in_column(column, query);
        let row = self.selected_row[self.selected_column];
        if row >= items.len() {
            return;
        }
        let Some(target_row) = row.checked_add_signed(direction) else {
            return;
        };
        if target_row >= items.len() {
            return;
        }
        let (a, b) = (items[row].0, items[target_row].0);
        self.issues.swap(a, b);
        self.selected_row[self.selected_column] = target_row;
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

    /// True when any issue in this project has a tmux session listening on a port
    /// (i.e. a dev env is running). Mirrors the per-card 🔌 condition.
    pub fn has_listening_ports(&self) -> bool {
        self.issues.iter().any(|issue| {
            self.listening_ports_for(issue)
                .is_some_and(|ports| !ports.is_empty())
        })
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
            if before_ok && after_ok && best.is_none_or(|b| dir_name.len() < b.len()) {
                best = Some(dir_name.as_str());
            }
        }
        best.map(|s| s.to_string())
    }

    pub fn auto_assign_worktrees(&mut self) -> bool {
        let assignments: Vec<(usize, String)> = (0..self.issues.len())
            .filter(|&i| {
                self.issues[i].worktree.is_none() && self.issues[i].kind != IssueKind::Orchestrator
            })
            .filter_map(|i| self.detect_worktree(&self.issues[i]).map(|wt| (i, wt)))
            .collect();

        if assignments.is_empty() {
            return false;
        }

        for (i, wt) in assignments {
            self.issues[i].attach_worktree(wt.clone());
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

        for link in &issue.github_pr_links {
            if let Some(pr) = live
                .user_prs
                .iter()
                .chain(live.review_requested_prs.iter())
                .find(|p| p.number == link.number)
            {
                // Fork PR branches don't exist locally, so skip them.
                if pr.is_cross_repository {
                    continue;
                }
                return Some(pr.head_branch.as_str());
            }
        }

        None
    }

    pub fn pr_for(&self, issue: &Issue) -> Option<&PrStatus> {
        let live = &self.live;
        for link in &issue.github_pr_links {
            if let Some(pr) = live.pr_statuses_by_number.get(&link.number) {
                return Some(pr);
            }
            let found = live
                .pr_statuses
                .values()
                .chain(live.user_prs.iter())
                .chain(live.review_requested_prs.iter())
                .find(|p| p.number == link.number);
            if found.is_some() {
                return found;
            }
        }
        if let Some(branch) = self.branch_for(issue) {
            if let Some(pr) = live.pr_statuses.get(branch) {
                return Some(pr);
            }
        }
        None
    }

    pub fn stack_for_pr(&self, number: u32) -> Option<&GithubStack> {
        self.live
            .github_stacks
            .iter()
            .find(|stack| stack.pull_requests.iter().any(|pr| pr.number == number))
    }

    pub fn stack_for_issue(&self, issue: &Issue) -> Option<&GithubStack> {
        if let Some(pr) = self.pr_for(issue) {
            return self.stack_for_pr(pr.number);
        }
        issue
            .github_pr_links
            .iter()
            .find_map(|link| self.stack_for_pr(link.number))
    }

    pub fn sync_prs_as_issues(&mut self) -> (bool, Option<String>) {
        if !self.live.pr_poll_done {
            return (false, None);
        }

        let live_authored_numbers: HashSet<u32> =
            self.live.user_prs.iter().map(|pr| pr.number).collect();
        let live_review_numbers: HashSet<u32> = self
            .live
            .review_requested_prs
            .iter()
            .map(|pr| pr.number)
            .collect();

        // Remove authored pr_imported issues whose PR is no longer in the live set
        // (closed, merged, or from a stale/wrong repo identity).
        // Review-requested issues are NOT removed, they get moved to Done instead.
        let before = self.issues.len();
        self.issues.retain(|issue| {
            if !issue.is_any_pr_imported() {
                return true;
            }
            if issue.primary_pr_import_source() == Some(PrImportSource::ReviewRequested) {
                return true;
            }
            issue
                .github_pr_links
                .iter()
                .any(|l| live_authored_numbers.contains(&l.number))
        });
        let removed = before - self.issues.len();

        // Move review-requested issues to Done when no longer pending
        let now = unix_now();
        let mut completed = 0usize;
        for issue in &mut self.issues {
            if !issue.is_any_pr_imported() {
                continue;
            }
            if issue.primary_pr_import_source() != Some(PrImportSource::ReviewRequested) {
                continue;
            }
            if issue.column == Column::Done {
                continue;
            }
            let has_live_review = issue
                .github_pr_links
                .iter()
                .any(|l| live_review_numbers.contains(&l.number));
            if !has_live_review {
                issue.column = Column::Done;
                issue.done_at = Some(now);
                completed += 1;
            }
        }

        // Build sets of already-claimed PRs for dedup
        let claimed_branches: HashSet<String> = self
            .issues
            .iter()
            .filter_map(|issue| self.branch_for(issue).map(|b| b.to_string()))
            .collect();

        let claimed_pr_numbers: HashSet<u32> = self
            .issues
            .iter()
            .flat_map(|issue| issue.pr_numbers())
            .collect();

        let issue_ids: Vec<String> = self.issues.iter().map(|i| i.id.to_lowercase()).collect();

        let mut new_issues: Vec<Issue> = Vec::new();

        // Helper: check if a PR should be imported
        let should_import = |pr: &PrStatus,
                             claimed_branches: &HashSet<String>,
                             claimed_pr_numbers: &HashSet<u32>,
                             new_pr_numbers: &HashSet<u32>,
                             issue_ids: &[String]|
         -> bool {
            if pr.state != PrState::Open || pr.is_draft {
                return false;
            }
            // Fork branches live in another repo's namespace, so skip the
            // branch checks and dedup fork PRs by number only.
            if !pr.is_cross_repository {
                let branch = &pr.head_branch;
                if branch == "main" || branch == "master" {
                    return false;
                }
                if claimed_branches.contains(branch) {
                    return false;
                }
                let branch_lower = branch.to_lowercase();
                let has_prefix_match = issue_ids.iter().any(|id| {
                    branch_lower.starts_with(&format!("{}/", id))
                        || branch_lower.starts_with(&format!("{}-", id))
                });
                if has_prefix_match {
                    return false;
                }
            }
            if claimed_pr_numbers.contains(&pr.number) || new_pr_numbers.contains(&pr.number) {
                return false;
            }
            true
        };

        let mut new_pr_numbers: HashSet<u32> = HashSet::new();

        // Import authored PRs
        let authored_prs: &[PrStatus] = if self.config.auto_import_authored_prs {
            &self.live.user_prs
        } else {
            &[]
        };
        for pr in authored_prs {
            if !should_import(
                pr,
                &claimed_branches,
                &claimed_pr_numbers,
                &new_pr_numbers,
                &issue_ids,
            ) {
                continue;
            }
            new_pr_numbers.insert(pr.number);
            let id = self.next_issue_id_after(new_issues.len() as u32);
            new_issues.push(self.imported_pr_issue(id, pr, PrImportSource::Authored));
        }

        // Import review-requested PRs
        let review_prs: &[PrStatus] = if self.config.auto_import_reviews {
            &self.live.review_requested_prs
        } else {
            &[]
        };
        for pr in review_prs {
            if !should_import(
                pr,
                &claimed_branches,
                &claimed_pr_numbers,
                &new_pr_numbers,
                &issue_ids,
            ) {
                continue;
            }
            new_pr_numbers.insert(pr.number);
            let id = self.next_issue_id_after(new_issues.len() as u32);
            new_issues.push(self.imported_pr_issue(id, pr, PrImportSource::ReviewRequested));
        }

        let added = new_issues.len();
        self.issues.append(&mut new_issues);

        if added == 0 && removed == 0 && completed == 0 {
            return (false, None);
        }

        let mut parts: Vec<String> = Vec::new();
        if added > 0 {
            parts.push(format!(
                "Imported {} PR{}",
                added,
                if added == 1 { "" } else { "s" }
            ));
        }
        if removed > 0 {
            parts.push(format!(
                "Removed {} stale PR{}",
                removed,
                if removed == 1 { "" } else { "s" }
            ));
        }
        if completed > 0 {
            parts.push(format!(
                "Completed {} review{}",
                completed,
                if completed == 1 { "" } else { "s" }
            ));
        }
        (true, Some(parts.join(", ")))
    }

    /// Issue skeleton for a PR auto-imported into Code Review. Review-requested
    /// PRs get a review prompt; authored PRs get none.
    fn imported_pr_issue(&self, id: String, pr: &PrStatus, source: PrImportSource) -> Issue {
        let prompt = match source {
            PrImportSource::Authored => None,
            PrImportSource::ReviewRequested => {
                let body = self
                    .config
                    .review_prompt
                    .as_deref()
                    .unwrap_or(DEFAULT_REVIEW_PROMPT);
                Some(format!(
                    "Review this PR: #{} ({}). {}",
                    pr.number, pr.url, body
                ))
            }
        };
        Issue {
            prompt,
            github_pr_links: vec![LinkedGithubPr {
                number: pr.number,
                imported: true,
                import_source: Some(source),
            }],
            ..Issue::new(
                id,
                pr.title.clone(),
                Column::CodeReview,
                self.config.agent_kind,
            )
        }
    }

    pub fn done_worktree_names(&self) -> HashSet<String> {
        self.issues
            .iter()
            .filter(|i| i.column == Column::Done)
            .filter_map(|i| i.worktree.clone())
            .collect()
    }

    pub fn freeze_worktree_status(&mut self, worktree: &str) {
        if let Some(status) = self.live.worktree_statuses.get(worktree).copied() {
            self.live
                .frozen_worktree_statuses
                .insert(worktree.to_string(), status);
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
        for pr in &live.review_requested_prs {
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
        crate::ops::next_issue_id_after(&self.issues, &self.config.project_name, offset)
    }

    pub fn clamp_all_rows(&mut self, query: &str) {
        for col in 0..4 {
            let count = self.column_count(col, query);
            if count == 0 {
                self.selected_row[col] = 0;
            } else if self.selected_row[col] >= count {
                self.selected_row[col] = count - 1;
            }
        }
    }

    fn clamp_row(&mut self, query: &str) {
        let Some(column) = Column::from_index(self.selected_column) else {
            return;
        };
        let count = self.issues_in_column(column, query).len();
        let row = &mut self.selected_row[self.selected_column];
        if count == 0 {
            *row = 0;
        } else if *row >= count {
            *row = count - 1;
        }
    }

    fn focus_first_match(&mut self, query: &str) {
        for col in 0..4 {
            if self.column_count(col, query) > 0 {
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

#[derive(Debug)]
pub struct LinkPickerState {
    /// The issue being linked from. Candidates are other issues in the same
    /// project; Enter toggles a link with the highlighted candidate.
    pub anchor_id: String,
    pub search: String,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct PruneDialogState {
    pub project_id: ProjectId,
    pub candidates: Vec<PruneCandidate>,
    pub selected: usize,
    pub error: Option<String>,
}

impl PruneDialogState {
    pub fn new(project_id: ProjectId, candidates: Vec<PruneCandidate>) -> Self {
        Self {
            project_id,
            candidates,
            selected: 0,
            error: None,
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.candidates.len() {
            self.selected += 1;
        }
    }

    pub fn toggle_current(&mut self) {
        let Some(candidate) = self.candidates.get_mut(self.selected) else {
            return;
        };
        candidate.action = match candidate.action {
            PruneAction::Keep => PruneAction::Remove,
            PruneAction::Remove => PruneAction::Keep,
        };
        self.error = None;
    }

    pub fn select_all_remove(&mut self) {
        self.set_all(PruneAction::Remove);
    }

    pub fn select_all_keep(&mut self) {
        self.set_all(PruneAction::Keep);
    }

    fn set_all(&mut self, action: PruneAction) {
        for candidate in &mut self.candidates {
            candidate.action = action;
        }
        self.error = None;
    }

    pub fn remove_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.action == PruneAction::Remove)
            .count()
    }

    pub fn dirty_remove_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.action == PruneAction::Remove && c.is_dirty())
            .count()
    }
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    KillSession {
        session_name: String,
        issue_id: String,
        project_id: ProjectId,
    },
    DeleteIssue {
        /// Stored by ID, not index: background workers can reorder/remove
        /// issues while the confirm prompt is open.
        issue_id: String,
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

pub use crate::dialog_state::{DialogField, DialogState};

/// 3-way field merge: for each field, if file diverged from base but memory didn't,
/// take the file value. If both diverged, memory wins.
fn crossed_orchestrator_boundary(issue: &Issue, base: &Issue) -> bool {
    (issue.kind == IssueKind::Orchestrator) != (base.kind == IssueKind::Orchestrator)
}

fn merge_issue_fields(memory: &mut Issue, base: &Issue, file: &Issue) {
    // Must be captured before merge_field!(kind) overwrites memory.kind.
    let memory_crossed_boundary = crossed_orchestrator_boundary(memory, base);

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
    merge_field!(pruned_at);
    merge_field!(setup_ran);
    merge_field!(linear_links);
    merge_field!(github_pr_links);
    merge_field!(linked_issues);

    // `sessions` merges entry-wise: per-agent entries are independent, so a
    // concurrent write to one agent's session (e.g. `bork issue start` from a
    // spawned agent) must not clobber another agent's entry held in memory.
    // Exception: a memory-side crossing of the orchestrator boundary cleared
    // the whole map, and that clear must win over concurrent file-side
    // inserts — resuming any pre-conversion session would skip the new
    // kind's prompt.
    if !memory_crossed_boundary {
        for agent in AgentKind::ALL {
            if memory.sessions.get(&agent) == base.sessions.get(&agent)
                && file.sessions.get(&agent) != base.sessions.get(&agent)
            {
                match file.sessions.get(&agent) {
                    Some(sid) => memory.sessions.insert(agent, sid.clone()),
                    None => memory.sessions.remove(&agent),
                };
            }
        }
    }

    // A file-side crossing of the orchestrator boundary clears sessions
    // atomically with the kind (`set_kind`). When that kind change won the
    // merge, its session clear must win too — otherwise a concurrent
    // memory-side session write survives and resumes the pre-conversion
    // conversation on the next launch.
    let file_crossed_boundary = crossed_orchestrator_boundary(file, base);
    // Skip when memory crossed too: its own set_kind already cleared the
    // pre-conversion sessions, and anything it recorded since (e.g. the new
    // orchestrator's session) is newer than the file's empty map.
    if file_crossed_boundary && !memory_crossed_boundary && memory.kind == file.kind {
        memory.sessions = file.sessions.clone();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageKind {
    #[default]
    Info,
    Warning,
    Error,
}

pub struct App {
    pub projects: Vec<Project>,
    pub focused_project: ProjectId,
    pub focused_swimlane: usize,
    pub sidebar: Option<SidebarState>,
    pub input_mode: InputMode,
    pub search_query: String,
    pub confirm_message: Option<String>,
    pub pending_confirm: Option<ConfirmAction>,
    pub dialog: Option<DialogState>,
    pub prune_dialog: Option<PruneDialogState>,
    pub should_quit: bool,
    pub message: Option<(String, MessageKind)>,
    pub message_set_at: Option<Instant>,
    pub busy_count: usize,
    /// When the spinner first became visible. Used to hold it on screen for
    /// at least `BUSY_MIN_VISIBLE` after `busy_count` drops back to zero so
    /// quick actions don't blink in and out.
    pub busy_visible_since: Option<Instant>,
    pub spinner_tick: usize,
    /// Issue IDs with a session launch currently in flight. Guards against
    /// double-launch races from repeated keypresses.
    pub launches_in_flight: HashSet<String>,
    /// Issue IDs whose in-flight launch was invalidated (session killed
    /// mid-detection). The landing result's session id and setup flag are
    /// discarded — the pane the detectors were watching is gone.
    pub launches_invalidated: HashSet<String>,
    pub linear_picker: Option<LinearPickerState>,
    pub linear_picker_context: LinearPickerContext,
    pub picker_tab: ImportSource,
    pub link_picker: Option<LinkPickerState>,
    pub debug_inspector_json: Option<String>,
    pub debug_inspector_scroll: usize,
    pub update_available: bool,
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
            search_query: String::new(),
            confirm_message: None,
            pending_confirm: None,
            dialog: None,
            prune_dialog: None,
            should_quit: false,
            message: None,
            message_set_at: None,
            busy_count: 0,
            busy_visible_since: None,
            spinner_tick: 0,
            launches_in_flight: HashSet::new(),
            launches_invalidated: HashSet::new(),
            linear_picker: None,
            linear_picker_context: LinearPickerContext::Import,
            picker_tab: ImportSource::Linear,
            link_picker: None,
            debug_inspector_json: None,
            debug_inspector_scroll: 0,
            update_available: false,
        }
    }

    pub fn add_background_project(&mut self, config: AppConfig, state: AppState) {
        self.projects.push(Project::new(config, state));
    }

    pub fn set_available_agents(
        &mut self,
        available_agents: Vec<AgentKind>,
        default_agent: Option<AgentKind>,
    ) {
        for project in &mut self.projects {
            project.set_available_agents(available_agents.clone(), default_agent);
        }
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

    pub fn known_project_roots(&self) -> HashSet<ProjectId> {
        self.projects.iter().map(|p| p.id()).collect()
    }

    pub fn apply_reload_result(&mut self, result: crate::global_config::ReloadResult) {
        for (config, state) in result.new_projects {
            self.add_background_project(config, state);
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

    #[allow(dead_code)] // Future sidebar/swimlane reordering
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

    #[allow(dead_code)] // Core navigation accessor; actions currently go through context_project_mut
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
            n if n >= 3 => CardSize::Medium,
            _ => CardSize::Full,
        }
    }

    pub fn show_message(&mut self, msg: impl Into<String>, kind: MessageKind) {
        self.message = Some((msg.into(), kind));
        self.message_set_at = Some(Instant::now());
    }

    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.show_message(msg, MessageKind::Info);
    }

    pub fn set_warning(&mut self, msg: impl Into<String>) {
        self.show_message(msg, MessageKind::Warning);
    }

    #[allow(dead_code)] // Convenience wrapper; used in tests and future error handling
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.show_message(msg, MessageKind::Error);
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

    /// Increment the busy counter and start (or extend) the spinner's
    /// minimum-visible window. Use this instead of mutating `busy_count`
    /// directly so quick actions still show the spinner long enough to
    /// register visually.
    pub fn begin_busy(&mut self) {
        if self.busy_count == 0 && self.busy_visible_since.is_none() {
            self.busy_visible_since = Some(Instant::now());
        }
        self.busy_count += 1;
    }

    /// Mark an issue's in-flight launch as invalidated (its session was
    /// killed mid-detection), so the landing handler drops the result's
    /// session id and setup flag. Guarded on the launch actually being in
    /// flight — an unconditional insert would leave a stale entry that
    /// poisons the issue's next launch.
    pub fn invalidate_inflight_launch(&mut self, issue_id: &str) {
        if self.launches_in_flight.contains(issue_id) {
            self.launches_invalidated.insert(issue_id.to_string());
        }
    }

    /// Whether the spinner should currently be drawn. True while any
    /// background action is in flight, and for at least `BUSY_MIN_VISIBLE`
    /// after the last one finishes.
    pub fn is_busy_visible(&self) -> bool {
        if self.busy_count > 0 {
            return true;
        }
        match self.busy_visible_since {
            Some(started) => started.elapsed() < BUSY_MIN_VISIBLE,
            None => false,
        }
    }

    /// Clear the busy-visible window if its minimum has elapsed and no
    /// new work is in flight. Returns `true` if state changed (and the UI
    /// should redraw to hide the spinner).
    pub fn tick_busy_visibility(&mut self) -> bool {
        if self.busy_count == 0 {
            if let Some(started) = self.busy_visible_since {
                if started.elapsed() >= BUSY_MIN_VISIBLE {
                    self.busy_visible_since = None;
                    return true;
                }
            }
        }
        false
    }

    /// Dot-matrix "Pillar Sweep" spinner: a 3-wide block of filled dots
    /// sweeps left-to-right across 5 cells, then trails off. Each entry
    /// is `true` for a filled dot, `false` for an empty dot. Renderers
    /// draw filled and empty dots in different styles for the two-tone
    /// dot-matrix look.
    pub fn spinner_frame(&self) -> [bool; 5] {
        const FRAMES: [[bool; 5]; 8] = [
            [true, false, false, false, false],
            [true, true, false, false, false],
            [true, true, true, false, false],
            [false, true, true, true, false],
            [false, false, true, true, true],
            [false, false, false, true, true],
            [false, false, false, false, true],
            [false, false, false, false, false],
        ];
        // Tick runs at 50ms; advance one frame every 2 ticks (~10 fps).
        FRAMES[(self.spinner_tick / 2) % FRAMES.len()]
    }

    pub fn open_dialog(&mut self, ctx: &ActionContext) {
        self.open_dialog_in_column(Column::Todo, ctx);
    }

    pub fn open_dialog_in_column(&mut self, column: Column, ctx: &ActionContext) {
        let p = self.context_project(ctx);
        let github_available = p.has_github_prs();
        let mut state = DialogState::new(
            p.dialog_default_agent(),
            p.config.agent_mode,
            p.available_agents.clone(),
            p.linear_available,
            github_available,
        );
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
            p.available_agents.clone(),
            p.linear_available,
            github_available,
            live,
        ));
        self.input_mode = InputMode::Dialog;
    }

    pub fn close_dialog(&mut self) {
        self.dialog = None;
        self.input_mode = InputMode::Normal;
    }

    pub fn open_prune_dialog(&mut self, ctx: &ActionContext) {
        let candidates = crate::prune::scan_candidates(self.context_project(ctx));
        if candidates.is_empty() {
            self.set_message("No worktrees to prune");
            return;
        }
        self.prune_dialog = Some(PruneDialogState::new(ctx.project_id.clone(), candidates));
        self.input_mode = InputMode::PruneDialog;
    }

    pub fn close_prune_dialog(&mut self) {
        self.prune_dialog = None;
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
                self.set_warning("No issues loaded yet");
            } else {
                self.set_warning("No import sources available");
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
        let Some(picker) = &self.linear_picker else {
            return Vec::new();
        };
        self.active_project().filtered_linear_issues(picker)
    }

    pub fn filtered_github_prs(&self) -> Vec<&PrStatus> {
        let Some(picker) = &self.linear_picker else {
            return Vec::new();
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
        self.search_query.push(c);
        let query = self.search_query.clone();
        let p = self.context_project_mut(ctx);
        p.clamp_all_rows(&query);
        p.focus_first_match(&query);
    }

    pub fn search_delete_char(&mut self, ctx: &ActionContext) {
        if self.search_query.is_empty() {
            self.cancel_search(ctx);
            return;
        }
        self.search_query.pop();
        let query = self.search_query.clone();
        let p = self.context_project_mut(ctx);
        p.clamp_all_rows(&query);
        p.focus_first_match(&query);
    }

    pub fn confirm_search(&mut self) {
        self.input_mode = InputMode::Normal;
    }

    pub fn cancel_search(&mut self, _ctx: &ActionContext) {
        self.search_query.clear();
        for project in &mut self.projects {
            project.clamp_all_rows("");
        }
        self.input_mode = InputMode::Normal;
    }

    pub fn clear_search(&mut self, ctx: &ActionContext) {
        if !self.active_project().marked_issues.is_empty() {
            self.context_project_mut(ctx).clear_marks();
            return;
        }
        // Esc peels off one active board filter at a time.
        if self.active_project().link_filter.is_some() {
            self.clear_link_filter(ctx);
            return;
        }
        if !self.search_query.is_empty() {
            self.search_query.clear();
            for project in &mut self.projects {
                project.clamp_all_rows("");
            }
        }
    }

    pub fn has_active_search(&self) -> bool {
        !self.search_query.is_empty()
    }

    /// Toggle the board filter to the selected issue's connected link component.
    /// Pressing it again on a filtered board clears the filter.
    pub fn toggle_link_filter(&mut self, ctx: &ActionContext) {
        let query = self.search_query.clone();
        if self.context_project(ctx).link_filter.is_some() {
            self.clear_link_filter(ctx);
            return;
        }
        let anchor = match self.context_project(ctx).selected_issue(&query) {
            Some(issue) if issue.has_links() => issue.id.clone(),
            Some(_) => {
                self.set_warning("Issue has no links to filter by");
                return;
            }
            None => return,
        };
        let project = self.context_project_mut(ctx);
        project.link_filter = Some(anchor);
        project.clamp_all_rows(&query);
    }

    pub fn clear_link_filter(&mut self, ctx: &ActionContext) {
        let query = self.search_query.clone();
        let project = self.context_project_mut(ctx);
        project.link_filter = None;
        project.clamp_all_rows(&query);
    }

    pub fn open_link_picker(&mut self, ctx: &ActionContext) {
        let query = self.search_query.clone();
        let Some(issue) = self.context_project(ctx).selected_issue(&query) else {
            return;
        };
        let anchor_id = issue.id.clone();
        self.link_picker = Some(LinkPickerState {
            anchor_id,
            search: String::new(),
            selected: 0,
        });
        self.input_mode = InputMode::LinkPicker;
    }

    pub fn close_link_picker(&mut self) {
        self.link_picker = None;
        self.input_mode = InputMode::Normal;
    }

    /// Candidate issues for the link picker: every other issue in the anchor's
    /// project, filtered by the picker search (matches id or title).
    pub fn link_picker_candidates(&self) -> Vec<(String, String, bool)> {
        let Some(picker) = &self.link_picker else {
            return Vec::new();
        };
        let project = self.active_project();
        let anchor = project
            .issues
            .iter()
            .find(|i| i.id.eq_ignore_ascii_case(&picker.anchor_id));
        let query = picker.search.to_lowercase();
        project
            .issues
            .iter()
            .filter(|i| !i.id.eq_ignore_ascii_case(&picker.anchor_id))
            .filter(|i| {
                query.is_empty()
                    || i.id.to_lowercase().contains(&query)
                    || i.title.to_lowercase().contains(&query)
            })
            .map(|i| {
                let linked = anchor.is_some_and(|a| a.is_linked_to(&i.id));
                (i.id.clone(), i.title.clone(), linked)
            })
            .collect()
    }

    pub fn link_picker_move_down(&mut self) {
        let count = self.link_picker_candidates().len();
        if let Some(picker) = &mut self.link_picker {
            if picker.selected + 1 < count {
                picker.selected += 1;
            }
        }
    }

    pub fn link_picker_move_up(&mut self) {
        if let Some(picker) = &mut self.link_picker {
            picker.selected = picker.selected.saturating_sub(1);
        }
    }

    pub fn link_picker_push_char(&mut self, c: char) {
        if let Some(picker) = &mut self.link_picker {
            picker.search.push(c);
            picker.selected = 0;
        }
    }

    pub fn link_picker_delete_char(&mut self) {
        if let Some(picker) = &mut self.link_picker {
            picker.search.pop();
            picker.selected = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::DEFAULT_DONE_SESSION_TTL;
    use crate::types::{AgentKind, IssueKind, PrState, PrStatus};

    fn test_config() -> AppConfig {
        AppConfig {
            project_name: "bork".into(),
            project_root: PathBuf::from("/tmp/test-bork"),
            agent_kind: AgentKind::OpenCode,
            agent_mode: crate::types::AgentMode::Plan,
            default_prompt: None,
            review_prompt: None,
            orchestrator_prompt: None,
            setup_script: None,
            teardown_script: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
            auto_import_reviews: true,
            auto_import_authored_prs: true,
            agents_allowlist: None,
            prune_threshold: crate::config::DEFAULT_PRUNE_THRESHOLD,
            auto_prune_check_interval: crate::config::DEFAULT_AUTO_PRUNE_CHECK_INTERVAL,
            agent_launch: std::collections::HashMap::new(),
        }
    }

    fn test_issue(id: &str, column: Column) -> Issue {
        Issue::new(
            id,
            format!("Test issue {}", id),
            column,
            AgentKind::OpenCode,
        )
    }

    fn test_issue_titled(id: &str, title: &str, column: Column) -> Issue {
        let mut issue = test_issue(id, column);
        issue.title = title.to_string();
        issue
    }

    fn test_app(issues: Vec<Issue>) -> App {
        let state = AppState {
            issues,
            last_prune_at: None,
        };
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
            is_cross_repository: false,
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
    fn has_listening_ports_true_when_issue_session_has_ports() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        let session = app.project().issues[0].session_name(&app.project().config.project_name);
        app.project_mut()
            .live
            .listening_ports
            .insert(session, vec![3000]);
        assert!(app.project().has_listening_ports());
    }

    #[test]
    fn has_listening_ports_false_when_no_ports() {
        let app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        assert!(!app.project().has_listening_ports());
    }

    #[test]
    fn has_listening_ports_false_for_empty_port_list() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        let session = app.project().issues[0].session_name(&app.project().config.project_name);
        app.project_mut()
            .live
            .listening_ports
            .insert(session, vec![]);
        assert!(!app.project().has_listening_ports());
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
    fn test_auto_assign_skips_orchestrator_issues() {
        let mut issue = test_issue("bork-8", Column::InProgress);
        issue.kind = IssueKind::Orchestrator;
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(!changed);
        assert!(app.project().issues[0].worktree.is_none());
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
    fn test_auto_assign_clears_pruned_at() {
        let mut issue = test_issue("bork-8", Column::InProgress);
        issue.pruned_at = Some(1_700_000_000);
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-8".into(), "bork-8/init-cli".into());
        let changed = app.project_mut().auto_assign_worktrees();
        assert!(changed);
        assert_eq!(app.project().issues[0].worktree, Some("bork-8".into()));
        assert!(
            app.project().issues[0].pruned_at.is_none(),
            "pruned_at should clear when a new worktree gets attached"
        );
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
        assert!(app.project().issues[0].worktree.is_none());
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
        app.project_mut().live.pr_poll_done = true;

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert_eq!(app.project().issues[0].title, "PR #1");
        assert_eq!(app.project().issues[0].column, Column::CodeReview);
        assert!(app.project().issues[0].has_pr_number(1));
    }

    #[test]
    fn sync_prs_skips_authored_when_disabled() {
        let mut app = test_app(vec![]);
        app.project_mut().config.auto_import_authored_prs = false;
        app.project_mut().live.user_prs = vec![test_pr(1, "feature/new")];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_reviews_when_disabled() {
        let mut app = test_app(vec![]);
        app.project_mut().config.auto_import_reviews = false;
        app.project_mut().live.review_requested_prs = vec![test_pr(7, "someones/branch")];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_imports_reviews_when_enabled() {
        let mut app = test_app(vec![]);
        app.project_mut().live.review_requested_prs = vec![test_pr(7, "someones/branch")];
        app.project_mut().live.pr_poll_done = true;

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert_eq!(
            app.project().issues[0].primary_pr_import_source(),
            Some(PrImportSource::ReviewRequested)
        );
    }

    #[test]
    fn sync_prs_imports_fork_review_pr() {
        let mut app = test_app(vec![]);
        let mut pr = test_pr(106, "linear-api-fallback");
        pr.is_cross_repository = true;
        app.project_mut().live.review_requested_prs = vec![pr];
        app.project_mut().live.pr_poll_done = true;

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert!(app.project().issues[0].has_pr_number(106));
        assert_eq!(
            app.project().issues[0].primary_pr_import_source(),
            Some(PrImportSource::ReviewRequested)
        );
    }

    #[test]
    fn sync_prs_fork_pr_skips_branch_checks() {
        // Fork branches named like an issue id (or "main") live in another
        // repo's namespace, so branch-based dedup must not apply.
        let existing = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![existing]);
        let mut prefix_pr = test_pr(50, "bork-1/some-fork-branch");
        prefix_pr.is_cross_repository = true;
        let mut main_pr = test_pr(51, "main");
        main_pr.is_cross_repository = true;
        app.project_mut().live.review_requested_prs = vec![prefix_pr, main_pr];
        app.project_mut().live.pr_poll_done = true;

        assert!(app.project_mut().sync_prs_as_issues().0);
        let issues = &app.project().issues;
        assert!(issues.iter().any(|i| i.has_pr_number(50)));
        assert!(issues.iter().any(|i| i.has_pr_number(51)));
    }

    #[test]
    fn sync_prs_fork_pr_dedups_by_number() {
        let mut existing = test_issue("bork-1", Column::CodeReview);
        existing.github_pr_links = vec![LinkedGithubPr {
            number: 106,
            imported: true,
            import_source: Some(PrImportSource::ReviewRequested),
        }];
        let mut app = test_app(vec![existing]);
        let mut pr = test_pr(106, "linear-api-fallback");
        pr.is_cross_repository = true;
        app.project_mut().live.review_requested_prs = vec![pr];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_disabled_reviews_still_complete_existing() {
        // Throwaway-repo case: auto-import off, but a previously imported
        // review issue should still move to Done once the review clears.
        let mut existing = test_issue("bork-1", Column::CodeReview);
        existing.github_pr_links = vec![LinkedGithubPr {
            number: 7,
            imported: true,
            import_source: Some(PrImportSource::ReviewRequested),
        }];
        let mut app = test_app(vec![existing]);
        app.project_mut().config.auto_import_reviews = false;
        // PR #7 no longer in the live review set.
        app.project_mut().live.review_requested_prs = vec![];
        app.project_mut().live.pr_poll_done = true;

        let (changed, _) = app.project_mut().sync_prs_as_issues();
        assert!(changed);
        assert_eq!(app.project().issues[0].column, Column::Done);
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
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_closed_prs() {
        let mut app = test_app(vec![]);
        let mut pr = test_pr(1, "feature/new");
        pr.state = PrState::Closed;
        app.project_mut().live.user_prs = vec![pr];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_merged_prs() {
        let mut app = test_app(vec![]);
        let mut pr = test_pr(1, "feature/new");
        pr.state = PrState::Merged;
        app.project_mut().live.user_prs = vec![pr];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert!(app.project().issues.is_empty());
    }

    #[test]
    fn sync_prs_skips_main_branch() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![test_pr(1, "main")];
        app.project_mut().live.pr_poll_done = true;

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
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_dedup_by_issue_id_prefix() {
        let issue = test_issue("bork-5", Column::InProgress);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(1, "bork-5/follow-up")];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_dedup_by_pr_number() {
        let mut issue = test_issue("bork-1", Column::CodeReview);
        issue.github_pr_links.push(crate::types::LinkedGithubPr {
            number: 42,
            imported: false,
            import_source: None,
        });
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(42, "some/branch")];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_reimports_after_delete() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![test_pr(42, "feature/new")];
        app.project_mut().live.pr_poll_done = true;

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert!(app.project().issues[0].has_pr_number(42));

        app.project_mut().issues.clear();

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
        assert!(app.project().issues[0].has_pr_number(42));
    }

    #[test]
    fn sync_prs_multiple_prs_get_unique_ids() {
        let mut app = test_app(vec![]);
        app.project_mut().live.user_prs = vec![
            test_pr(1, "feature/a"),
            test_pr(2, "feature/b"),
            test_pr(3, "feature/c"),
        ];
        app.project_mut().live.pr_poll_done = true;

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
        app.project_mut().live.pr_poll_done = true;

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
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    #[test]
    fn sync_prs_prefix_no_false_match_on_similar_ids() {
        // Issue "bork-1" should NOT match branch "bork-10/something"
        let issue = test_issue("bork-1", Column::InProgress);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(1, "bork-10/something")];
        app.project_mut().live.pr_poll_done = true;

        assert!(app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 2); // new issue created
    }

    #[test]
    fn sync_prs_prefix_match_is_case_insensitive() {
        let issue = test_issue("BORK-5", Column::InProgress);
        let mut app = test_app(vec![issue]);
        app.project_mut().live.user_prs = vec![test_pr(1, "bork-5/fix")];
        app.project_mut().live.pr_poll_done = true;

        assert!(!app.project_mut().sync_prs_as_issues().0);
        assert_eq!(app.project().issues.len(), 1);
    }

    // ================================================================
    // DialogState: mode cycling (field 3 = mode)
    // ================================================================

    fn claude_dialog() -> DialogState {
        DialogState::new(
            crate::types::AgentKind::Claude,
            crate::types::AgentMode::Plan,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
        )
    }

    fn opencode_dialog() -> DialogState {
        DialogState::new(
            crate::types::AgentKind::OpenCode,
            crate::types::AgentMode::Plan,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
        )
    }

    fn codex_dialog() -> DialogState {
        DialogState::new(
            crate::types::AgentKind::Codex,
            crate::types::AgentMode::Plan,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
        )
    }

    #[test]
    fn dialog_claude_mode_cycles_plan_build_yolo() {
        let mut d = claude_dialog();
        assert_eq!(d.agent_mode, crate::types::AgentMode::Plan);
        d.focused_field = 2; // Mode field (Kind, Agent, Mode)
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
        d.focused_field = 2; // Mode field (Kind, Agent, Mode)
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Plan);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
    }

    #[test]
    fn dialog_codex_mode_cycles_plan_build_yolo() {
        let mut d = codex_dialog();
        d.focused_field = 2; // Mode field (Kind, Agent, Mode)
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
        d.push_char(' ');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Plan);
    }

    #[test]
    fn dialog_mode_toggle_with_h_and_l_keys() {
        let mut d = claude_dialog();
        d.focused_field = 2; // Mode field (Kind, Agent, Mode)
        d.push_char('l');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
        d.push_char('h');
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
    }

    #[test]
    fn dialog_pi_hides_mode_field() {
        let d = DialogState::new(
            crate::types::AgentKind::Pi,
            crate::types::AgentMode::Plan,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
        );
        assert!(!d.ordered_fields().contains(&DialogField::Mode));
        // Pi pins its single mode to Build.
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
    }

    #[test]
    fn dialog_cycling_to_pi_resets_mode_to_build() {
        // Start on Claude in Yolo, then cycle agents until we land on Pi.
        let mut d = claude_dialog();
        d.agent_mode = crate::types::AgentMode::Yolo;
        d.focused_field = 1; // Agent field

        for _ in 0..crate::types::AgentKind::ALL.len() {
            if d.agent_kind == crate::types::AgentKind::Pi {
                break;
            }
            d.push_char('l');
        }
        assert_eq!(d.agent_kind, crate::types::AgentKind::Pi);
        assert_eq!(d.agent_mode, crate::types::AgentMode::Build);
    }

    #[test]
    fn dialog_new_uses_config_agent_kind() {
        let config = test_config();
        let d = DialogState::new(
            config.agent_kind,
            config.agent_mode,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
        );
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
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
            &LiveState::default(),
        );
        assert_eq!(d.agent_kind, crate::types::AgentKind::Claude);
        assert_eq!(d.agent_mode, crate::types::AgentMode::Yolo);
    }

    #[test]
    fn dialog_new_defaults_to_agentic_with_title_focused() {
        let d = DialogState::new(
            crate::types::AgentKind::OpenCode,
            crate::types::AgentMode::Plan,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
        );
        assert_eq!(d.kind, IssueKind::Agentic);
        // Agentic, no linear: Kind(0), Agent(1), Mode(2), Title(3)
        assert_eq!(d.focused_field, 3);
    }

    #[test]
    fn dialog_kind_space_cycles_three_kinds() {
        let mut d = opencode_dialog();
        d.focused_field = 0; // Kind field
        d.push_char(' ');
        assert_eq!(d.kind, IssueKind::Orchestrator);
        d.push_char(' ');
        assert_eq!(d.kind, IssueKind::NonAgentic);
        d.push_char(' ');
        assert_eq!(d.kind, IssueKind::Agentic);
    }

    #[test]
    fn dialog_kind_h_l_move_and_clamp() {
        let mut d = opencode_dialog();
        d.focused_field = 0; // Kind field
        d.push_char('l');
        assert_eq!(d.kind, IssueKind::Orchestrator);
        d.push_char('l');
        assert_eq!(d.kind, IssueKind::NonAgentic);
        d.push_char('l');
        assert_eq!(d.kind, IssueKind::NonAgentic);
        d.push_char('h');
        assert_eq!(d.kind, IssueKind::Orchestrator);
        d.push_char('h');
        assert_eq!(d.kind, IssueKind::Agentic);
        d.push_char('h');
        assert_eq!(d.kind, IssueKind::Agentic);
    }

    #[test]
    fn dialog_orchestrator_hides_github_pr_field_keeps_agent() {
        let mut d = DialogState::new(
            crate::types::AgentKind::OpenCode,
            crate::types::AgentMode::Plan,
            crate::types::AgentKind::ALL.to_vec(),
            true,
            true,
        );
        d.kind = IssueKind::Orchestrator;
        assert_eq!(
            d.ordered_fields(),
            vec![
                DialogField::Kind,
                DialogField::Linear,
                DialogField::Agent,
                DialogField::Mode,
                DialogField::Title,
                DialogField::Prompt,
            ]
        );
    }

    #[test]
    fn dialog_from_issue_resolves_fork_review_pr() {
        let mut issue = test_issue("bork-1", Column::CodeReview);
        issue.github_pr_links = vec![LinkedGithubPr {
            number: 106,
            imported: true,
            import_source: Some(PrImportSource::ReviewRequested),
        }];
        let mut pr = test_pr(106, "linear-api-fallback");
        pr.is_cross_repository = true;
        let live = LiveState {
            review_requested_prs: vec![pr],
            ..Default::default()
        };
        let d = DialogState::from_issue(
            &issue,
            0,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            true,
            &live,
        );
        assert_eq!(d.github_prs.len(), 1);
        assert_eq!(d.github_prs[0].number, 106);
    }

    #[test]
    fn dialog_from_orchestrator_issue_focuses_title() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.kind = IssueKind::Orchestrator;
        let d = DialogState::from_issue(
            &issue,
            0,
            crate::types::AgentKind::ALL.to_vec(),
            true,
            true,
            &LiveState::default(),
        );
        assert_eq!(d.ordered_fields()[d.focused_field], DialogField::Title);
    }

    #[test]
    fn dialog_prompt_supports_normal_edit_commands() {
        let mut d = DialogState::new(
            crate::types::AgentKind::OpenCode,
            crate::types::AgentMode::Plan,
            crate::types::AgentKind::ALL.to_vec(),
            false,
            false,
        );
        // Agentic, no linear: Kind(0), Agent(1), Mode(2), Title(3), Prompt(4)
        d.focused_field = 4;

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
        app.project_mut().move_issue_right("");
        assert_eq!(app.project().issues[0].column, Column::InProgress);
    }

    #[test]
    fn move_issue_right_from_done_stays_in_done() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Done)]);
        app.project_mut().selected_column = 3;
        app.project_mut().move_issue_right("");
        assert_eq!(app.project().issues[0].column, Column::Done);
    }

    #[test]
    fn move_issue_left_from_in_progress_goes_to_todo() {
        let mut app = test_app(vec![test_issue("bork-1", Column::InProgress)]);
        app.project_mut().selected_column = 1;
        app.project_mut().move_issue_left("");
        assert_eq!(app.project().issues[0].column, Column::Todo);
    }

    #[test]
    fn move_issue_to_done_sets_done_at() {
        let mut app = test_app(vec![test_issue("bork-1", Column::CodeReview)]);
        app.project_mut().selected_column = 2;
        app.project_mut().move_issue_right("");
        assert_eq!(app.project().issues[0].column, Column::Done);
        assert!(app.project().issues[0].done_at.is_some());
    }

    #[test]
    fn move_issue_out_of_done_clears_done_at() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = Some(1700000000);
        let mut app = test_app(vec![issue]);
        app.project_mut().selected_column = 3;
        app.project_mut().move_issue_left("");
        assert_eq!(app.project().issues[0].column, Column::CodeReview);
        assert_eq!(app.project().issues[0].done_at, None);
    }

    #[test]
    fn move_issue_within_non_done_columns_keeps_done_at_none() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        app.project_mut().selected_column = 0;
        app.project_mut().move_issue_right(""); // Todo -> InProgress
        assert_eq!(app.project().issues[0].done_at, None);
        app.project_mut().selected_column = 1;
        app.project_mut().move_issue_right(""); // InProgress -> CodeReview
        assert_eq!(app.project().issues[0].done_at, None);
    }

    #[test]
    fn backfill_done_at_on_startup() {
        // Legacy issues in Done without done_at should get backfilled on App::new
        let mut issue = test_issue("bork-1", Column::Done);
        issue.done_at = None;
        let state = AppState {
            last_prune_at: None,
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
            last_prune_at: None,
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
            last_prune_at: None,
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
        app.project_mut().move_issue_right(""); // -> Done
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

    #[test]
    fn branch_for_skips_fork_pr_branches() {
        let mut issue = test_issue("bork-1", Column::CodeReview);
        issue.github_pr_links = vec![LinkedGithubPr {
            number: 106,
            imported: true,
            import_source: Some(PrImportSource::ReviewRequested),
        }];
        let mut app = test_app(vec![issue]);
        let mut pr = test_pr(106, "linear-api-fallback");
        pr.is_cross_repository = true;
        app.project_mut().live.review_requested_prs = vec![pr];

        let branch = app.project().branch_for(&app.project().issues[0].clone());
        assert_eq!(branch, None, "Fork PR branch does not exist locally");
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
        let todo = app.project().issues_in_column(Column::Todo, "");
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
        let q = app.search_query.clone();
        let results = app.project().issues_in_column(Column::Todo, &q);
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
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);

        app.search_query = "FIX LOGIN".to_string();
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);
    }

    #[test]
    fn search_matches_issue_id() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login bug", Column::Todo),
            test_issue_titled("bork-2", "Add dark mode", Column::Todo),
        ]);
        app.search_query = "bork-2".to_string();
        let q = app.search_query.clone();
        let results = app.project().issues_in_column(Column::Todo, &q);
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
        let q = app.search_query.clone();
        let results = app.project().issues_in_column(Column::Todo, &q);
        assert_eq!(results.len(), 2, "bork-1 and bork-12 both contain 'bork-1'");
    }

    #[test]
    fn search_empty_query_returns_all() {
        let mut app = test_app(vec![
            test_issue("bork-1", Column::Todo),
            test_issue("bork-2", Column::Todo),
        ]);
        app.search_query = String::new();
        assert_eq!(app.project().issues_in_column(Column::Todo, "").len(), 2);
    }

    #[test]
    fn search_no_matches_returns_empty() {
        let mut app = test_app(vec![test_issue_titled(
            "bork-1",
            "Fix login bug",
            Column::Todo,
        )]);
        app.search_query = "zzzzz".to_string();
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 0);
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
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);
        assert_eq!(
            app.project().issues_in_column(Column::InProgress, &q).len(),
            1
        );
        assert_eq!(
            app.project().issues_in_column(Column::CodeReview, &q).len(),
            0
        );
        assert_eq!(app.project().issues_in_column(Column::Done, &q).len(), 1);
    }

    #[test]
    fn search_preserves_global_indices() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Add feature", Column::Todo),
            test_issue_titled("bork-2", "Fix bug", Column::Todo),
            test_issue_titled("bork-3", "Fix crash", Column::Todo),
        ]);
        app.search_query = "fix".to_string();
        let q = app.search_query.clone();
        let results = app.project().issues_in_column(Column::Todo, &q);
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
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
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
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.cancel_search(&ctx);
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
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);

        let ctx = app.action_context();
        app.clear_search(&ctx);
        assert!(app.search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo, "").len(), 2);
    }

    #[test]
    fn clear_search_noop_when_no_filter() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        let ctx = app.action_context();
        app.clear_search(&ctx);
        assert!(app.search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo, "").len(), 1);
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
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        assert_eq!(app.search_query, "f");
        app.search_push_char('i', &ctx);
        assert_eq!(app.search_query, "fi");
        app.search_push_char('x', &ctx);
        assert_eq!(app.search_query, "fix");
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
        assert_eq!(app.search_query, "fi");
    }

    #[test]
    fn search_backspace_on_empty_cancels_search() {
        let mut app = test_app(vec![]);
        let ctx = app.action_context();
        app.start_search();
        assert_eq!(app.input_mode, InputMode::Search);

        app.search_delete_char(&ctx);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.search_query.is_empty());
    }

    #[test]
    fn search_backspace_on_single_char_stays_in_search() {
        let mut app = test_app(vec![test_issue_titled("bork-1", "Fix bug", Column::Todo)]);
        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_delete_char(&ctx);

        assert_eq!(app.input_mode, InputMode::Search);
        assert!(app.search_query.is_empty());
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
        let q = app.search_query.clone();
        let count = app.project().issues_in_column(Column::Todo, &q).len();
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

        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 0);
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
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);
        assert_eq!(
            app.project().issues_in_column(Column::InProgress, &q).len(),
            0
        );

        // Confirm — filter stays, back to normal
        app.confirm_search();
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.search_query, "fix");
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);

        // Clear — all issues visible again
        app.clear_search(&ctx);
        assert!(app.search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo, "").len(), 1);
        assert_eq!(
            app.project().issues_in_column(Column::InProgress, "").len(),
            1
        );
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
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);

        // Cancel — clears query, all issues back
        app.cancel_search(&ctx);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.search_query.is_empty());
        assert_eq!(app.project().issues_in_column(Column::Todo, "").len(), 2);
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
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 2);

        // Re-enter: query still "fix", refine to "fix log"
        app.start_search();
        assert_eq!(app.search_query, "fix");
        app.search_push_char(' ', &ctx);
        app.search_push_char('l', &ctx);
        app.search_push_char('o', &ctx);
        app.search_push_char('g', &ctx);
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 2);

        // Refine further to "fix login"
        app.search_push_char('i', &ctx);
        app.search_push_char('n', &ctx);
        let q = app.search_query.clone();
        assert_eq!(app.project().issues_in_column(Column::Todo, &q).len(), 1);
        assert_eq!(
            app.project().issues_in_column(Column::Todo, &q)[0].1.id,
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

        app.search_query = "fix".to_string();
        let q = app.search_query.clone();
        app.project_mut().clamp_all_rows(&q);
        app.project_mut().selected_row[0] = 0;

        let q = app.search_query.clone();
        let issue = app
            .project()
            .selected_issue(&q)
            .expect("should have selected issue");
        assert_eq!(issue.id, "bork-2", "first filtered result should be bork-2");

        app.project_mut().selected_row[0] = 1;
        let q = app.search_query.clone();
        let issue = app
            .project()
            .selected_issue(&q)
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

        app.search_query = "fix".to_string();
        let q = app.search_query.clone();
        app.project_mut().clamp_all_rows(&q);
        app.project_mut().selected_row[0] = 0;

        let q = app.search_query.clone();
        let idx = app
            .project()
            .selected_issue_index(&q)
            .expect("should have index");
        assert_eq!(idx, 1, "global index of 'Fix bug' is 1, not 0");
    }

    // ================================================================
    // Search: expanded field matching (linear, branch, PR title)
    // ================================================================

    #[test]
    fn search_matches_linear_identifier() {
        let mut issue = test_issue_titled("bork-1", "Some feature", Column::Todo);
        issue.linear_links.push(crate::types::LinkedLinear {
            id: "uuid".into(),
            identifier: "VIL-123".into(),
            url: "https://linear.app/issue/VIL-123".into(),
            imported: false,
        });
        let app = test_app(vec![issue]);
        let results = app.project().issues_in_column(Column::Todo, "vil-123");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.id, "bork-1");
    }

    #[test]
    fn search_matches_partial_linear_identifier() {
        let mut issue = test_issue_titled("bork-1", "Some feature", Column::Todo);
        issue.linear_links.push(crate::types::LinkedLinear {
            id: "uuid".into(),
            identifier: "VIL-123".into(),
            url: "https://linear.app/issue/VIL-123".into(),
            imported: false,
        });
        let app = test_app(vec![issue]);
        let results = app.project().issues_in_column(Column::Todo, "vil");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_linear_identifier_case_insensitive() {
        let mut issue = test_issue_titled("bork-1", "Some feature", Column::Todo);
        issue.linear_links.push(crate::types::LinkedLinear {
            id: "uuid".into(),
            identifier: "VIL-123".into(),
            url: "https://linear.app/issue/VIL-123".into(),
            imported: false,
        });
        let app = test_app(vec![issue]);
        assert_eq!(app.project().issues_in_column(Column::Todo, "VIL").len(), 1);
        assert_eq!(app.project().issues_in_column(Column::Todo, "vil").len(), 1);
    }

    #[test]
    fn search_skips_none_linear_identifier() {
        let issue = test_issue_titled("bork-1", "Some feature", Column::Todo);
        assert!(issue.linear_links.is_empty());
        let app = test_app(vec![issue]);
        let results = app.project().issues_in_column(Column::Todo, "vil");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn search_matches_branch_name() {
        let mut issue = test_issue_titled("bork-5", "Dark mode", Column::Todo);
        issue.worktree = Some("bork-5-dark-mode".to_string());
        let mut app = test_app(vec![issue]);
        app.project_mut().live.worktree_branches.insert(
            "bork-5-dark-mode".to_string(),
            "bork-5/dark-mode".to_string(),
        );
        let results = app.project().issues_in_column(Column::Todo, "dark-mode");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.id, "bork-5");
    }

    #[test]
    fn search_matches_partial_branch_name() {
        let mut issue = test_issue_titled("bork-5", "Add feature", Column::Todo);
        issue.worktree = Some("bork-5-feat".to_string());
        let mut app = test_app(vec![issue]);
        app.project_mut().live.worktree_branches.insert(
            "bork-5-feat".to_string(),
            "bork-5/feature-dark-mode".to_string(),
        );
        let results = app.project().issues_in_column(Column::Todo, "feat");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_matches_pr_title() {
        let mut issue = test_issue_titled("bork-1", "Some task", Column::InProgress);
        issue.worktree = Some("bork-1-task".to_string());
        let mut app = test_app(vec![issue]);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1-task".to_string(), "bork-1/task".to_string());
        app.project_mut()
            .live
            .pr_statuses
            .insert("bork-1/task".to_string(), test_pr(42, "bork-1/task"));
        // PR title from test_pr is "PR #42", search for it
        let results = app.project().issues_in_column(Column::InProgress, "PR #42");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_matches_partial_pr_title() {
        let mut issue = test_issue_titled("bork-1", "Some task", Column::InProgress);
        issue.worktree = Some("bork-1-wt".to_string());
        issue.github_pr_links.push(crate::types::LinkedGithubPr {
            number: 7,
            imported: false,
            import_source: None,
        });
        let mut app = test_app(vec![issue]);
        let custom_pr = PrStatus {
            number: 7,
            title: "Refactor auth module".into(),
            url: "https://github.com/test/repo/pull/7".into(),
            author: "testuser".into(),
            state: PrState::Open,
            is_draft: false,
            checks: None,
            review: None,
            additions: 0,
            deletions: 0,
            head_branch: "bork-1/task".into(),
            is_cross_repository: false,
        };
        app.project_mut()
            .live
            .pr_statuses
            .insert("bork-1/task".to_string(), custom_pr);
        app.project_mut()
            .live
            .worktree_branches
            .insert("bork-1-wt".to_string(), "bork-1/task".to_string());
        let results = app.project().issues_in_column(Column::InProgress, "refact");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_matches_across_any_field() {
        let mut issue = test_issue_titled("bork-1", "Add feature", Column::Todo);
        issue.linear_links.push(crate::types::LinkedLinear {
            id: "uuid".into(),
            identifier: "VIL-99".into(),
            url: "https://a".into(),
            imported: false,
        });
        let app = test_app(vec![issue]);
        let results = app.project().issues_in_column(Column::Todo, "vil");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_no_duplicate_when_multiple_fields_match() {
        let mut issue = test_issue_titled("bork-fix", "Fix something", Column::Todo);
        issue.linear_links.push(crate::types::LinkedLinear {
            id: "uuid".into(),
            identifier: "FIX-1".into(),
            url: "https://a".into(),
            imported: false,
        });
        let app = test_app(vec![issue]);
        // "fix" matches both title, id, and linear_identifier
        let results = app.project().issues_in_column(Column::Todo, "fix");
        assert_eq!(results.len(), 1, "issue should appear exactly once");
    }

    // ================================================================
    // Search: global search across swimlanes
    // ================================================================

    #[test]
    fn search_filters_all_visible_swimlanes() {
        let mut app = App::new(
            test_config_named("alpha"),
            AppState {
                last_prune_at: None,
                issues: vec![
                    test_issue_titled("alpha-1", "Fix login", Column::Todo),
                    test_issue_titled("alpha-2", "Add feature", Column::Todo),
                ],
            },
        );
        app.add_background_project(
            test_config_named("beta"),
            AppState {
                last_prune_at: None,
                issues: vec![
                    test_issue_titled("beta-1", "Fix crash", Column::Todo),
                    test_issue_titled("beta-2", "Dark mode", Column::Todo),
                ],
            },
        );
        app.enable_sidebar();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone(), beta_id.clone()];

        app.search_query = "fix".to_string();

        let alpha = app.find_project(&alpha_id).unwrap();
        assert_eq!(
            alpha
                .issues_in_column(Column::Todo, &app.search_query)
                .len(),
            1
        );

        let beta = app.find_project(&beta_id).unwrap();
        assert_eq!(
            beta.issues_in_column(Column::Todo, &app.search_query).len(),
            1
        );
    }

    #[test]
    fn search_query_lives_on_app_not_project() {
        let mut app = test_multi_app();
        app.search_query = "test".to_string();

        let alpha = &app.projects[0];
        let beta = &app.projects[1];

        // Both projects use the same query from App
        let alpha_results = alpha.issues_in_column(Column::Todo, &app.search_query);
        let beta_results = beta.issues_in_column(Column::Todo, &app.search_query);

        // test_multi_app creates issues with titles like "Test issue alpha-1"
        // so "test" should match them
        assert!(!alpha_results.is_empty() || !beta_results.is_empty());
    }

    #[test]
    fn search_push_char_affects_all_swimlanes() {
        let mut app = App::new(
            test_config_named("alpha"),
            AppState {
                last_prune_at: None,
                issues: vec![
                    test_issue_titled("alpha-1", "Fix login", Column::Todo),
                    test_issue_titled("alpha-2", "Add feature", Column::Todo),
                ],
            },
        );
        app.add_background_project(
            test_config_named("beta"),
            AppState {
                last_prune_at: None,
                issues: vec![
                    test_issue_titled("beta-1", "Fix crash", Column::Todo),
                    test_issue_titled("beta-2", "Dark mode", Column::Todo),
                ],
            },
        );
        app.enable_sidebar();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone(), beta_id.clone()];

        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.search_push_char('x', &ctx);

        let q = &app.search_query;
        let alpha = app.find_project(&alpha_id).unwrap();
        assert_eq!(alpha.issues_in_column(Column::Todo, q).len(), 1);

        let beta = app.find_project(&beta_id).unwrap();
        assert_eq!(beta.issues_in_column(Column::Todo, q).len(), 1);
    }

    #[test]
    fn search_cancel_clears_across_swimlanes() {
        let mut app = App::new(
            test_config_named("alpha"),
            AppState {
                last_prune_at: None,
                issues: vec![
                    test_issue_titled("alpha-1", "Fix login", Column::Todo),
                    test_issue_titled("alpha-2", "Add feature", Column::Todo),
                ],
            },
        );
        app.add_background_project(
            test_config_named("beta"),
            AppState {
                last_prune_at: None,
                issues: vec![test_issue_titled("beta-1", "Fix crash", Column::Todo)],
            },
        );
        app.enable_sidebar();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone(), beta_id.clone()];

        let ctx = app.action_context();
        app.start_search();
        app.search_push_char('f', &ctx);
        app.search_push_char('i', &ctx);
        app.search_push_char('x', &ctx);
        app.cancel_search(&ctx);

        assert!(app.search_query.is_empty());
        let alpha = app.find_project(&alpha_id).unwrap();
        assert_eq!(alpha.issues_in_column(Column::Todo, "").len(), 2);
        let beta = app.find_project(&beta_id).unwrap();
        assert_eq!(beta.issues_in_column(Column::Todo, "").len(), 1);
    }

    #[test]
    fn search_clear_clears_across_swimlanes() {
        let mut app = App::new(
            test_config_named("alpha"),
            AppState {
                last_prune_at: None,
                issues: vec![
                    test_issue_titled("alpha-1", "Fix login", Column::Todo),
                    test_issue_titled("alpha-2", "Add feature", Column::Todo),
                ],
            },
        );
        app.add_background_project(
            test_config_named("beta"),
            AppState {
                last_prune_at: None,
                issues: vec![test_issue_titled("beta-1", "Fix crash", Column::Todo)],
            },
        );
        app.enable_sidebar();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone(), beta_id.clone()];

        app.search_query = "fix".to_string();
        let ctx = app.action_context();
        app.clear_search(&ctx);

        assert!(app.search_query.is_empty());
        let alpha = app.find_project(&alpha_id).unwrap();
        assert_eq!(alpha.issues_in_column(Column::Todo, "").len(), 2);
        let beta = app.find_project(&beta_id).unwrap();
        assert_eq!(beta.issues_in_column(Column::Todo, "").len(), 1);
    }

    // ================================================================
    // Search: navigation with active search query
    // ================================================================

    #[test]
    fn move_selection_down_respects_search_filter() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Add feature", Column::Todo),
            test_issue_titled("bork-3", "Fix crash", Column::Todo),
        ]);
        app.project_mut().selected_column = 0;
        app.project_mut().selected_row[0] = 0;
        app.search_query = "fix".to_string();
        let q = app.search_query.clone();
        app.project_mut().clamp_all_rows(&q);

        // Only 2 "fix" results, move down once
        let q = app.search_query.clone();
        app.project_mut().move_selection_down(&q);
        assert_eq!(app.project().selected_row[0], 1);

        // Moving down again should not go past the filtered count
        let q = app.search_query.clone();
        app.project_mut().move_selection_down(&q);
        assert_eq!(app.project().selected_row[0], 1);
    }

    #[test]
    fn scroll_to_bottom_respects_search_filter() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Add feature", Column::Todo),
            test_issue_titled("bork-3", "Fix crash", Column::Todo),
        ]);
        app.project_mut().selected_column = 0;
        app.search_query = "fix".to_string();
        let q = app.search_query.clone();
        app.project_mut().scroll_to_bottom(&q);
        assert_eq!(
            app.project().selected_row[0],
            1,
            "should scroll to last filtered item (index 1 of 2 fix results)"
        );
    }

    #[test]
    fn focus_right_with_search_skips_empty_columns() {
        let mut app = test_app(vec![
            test_issue_titled("bork-1", "Fix login", Column::Todo),
            test_issue_titled("bork-2", "Add feature", Column::InProgress),
            test_issue_titled("bork-3", "Fix crash", Column::Done),
        ]);
        app.project_mut().selected_column = 0;
        app.project_mut().selected_row[0] = 0;
        app.search_query = "fix".to_string();
        let q = app.search_query.clone();
        app.project_mut().clamp_all_rows(&q);

        // focus_right from the only "fix" in Todo should skip InProgress (no fix matches)
        // and land in Done
        let q = app.search_query.clone();
        app.project_mut().focus_right(&q);
        assert_eq!(
            app.project().selected_column,
            3,
            "should skip InProgress (no fix match) and land in Done"
        );
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
        issue.linear_links.push(crate::types::LinkedLinear {
            id: "uuid-1".into(),
            identifier: "TEST-1".into(),
            url: "https://a".into(),
            imported: true,
        });
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
            agent_mode: crate::types::AgentMode::Plan,
            default_prompt: None,
            review_prompt: None,
            orchestrator_prompt: None,
            setup_script: None,
            teardown_script: None,
            done_session_ttl: DEFAULT_DONE_SESSION_TTL,
            debug: false,
            auto_import_reviews: true,
            auto_import_authored_prs: true,
            agents_allowlist: None,
            prune_threshold: crate::config::DEFAULT_PRUNE_THRESHOLD,
            auto_prune_check_interval: crate::config::DEFAULT_AUTO_PRUNE_CHECK_INTERVAL,
            agent_launch: std::collections::HashMap::new(),
        }
    }

    fn test_multi_app() -> App {
        let mut app = App::new(
            test_config_named("alpha"),
            AppState {
                last_prune_at: None,
                issues: vec![test_issue("alpha-1", Column::Todo)],
            },
        );
        app.add_background_project(
            test_config_named("beta"),
            AppState {
                last_prune_at: None,
                issues: vec![
                    test_issue("beta-1", Column::Todo),
                    test_issue("beta-2", Column::InProgress),
                ],
            },
        );
        app.add_background_project(
            test_config_named("gamma"),
            AppState {
                last_prune_at: None,
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
        assert_eq!(app.card_size(), CardSize::Full);

        app.sidebar.as_mut().unwrap().swimlanes = vec![
            app.projects[0].id(),
            app.projects[1].id(),
            app.projects[2].id(),
        ];
        assert_eq!(app.card_size(), CardSize::Medium);
    }

    #[test]
    fn add_background_project() {
        let mut app = test_app(vec![test_issue("a-1", Column::Todo)]);
        assert_eq!(app.projects.len(), 1);
        app.add_background_project(test_config_named("other"), AppState::default());
        assert_eq!(app.projects.len(), 2);
        assert_eq!(app.projects[1].config.project_name, "other");
    }

    #[test]
    fn enable_sidebar_needs_two_projects() {
        let mut app = test_app(vec![]);
        app.enable_sidebar();
        assert!(app.sidebar.is_none());

        app.add_background_project(test_config_named("b"), AppState::default());
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

        app.add_background_project(test_config_named("delta"), AppState::default());

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
    fn search_is_global_on_app() {
        let mut app = test_multi_app();
        let alpha_id = app.projects[0].id();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![alpha_id.clone(), beta_id.clone()];

        app.focused_swimlane = 0;
        let ctx = app.action_context();
        app.search_push_char('x', &ctx);
        assert_eq!(app.search_query, "x");

        app.focused_swimlane = 1;
        let ctx = app.action_context();
        app.search_push_char('y', &ctx);
        assert_eq!(app.search_query, "xy");
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

    // --- apply_reload_result tests ---

    #[test]
    fn apply_reload_adds_new_projects() {
        let mut app = test_app(vec![]);
        assert_eq!(app.projects.len(), 1);
        assert!(app.sidebar.is_none());

        let result = crate::global_config::ReloadResult {
            new_projects: vec![(test_config_named("beta"), AppState::default())],
        };
        app.apply_reload_result(result);

        assert_eq!(app.projects.len(), 2);
        assert_eq!(app.projects[1].config.project_name, "beta");
        assert!(app.sidebar.is_some());
    }

    #[test]
    fn apply_reload_empty_is_noop() {
        let mut app = test_app(vec![]);
        let result = crate::global_config::ReloadResult {
            new_projects: vec![],
        };
        app.apply_reload_result(result);

        assert_eq!(app.projects.len(), 1);
        assert!(app.sidebar.is_none());
    }

    #[test]
    fn apply_reload_preserves_existing_sidebar_state() {
        let mut app = test_multi_app();
        let beta_id = app.projects[1].id();
        app.sidebar.as_mut().unwrap().swimlanes = vec![app.projects[0].id(), beta_id.clone()];
        app.sidebar.as_mut().unwrap().selected = 1;

        let result = crate::global_config::ReloadResult {
            new_projects: vec![],
        };
        app.apply_reload_result(result);

        let sidebar = app.sidebar.as_ref().unwrap();
        assert_eq!(sidebar.swimlanes.len(), 2);
        assert!(sidebar.swimlanes.contains(&beta_id));
        assert_eq!(sidebar.selected, 1);
    }

    #[test]
    fn apply_reload_enables_sidebar_on_second_project() {
        let mut app = test_app(vec![]);
        assert!(app.sidebar.is_none());

        let result = crate::global_config::ReloadResult {
            new_projects: vec![
                (test_config_named("beta"), AppState::default()),
                (test_config_named("gamma"), AppState::default()),
            ],
        };
        app.apply_reload_result(result);

        assert_eq!(app.projects.len(), 3);
        assert!(app.sidebar.is_some());
    }

    #[test]
    fn apply_reload_multiple_batches_accumulate() {
        let mut app = test_app(vec![]);

        let result1 = crate::global_config::ReloadResult {
            new_projects: vec![(test_config_named("beta"), AppState::default())],
        };
        app.apply_reload_result(result1);
        assert_eq!(app.projects.len(), 2);

        let result2 = crate::global_config::ReloadResult {
            new_projects: vec![(test_config_named("gamma"), AppState::default())],
        };
        app.apply_reload_result(result2);
        assert_eq!(app.projects.len(), 3);
        assert_eq!(app.projects[2].config.project_name, "gamma");
    }

    // ---------------------------------------------------------------
    // merge_issue_fields (3-way per-field merge)
    // ---------------------------------------------------------------

    #[test]
    fn merge_field_file_changed_memory_unchanged() {
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        let mut file = base.clone();
        file.title = "Updated externally".to_string();

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.title, "Updated externally");
    }

    #[test]
    fn merge_field_memory_changed_file_unchanged() {
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        memory.title = "Updated locally".to_string();
        let file = base.clone();

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.title, "Updated locally");
    }

    #[test]
    fn merge_field_both_changed_memory_wins() {
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        memory.title = "Local edit".to_string();
        let mut file = base.clone();
        file.title = "External edit".to_string();

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.title, "Local edit");
    }

    #[test]
    fn merge_field_neither_changed() {
        let base = test_issue_titled("a", "Same", Column::Todo);
        let mut memory = base.clone();
        let file = base.clone();

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.title, "Same");
    }

    #[test]
    fn merge_sessions_entrywise_keeps_both_sides() {
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        memory
            .sessions
            .insert(AgentKind::Claude, "uuid-local".to_string());
        let mut file = base.clone();
        file.sessions
            .insert(AgentKind::OpenCode, "ses_external".to_string());

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(
            memory.sessions.get(&AgentKind::Claude).map(String::as_str),
            Some("uuid-local"),
            "memory-side entry survives"
        );
        assert_eq!(
            memory
                .sessions
                .get(&AgentKind::OpenCode)
                .map(String::as_str),
            Some("ses_external"),
            "file-side entry merges in"
        );
    }

    #[test]
    fn merge_memory_side_conversion_rejects_file_session_insert() {
        // TUI converts to orchestrator (clears sessions) while an external
        // `bork issue start` writes a brand-new session entry to disk. The
        // in-memory clear is part of the conversion and must win.
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        let _ = memory.set_kind(IssueKind::Orchestrator);
        let mut file = base.clone();
        file.sessions
            .insert(AgentKind::OpenCode, "ses_external".to_string());

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.kind, IssueKind::Orchestrator);
        assert!(
            memory.sessions.is_empty(),
            "conversion's clear must not adopt concurrent file-side sessions"
        );
    }

    #[test]
    fn merge_pruned_at_and_setup_ran_adopt_file_changes() {
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        let mut file = base.clone();
        file.pruned_at = Some(1_700_000_000);
        file.setup_ran = true;

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.pruned_at, Some(1_700_000_000));
        assert!(memory.setup_ran);
    }

    #[test]
    fn merge_orchestrator_conversion_clears_memory_sessions() {
        // File side converted to orchestrator (set_kind cleared sessions);
        // a concurrent memory-side session write must not survive the merge.
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        memory
            .sessions
            .insert(AgentKind::OpenCode, "ses_stale".to_string());
        let mut file = base.clone();
        file.kind = IssueKind::Orchestrator;

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.kind, IssueKind::Orchestrator);
        assert!(
            memory.sessions.is_empty(),
            "conversion's session clear wins over the concurrent write"
        );
    }

    #[test]
    fn merge_double_crossing_keeps_memory_post_conversion_session() {
        // Both sides converted to orchestrator; memory then launched it and
        // recorded the new orchestrator's session. The file's empty map is
        // older than that write and must not wipe it.
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        let _ = memory.set_kind(IssueKind::Orchestrator);
        memory
            .sessions
            .insert(AgentKind::OpenCode, "ses_orch".to_string());
        let mut file = base.clone();
        let _ = file.set_kind(IssueKind::Orchestrator);

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.kind, IssueKind::Orchestrator);
        assert_eq!(
            memory
                .sessions
                .get(&AgentKind::OpenCode)
                .map(String::as_str),
            Some("ses_orch"),
            "memory's post-conversion session survives a concurrent file-side conversion"
        );
    }

    #[test]
    fn merge_fields_independent_per_field() {
        let base = test_issue_titled("a", "Original", Column::Todo);
        let mut memory = base.clone();
        memory.title = "Local title".to_string();
        // memory.column stays Todo (unchanged from base)

        let mut file = base.clone();
        file.column = Column::InProgress;
        // file.title stays "Original" (unchanged from base)

        merge_issue_fields(&mut memory, &base, &file);
        assert_eq!(memory.title, "Local title", "memory wins on title");
        assert_eq!(memory.column, Column::InProgress, "file wins on column");
    }

    // ---------------------------------------------------------------
    // merge_external_state (full Project-level merge)
    // ---------------------------------------------------------------

    fn test_project(issues: Vec<Issue>) -> Project {
        let state = AppState {
            last_prune_at: None,
            issues: issues.clone(),
        };
        let mut project = Project::new(test_config(), state);
        // base_issues is set by new(), but last_state_mtime will be None
        // since /tmp/test-bork/.bork/state.json doesn't exist. That's fine for tests.
        project.base_issues = issues;
        project
    }

    #[test]
    fn merge_takes_later_last_prune_at() {
        let mut project = test_project(vec![]);
        project.last_prune_at = Some(100);

        // External writer recorded a later prune.
        let file_state = AppState {
            last_prune_at: Some(200),
            issues: vec![],
        };
        project.merge_external_state(file_state);
        assert_eq!(project.last_prune_at, Some(200));

        // A file without the field never regresses the in-memory value.
        project.merge_external_state(AppState::default());
        assert_eq!(project.last_prune_at, Some(200));
    }

    #[test]
    fn merge_clean_state_replaces_entirely() {
        let mut project = test_project(vec![test_issue("a", Column::Todo)]);
        assert!(!project.state_dirty);

        let file_state = AppState {
            last_prune_at: None,
            issues: vec![test_issue("b", Column::InProgress)],
        };
        project.merge_external_state(file_state);

        assert_eq!(project.issues.len(), 1);
        assert_eq!(project.issues[0].id, "b");
        assert_eq!(project.issues[0].column, Column::InProgress);
    }

    #[test]
    fn merge_dirty_adds_new_external_issue() {
        let mut project = test_project(vec![test_issue("a", Column::Todo)]);
        project.mark_dirty();

        let file_state = AppState {
            last_prune_at: None,
            issues: vec![
                test_issue("a", Column::Todo),
                test_issue("b", Column::InProgress),
            ],
        };
        project.merge_external_state(file_state);

        assert_eq!(project.issues.len(), 2);
        let ids: Vec<&str> = project.issues.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
    }

    #[test]
    fn merge_dirty_removes_externally_deleted_issue() {
        let mut project = test_project(vec![
            test_issue("a", Column::Todo),
            test_issue("b", Column::InProgress),
        ]);
        project.mark_dirty();

        let file_state = AppState {
            last_prune_at: None,
            issues: vec![test_issue("a", Column::Todo)],
        };
        project.merge_external_state(file_state);

        assert_eq!(project.issues.len(), 1);
        assert_eq!(project.issues[0].id, "a");
    }

    #[test]
    fn merge_dirty_field_level_merge_for_existing() {
        let original = test_issue_titled("a", "Original", Column::Todo);
        let mut project = test_project(vec![original.clone()]);

        // Local change: move to InProgress
        project.issues[0].column = Column::InProgress;
        project.mark_dirty();

        // External change: update title (but column still Todo in file, matching base)
        let mut file_issue = original;
        file_issue.title = "Renamed externally".to_string();
        let file_state = AppState {
            last_prune_at: None,
            issues: vec![file_issue],
        };
        project.merge_external_state(file_state);

        assert_eq!(
            project.issues[0].title, "Renamed externally",
            "file wins on title"
        );
        assert_eq!(
            project.issues[0].column,
            Column::InProgress,
            "memory wins on column"
        );
    }

    #[test]
    fn merge_dirty_locally_added_issue_kept() {
        let mut project = test_project(vec![test_issue("a", Column::Todo)]);

        // Locally add a new issue (not in base)
        project.issues.push(test_issue("local-new", Column::Todo));
        project.mark_dirty();

        // External file still only has "a"
        let file_state = AppState {
            last_prune_at: None,
            issues: vec![test_issue("a", Column::Todo)],
        };
        project.merge_external_state(file_state);

        // "local-new" is not in file_ids, so it gets removed by retain.
        // This is correct: if an external process is the source of truth for
        // what issues exist, a locally-added issue that isn't in the file
        // should be removed. The dirty flush would have persisted it first
        // in the normal flow (flush happens after merge).
        assert_eq!(project.issues.len(), 1);
        assert_eq!(project.issues[0].id, "a");
    }

    #[test]
    fn merge_clamps_selection_after_removal() {
        let mut project = test_project(vec![
            test_issue("a", Column::Todo),
            test_issue("b", Column::Todo),
            test_issue("c", Column::Todo),
        ]);
        project.selected_row[0] = 2; // pointing at 3rd issue

        // External change removes 2 issues
        let file_state = AppState {
            last_prune_at: None,
            issues: vec![test_issue("a", Column::Todo)],
        };
        project.merge_external_state(file_state);

        assert_eq!(project.issues.len(), 1);
        assert_eq!(project.selected_row[0], 0, "row clamped to valid range");
    }

    #[test]
    fn merge_empty_file_removes_all() {
        let mut project = test_project(vec![
            test_issue("a", Column::Todo),
            test_issue("b", Column::InProgress),
        ]);

        let file_state = AppState::default();
        project.merge_external_state(file_state);

        assert!(project.issues.is_empty());
    }

    #[test]
    fn merge_empty_memory_adds_from_file() {
        let mut project = test_project(vec![]);

        let file_state = AppState {
            last_prune_at: None,
            issues: vec![test_issue("new", Column::Todo)],
        };
        project.merge_external_state(file_state);

        assert_eq!(project.issues.len(), 1);
        assert_eq!(project.issues[0].id, "new");
    }

    #[test]
    fn merge_identical_state_is_noop() {
        let issues = vec![
            test_issue("a", Column::Todo),
            test_issue("b", Column::InProgress),
        ];
        let mut project = test_project(issues.clone());

        let file_state = AppState {
            last_prune_at: None,
            issues: issues.clone(),
        };
        project.merge_external_state(file_state);

        assert_eq!(project.issues.len(), 2);
        assert_eq!(project.issues, issues);
    }

    #[test]
    fn merge_updates_base_issues() {
        let mut project = test_project(vec![test_issue("a", Column::Todo)]);

        let new_issues = vec![
            test_issue("a", Column::Todo),
            test_issue("b", Column::InProgress),
        ];
        let file_state = AppState {
            last_prune_at: None,
            issues: new_issues.clone(),
        };
        project.merge_external_state(file_state);

        assert_eq!(
            project.base_issues, new_issues,
            "base_issues updated to file contents"
        );
    }

    // ================================================================
    // Message severity
    // ================================================================

    #[test]
    fn set_message_stores_info_kind() {
        let mut app = test_app(vec![]);
        app.set_message("hello");
        let (msg, kind) = app.message.as_ref().unwrap();
        assert_eq!(msg, "hello");
        assert_eq!(*kind, MessageKind::Info);
        assert!(app.message_set_at.is_some());
    }

    #[test]
    fn set_warning_stores_warning_kind() {
        let mut app = test_app(vec![]);
        app.set_warning("careful");
        let (msg, kind) = app.message.as_ref().unwrap();
        assert_eq!(msg, "careful");
        assert_eq!(*kind, MessageKind::Warning);
    }

    #[test]
    fn set_error_stores_error_kind() {
        let mut app = test_app(vec![]);
        app.set_error("boom");
        let (msg, kind) = app.message.as_ref().unwrap();
        assert_eq!(msg, "boom");
        assert_eq!(*kind, MessageKind::Error);
    }

    #[test]
    fn show_message_accepts_kind() {
        let mut app = test_app(vec![]);
        app.show_message("test", MessageKind::Warning);
        let (msg, kind) = app.message.as_ref().unwrap();
        assert_eq!(msg, "test");
        assert_eq!(*kind, MessageKind::Warning);
    }

    #[test]
    fn message_overwrites_previous() {
        let mut app = test_app(vec![]);
        app.set_error("first");
        app.set_message("second");
        let (msg, kind) = app.message.as_ref().unwrap();
        assert_eq!(msg, "second");
        assert_eq!(*kind, MessageKind::Info);
    }

    #[test]
    fn clear_expired_message_before_timeout() {
        let mut app = test_app(vec![]);
        app.set_message("fresh");
        assert!(!app.clear_expired_message());
        assert!(app.message.is_some());
    }

    #[test]
    fn clear_expired_message_after_timeout() {
        let mut app = test_app(vec![]);
        app.set_message("old");
        app.message_set_at = Some(Instant::now() - std::time::Duration::from_secs(4));
        assert!(app.clear_expired_message());
        assert!(app.message.is_none());
        assert!(app.message_set_at.is_none());
    }

    #[test]
    fn clear_expired_message_noop_when_no_message() {
        let mut app = test_app(vec![]);
        assert!(!app.clear_expired_message());
    }

    // ================================================================
    // PruneDialogState
    // ================================================================

    use crate::prune::PruneCandidate;

    // Orphan + clean => seeded action is Remove.
    fn make_candidate(name: &str) -> PruneCandidate {
        PruneCandidate::new(
            name.to_string(),
            Some(crate::types::WorktreeStatus {
                staged: 0,
                unstaged: 0,
            }),
            None,
            false,
        )
    }

    // Dirty => seeded action is Keep.
    fn make_dirty_candidate(name: &str) -> PruneCandidate {
        PruneCandidate::new(
            name.to_string(),
            Some(crate::types::WorktreeStatus {
                staged: 0,
                unstaged: 1,
            }),
            None,
            false,
        )
    }

    fn make_prune_dialog(candidates: Vec<PruneCandidate>) -> PruneDialogState {
        PruneDialogState::new(std::path::PathBuf::from("/x"), candidates)
    }

    #[test]
    fn prune_dialog_new_seeds_actions_from_defaults() {
        let dialog = make_prune_dialog(vec![
            make_candidate("a"),
            make_dirty_candidate("b"),
            make_candidate("c"),
        ]);
        assert_eq!(dialog.candidates[0].action, PruneAction::Remove);
        assert_eq!(dialog.candidates[1].action, PruneAction::Keep);
        assert_eq!(dialog.candidates[2].action, PruneAction::Remove);
        assert_eq!(dialog.selected, 0);
        assert!(dialog.error.is_none());
    }

    #[test]
    fn prune_dialog_move_up_at_top_is_noop() {
        let mut dialog = make_prune_dialog(vec![make_candidate("a"), make_candidate("b")]);
        assert_eq!(dialog.selected, 0);
        dialog.move_up();
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn prune_dialog_move_down_stops_at_last_row() {
        let mut dialog = make_prune_dialog(vec![make_candidate("a"), make_candidate("b")]);
        dialog.move_down();
        dialog.move_down();
        dialog.move_down();
        assert_eq!(dialog.selected, 1);
    }

    #[test]
    fn prune_dialog_move_up_after_down_returns() {
        let mut dialog = make_prune_dialog(vec![make_candidate("a"), make_candidate("b")]);
        dialog.move_down();
        assert_eq!(dialog.selected, 1);
        dialog.move_up();
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn prune_dialog_toggle_current_flips_action_and_clears_error() {
        let mut dialog = make_prune_dialog(vec![make_candidate("a")]);
        dialog.error = Some("old".into());
        dialog.toggle_current();
        assert_eq!(dialog.candidates[0].action, PruneAction::Keep);
        assert!(dialog.error.is_none());
        dialog.toggle_current();
        assert_eq!(dialog.candidates[0].action, PruneAction::Remove);
    }

    #[test]
    fn prune_dialog_toggle_with_no_rows_is_safe() {
        let mut dialog = make_prune_dialog(vec![]);
        // selected = 0 but no rows; toggle must not panic.
        dialog.toggle_current();
        assert!(dialog.candidates.is_empty());
    }

    #[test]
    fn prune_dialog_select_all_remove_and_keep_clear_error() {
        let mut dialog = make_prune_dialog(vec![make_candidate("a"), make_dirty_candidate("b")]);
        dialog.error = Some("err".into());
        dialog.select_all_keep();
        assert!(dialog
            .candidates
            .iter()
            .all(|c| c.action == PruneAction::Keep));
        assert!(dialog.error.is_none());

        dialog.error = Some("err2".into());
        dialog.select_all_remove();
        assert!(dialog
            .candidates
            .iter()
            .all(|c| c.action == PruneAction::Remove));
        assert!(dialog.error.is_none());
    }

    #[test]
    fn prune_dialog_counts_track_selection() {
        let mut dialog = make_prune_dialog(vec![
            make_candidate("clean"),
            make_dirty_candidate("dirty"),
            make_candidate("also-clean"),
        ]);
        // Defaults: clean rows Remove, dirty row Keep.
        assert_eq!(dialog.remove_count(), 2);
        assert_eq!(dialog.dirty_remove_count(), 0);

        dialog.select_all_remove();
        assert_eq!(dialog.remove_count(), 3);
        assert_eq!(dialog.dirty_remove_count(), 1);

        dialog.select_all_keep();
        assert_eq!(dialog.remove_count(), 0);
        assert_eq!(dialog.dirty_remove_count(), 0);
    }

    // ================================================================
    // build_candidates (name discovery itself is disk-based and tested
    // in prune.rs; these cover the live-cache enrichment)
    // ================================================================

    #[test]
    fn build_candidates_without_poll_data_defaults_keep() {
        let app = test_app(vec![]);
        let candidates = crate::prune::build_candidates(app.project(), vec!["wt-1".into()]);
        assert_eq!(candidates.len(), 1);
        // Status unknown (poll hasn't reached it) => never pre-selected.
        assert!(candidates[0].status.is_none());
        assert_eq!(candidates[0].action, PruneAction::Keep);
    }

    #[test]
    fn build_candidates_reads_status_from_live_cache() {
        let mut app = test_app(vec![]);
        app.project_mut().live.worktree_statuses.insert(
            "wt-1".into(),
            WorktreeStatus {
                staged: 0,
                unstaged: 0,
            },
        );
        let candidates = crate::prune::build_candidates(app.project(), vec!["wt-1".into()]);
        // Known-clean orphan => pre-selected for removal.
        assert_eq!(candidates[0].action, PruneAction::Remove);
    }

    #[test]
    fn build_candidates_falls_back_to_frozen_status() {
        let mut app = test_app(vec![]);
        app.project_mut().live.frozen_worktree_statuses.insert(
            "wt-frozen".into(),
            WorktreeStatus {
                staged: 0,
                unstaged: 2,
            },
        );
        let candidates = crate::prune::build_candidates(app.project(), vec!["wt-frozen".into()]);
        assert!(candidates[0].is_dirty());
        assert_eq!(candidates[0].action, PruneAction::Keep);
    }

    #[test]
    fn build_candidates_links_to_matching_issue() {
        let mut issue = test_issue("bork-1", Column::Done);
        issue.worktree = Some("wt-1".into());
        let mut app = test_app(vec![issue]);
        app.project_mut().live.worktree_statuses.insert(
            "wt-1".into(),
            WorktreeStatus {
                staged: 0,
                unstaged: 0,
            },
        );
        let candidates = crate::prune::build_candidates(app.project(), vec!["wt-1".into()]);
        assert_eq!(candidates[0].issue_id.as_deref(), Some("bork-1"));
        assert_eq!(candidates[0].issue_column, Some(Column::Done));
        // Done + clean + no session => default Remove
        assert_eq!(candidates[0].action, PruneAction::Remove);
    }

    fn linked_issue(id: &str, links: &[&str]) -> Issue {
        let mut issue = test_issue(id, Column::Todo);
        issue.linked_issues = links.iter().map(|s| s.to_string()).collect();
        issue
    }

    #[test]
    fn toggle_link_filter_sets_and_clears_anchor() {
        let mut app = test_app(vec![
            linked_issue("bork-1", &["bork-2"]),
            linked_issue("bork-2", &["bork-1"]),
        ]);
        let ctx = app.action_context();

        app.toggle_link_filter(&ctx);
        assert_eq!(app.active_project().link_filter.as_deref(), Some("bork-1"));

        app.toggle_link_filter(&ctx);
        assert!(app.active_project().link_filter.is_none());
    }

    #[test]
    fn toggle_link_filter_warns_without_links() {
        let mut app = test_app(vec![test_issue("bork-1", Column::Todo)]);
        let ctx = app.action_context();

        app.toggle_link_filter(&ctx);
        assert!(app.active_project().link_filter.is_none());
        assert!(app.message.is_some());
    }

    #[test]
    fn link_filter_hides_unconnected_issues() {
        let mut app = test_app(vec![
            linked_issue("bork-1", &["bork-2"]),
            linked_issue("bork-2", &["bork-1"]),
            test_issue("bork-3", Column::Todo),
        ]);
        let ctx = app.action_context();
        app.toggle_link_filter(&ctx);

        let visible = app.active_project().issues_in_column(Column::Todo, "");
        let ids: Vec<&str> = visible.iter().map(|(_, i)| i.id.as_str()).collect();
        assert!(ids.contains(&"bork-1"));
        assert!(ids.contains(&"bork-2"));
        assert!(!ids.contains(&"bork-3"));
    }

    #[test]
    fn external_merge_clears_stale_link_filter() {
        let mut app = test_app(vec![
            linked_issue("bork-1", &["bork-2"]),
            linked_issue("bork-2", &["bork-1"]),
        ]);
        let ctx = app.action_context();
        app.toggle_link_filter(&ctx);
        assert!(app.active_project().link_filter.is_some());

        // Simulate the anchor being deleted externally (e.g. `bork issue delete`).
        let remaining = vec![linked_issue("bork-2", &[])];
        app.active_project_mut().merge_external_state(AppState {
            issues: remaining,
            ..Default::default()
        });

        assert!(app.active_project().link_filter.is_none());
    }

    #[test]
    fn external_merge_keeps_link_filter_when_anchor_survives() {
        let mut app = test_app(vec![
            linked_issue("bork-1", &["bork-2"]),
            linked_issue("bork-2", &["bork-1"]),
        ]);
        let ctx = app.action_context();
        app.toggle_link_filter(&ctx);

        let same = vec![
            linked_issue("bork-1", &["bork-2"]),
            linked_issue("bork-2", &["bork-1"]),
        ];
        app.active_project_mut().merge_external_state(AppState {
            issues: same,
            ..Default::default()
        });

        assert_eq!(app.active_project().link_filter.as_deref(), Some("bork-1"));
    }

    #[test]
    fn link_picker_candidates_exclude_anchor() {
        let mut app = test_app(vec![
            test_issue("bork-1", Column::Todo),
            test_issue("bork-2", Column::Todo),
        ]);
        let ctx = app.action_context();
        app.open_link_picker(&ctx);

        let candidates = app.link_picker_candidates();
        let ids: Vec<&str> = candidates.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["bork-2"]);
    }
}
