use std::collections::BTreeMap;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AgentKind {
    OpenCode,
    Claude,
    Codex,
    Pi,
}

impl AgentKind {
    pub const ALL: [AgentKind; 4] = [
        AgentKind::OpenCode,
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Pi,
    ];

    pub fn command(self) -> &'static str {
        match self {
            AgentKind::OpenCode => "opencode",
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Pi => "pi",
        }
    }

    /// Whether this agent has bork-managed plan/build/yolo modes. Pi has a
    /// single mode, so the dialog hides the mode picker for it.
    pub fn has_modes(self) -> bool {
        !matches!(self, AgentKind::Pi)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "opencode" | "open_code" | "open-code" => Some(AgentKind::OpenCode),
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "pi" => Some(AgentKind::Pi),
            _ => None,
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.command())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentMode {
    Plan,
    Build,
    /// Claude/Codex-only: launches with dangerous no-prompt mode
    Yolo,
}

impl AgentMode {
    /// Parse a mode name from config (case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "plan" => Some(AgentMode::Plan),
            "build" => Some(AgentMode::Build),
            "yolo" => Some(AgentMode::Yolo),
            _ => None,
        }
    }

    /// Cycles Plan → Build → Plan (for OpenCode, which has no yolo mode).
    pub fn toggle(self) -> Self {
        match self {
            AgentMode::Plan => AgentMode::Build,
            AgentMode::Build | AgentMode::Yolo => AgentMode::Plan,
        }
    }

    /// Cycles Plan → Build → Yolo → Plan (for Claude and Codex).
    pub fn next_for_yolo_agents(self) -> Self {
        match self {
            AgentMode::Plan => AgentMode::Build,
            AgentMode::Build => AgentMode::Yolo,
            AgentMode::Yolo => AgentMode::Plan,
        }
    }
}

impl fmt::Display for AgentMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentMode::Plan => write!(f, "plan"),
            AgentMode::Build => write!(f, "build"),
            AgentMode::Yolo => write!(f, "yolo"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrImportSource {
    Authored,
    ReviewRequested,
}

impl fmt::Display for PrImportSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrImportSource::Authored => write!(f, "authored"),
            PrImportSource::ReviewRequested => write!(f, "review"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueKind {
    #[default]
    Agentic,
    NonAgentic,
    /// Coordinates work across multiple bork issues: maintains a plan file,
    /// spawns issues via `bork issue start`, and monitors their agents.
    Orchestrator,
}

impl IssueKind {
    /// Whether this kind launches an agent session.
    pub fn is_agentic(self) -> bool {
        matches!(self, Self::Agentic | Self::Orchestrator)
    }
}

impl fmt::Display for IssueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agentic => write!(f, "Agentic"),
            Self::NonAgentic => write!(f, "Todo"),
            Self::Orchestrator => write!(f, "Orchestrator"),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusInfo {
    pub status: AgentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorktreeStatus {
    pub staged: usize,
    pub unstaged: usize,
}

impl WorktreeStatus {
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.unstaged == 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkedLinear {
    pub id: String,
    pub identifier: String,
    pub url: String,
    #[serde(default)]
    pub imported: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkedGithubPr {
    pub number: u32,
    #[serde(default)]
    pub imported: bool,
    #[serde(default)]
    pub import_source: Option<PrImportSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub kind: IssueKind,
    pub column: Column,
    pub agent_kind: AgentKind,
    pub agent_mode: AgentMode,
    pub prompt: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub done_at: Option<u64>,
    /// Agent session IDs keyed by the agent that created them. Switching
    /// agents keeps every agent's session resumable; only the entry for the
    /// current `agent_kind` is ever used to resume.
    #[serde(default)]
    pub sessions: BTreeMap<AgentKind, String>,
    /// Timestamp of the last time this issue's worktree was pruned.
    /// Cleared when a new worktree is assigned.
    #[serde(default)]
    pub pruned_at: Option<u64>,
    /// Whether the worktree setup script has run for this issue. Set when a
    /// launch command that included the setup prefix was sent — independent
    /// of session-id capture, which is best effort and can miss.
    #[serde(default)]
    pub setup_ran: bool,

    // --- New multi-link fields ---
    #[serde(default)]
    pub linear_links: Vec<LinkedLinear>,
    #[serde(default)]
    pub github_pr_links: Vec<LinkedGithubPr>,

    /// IDs of other issues in the same project this one is tied to.
    /// Links are symmetric: each side stores the other's id.
    #[serde(default)]
    pub linked_issues: Vec<String>,

    // --- Legacy singular fields (read-only, for migration from old state.json) ---
    #[serde(default, skip_serializing)]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub linear_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub linear_identifier: Option<String>,
    #[serde(default, skip_serializing)]
    pub linear_url: Option<String>,
    #[serde(default, skip_serializing)]
    pub linear_imported: bool,
    #[serde(default, skip_serializing)]
    pub pr_number: Option<u32>,
    #[serde(default, skip_serializing, alias = "github_imported")]
    pub pr_imported: bool,
    #[serde(default, skip_serializing)]
    pub pr_import_source: Option<PrImportSource>,
}

impl Issue {
    /// Baseline issue with all optional/legacy fields empty. Combine with
    /// struct update syntax for variations:
    /// `Issue { prompt: Some(p), ..Issue::new(id, title, column, agent) }`.
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        column: Column,
        agent_kind: AgentKind,
    ) -> Self {
        Issue {
            id: id.into(),
            title: title.into(),
            kind: IssueKind::Agentic,
            column,
            agent_kind,
            agent_mode: AgentMode::Plan,
            prompt: None,
            worktree: None,
            done_at: None,
            sessions: BTreeMap::new(),
            pruned_at: None,
            setup_ran: false,
            linear_links: Vec::new(),
            github_pr_links: Vec::new(),
            linked_issues: Vec::new(),
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
            pr_import_source: None,
        }
    }

    pub fn session_name(&self, project_name: &str) -> String {
        format!("{}-{}", project_name, self.id.to_lowercase())
    }

    /// Session ID for the currently selected agent, if that agent has run
    /// before. Sessions created by other agents stay in `sessions` untouched.
    pub fn current_session_id(&self) -> Option<&str> {
        self.sessions.get(&self.agent_kind).map(String::as_str)
    }

    /// Whether a landed launch result's detected session id still applies to
    /// this issue. A kind change while detection was polling means `set_kind`
    /// invalidated everything the launch produced; an agent change means the
    /// switch's kill may have landed mid-detection, making the id suspect.
    pub fn accepts_launch_result(
        &self,
        launched_kind: IssueKind,
        launched_agent: AgentKind,
    ) -> bool {
        self.kind == launched_kind && self.agent_kind == launched_agent
    }

    /// Attach a worktree to this issue. Clears the `pruned_at` marker so the
    /// "pruned" card indicator disappears, and `setup_ran` because the setup
    /// script is scoped to a worktree — a fresh checkout (re-attach after a
    /// prune or an orchestrator round-trip) needs it to run again. Every
    /// attach path must keep these fields in sync, so the invariant lives
    /// here.
    pub fn attach_worktree(&mut self, worktree: String) {
        self.worktree = Some(worktree);
        self.pruned_at = None;
        self.setup_ran = false;
    }

    /// Migrate legacy singular fields into the new Vec fields.
    /// Called once after deserialization from old state.json format.
    ///
    /// Returns the legacy session id, if any, for the caller to attribute
    /// and file into `sessions` — the legacy field could hold another
    /// agent's id (the pre-map mismatch this map fixes, bork-147), and
    /// telling the owners apart needs the agents' on-disk transcript stores
    /// (`opencode::LegacySessionStores`), which this module can't touch.
    #[must_use]
    pub fn migrate_legacy_fields(&mut self) -> Option<String> {
        let legacy_session = self.session_id.take();
        if legacy_session.is_some() || !self.sessions.is_empty() {
            // A recorded or legacy id proves a launch happened, so the
            // one-time worktree setup must not run again.
            self.setup_ran = true;
        }

        if self.linear_links.is_empty() {
            if let (Some(id), Some(identifier), Some(url)) = (
                self.linear_id.take(),
                self.linear_identifier.take(),
                self.linear_url.take(),
            ) {
                self.linear_links.push(LinkedLinear {
                    id,
                    identifier,
                    url,
                    imported: self.linear_imported,
                });
            }
        }
        self.linear_imported = false;

        if self.github_pr_links.is_empty() {
            if let Some(number) = self.pr_number.take() {
                self.github_pr_links.push(LinkedGithubPr {
                    number,
                    imported: self.pr_imported,
                    import_source: self.pr_import_source.take(),
                });
            }
        }
        self.pr_imported = false;

        legacy_session
    }

    /// Change the issue kind, clearing state the new kind invalidates.
    /// Crossing the orchestrator boundary drops all agent sessions (resuming
    /// any of them would skip the new kind's prompt); becoming an orchestrator
    /// also drops the worktree and PR links since orchestrators run at the
    /// project root and have no PR of their own.
    ///
    /// Returns `true` when the orchestrator boundary was crossed. Callers
    /// should then kill any live tmux session, since re-attaching it would
    /// silently resume the old agent with the previous kind's prompt.
    #[must_use]
    pub fn set_kind(&mut self, kind: IssueKind) -> bool {
        let resets_session = self.kind_change_resets_session(kind);
        self.kind = kind;
        if !resets_session {
            return false;
        }
        self.sessions.clear();
        if kind == IssueKind::Orchestrator {
            self.worktree = None;
            self.github_pr_links.clear();
        }
        true
    }

    /// Change the selected agent, keeping every agent's stored session
    /// resumable. Returns `true` when it changed; callers must then kill any
    /// live tmux session, which is still running the old agent's process.
    #[must_use]
    pub fn set_agent_kind(&mut self, kind: AgentKind) -> bool {
        let changed = kind != self.agent_kind;
        self.agent_kind = kind;
        changed
    }

    /// Whether changing to `kind` crosses the orchestrator boundary, i.e. the
    /// issue's live session must be killed before committing the change (a
    /// re-attached session would resume the old agent with the previous
    /// kind's prompt and cwd). Single owner of the rule `set_kind` and its
    /// callers act on.
    pub fn kind_change_resets_session(&self, kind: IssueKind) -> bool {
        (self.kind == IssueKind::Orchestrator) != (kind == IssueKind::Orchestrator)
    }

    pub fn has_linear(&self) -> bool {
        !self.linear_links.is_empty()
    }

    #[allow(dead_code)] // Symmetric with has_linear(); natural API for issue state checks
    pub fn has_pr(&self) -> bool {
        !self.github_pr_links.is_empty()
    }

    pub fn pr_numbers(&self) -> Vec<u32> {
        self.github_pr_links.iter().map(|l| l.number).collect()
    }

    pub fn linear_identifiers(&self) -> Vec<&str> {
        self.linear_links
            .iter()
            .map(|l| l.identifier.as_str())
            .collect()
    }

    pub fn is_any_linear_imported(&self) -> bool {
        self.linear_links.iter().any(|l| l.imported)
    }

    pub fn is_any_pr_imported(&self) -> bool {
        self.github_pr_links.iter().any(|l| l.imported)
    }

    pub fn primary_pr_number(&self) -> Option<u32> {
        self.github_pr_links.first().map(|l| l.number)
    }

    pub fn primary_pr_import_source(&self) -> Option<PrImportSource> {
        self.github_pr_links.first().and_then(|l| l.import_source)
    }

    pub fn has_pr_number(&self, number: u32) -> bool {
        self.github_pr_links.iter().any(|l| l.number == number)
    }

    #[allow(dead_code)] // Used for deduplication when importing Linear issues
    pub fn has_linear_id(&self, id: &str) -> bool {
        self.linear_links.iter().any(|l| l.id == id)
    }

    pub fn has_links(&self) -> bool {
        !self.linked_issues.is_empty()
    }

    pub fn is_linked_to(&self, id: &str) -> bool {
        self.linked_issues
            .iter()
            .any(|l| l.eq_ignore_ascii_case(id))
    }
}

// --- PR types (ephemeral, not persisted) ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub number: u32,
    pub title: String,
    pub url: String,
    pub author: String,
    pub state: PrState,
    pub is_draft: bool,
    pub checks: Option<ChecksStatus>,
    pub review: Option<ReviewDecision>,
    pub additions: u32,
    pub deletions: u32,
    pub head_branch: String,
    /// True when the head branch lives in a fork. Fork PRs are excluded from
    /// branch-keyed indexing since their branch names can collide with upstream.
    pub is_cross_repository: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubStack {
    pub number: u32,
    pub url: String,
    pub base_ref: String,
    pub open: bool,
    pub pull_requests: Vec<GithubStackPullRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubStackPullRequest {
    pub number: u32,
    pub state: PrState,
    pub is_draft: bool,
    pub head_branch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_issue(id: &str, column: Column) -> Issue {
        Issue::new(
            id,
            format!("Test issue {}", id),
            column,
            AgentKind::OpenCode,
        )
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
            title: "Fix bug".into(),
            url: "https://github.com/test/repo/pull/1".into(),
            author: "testuser".into(),
            state: PrState::Open,
            is_draft: false,
            checks: Some(ChecksStatus::Success),
            review: Some(ReviewDecision::Approved),
            additions: 10,
            deletions: 5,
            head_branch: "main".into(),
            is_cross_repository: false,
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

    // --- IssueKind ---

    #[test]
    fn issue_kind_is_agentic() {
        assert!(IssueKind::Agentic.is_agentic());
        assert!(IssueKind::Orchestrator.is_agentic());
        assert!(!IssueKind::NonAgentic.is_agentic());
    }

    #[test]
    fn issue_kind_display() {
        assert_eq!(IssueKind::Agentic.to_string(), "Agentic");
        assert_eq!(IssueKind::NonAgentic.to_string(), "Todo");
        assert_eq!(IssueKind::Orchestrator.to_string(), "Orchestrator");
    }

    #[test]
    fn issue_kind_orchestrator_serde_roundtrip() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.kind = IssueKind::Orchestrator;
        let json = serde_json::to_string(&issue).unwrap();
        let back: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, IssueKind::Orchestrator);
    }

    #[test]
    fn issue_without_kind_defaults_to_agentic() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.kind, IssueKind::Agentic);
    }

    fn issue_with_session_state(kind: IssueKind) -> Issue {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue.kind = kind;
        issue.worktree = Some("bork-1-fix-bug".into());
        issue.sessions.insert(issue.agent_kind, "ses_abc".into());
        issue.github_pr_links.push(LinkedGithubPr {
            number: 42,
            imported: false,
            import_source: None,
        });
        issue
    }

    #[test]
    fn set_kind_to_orchestrator_clears_worktree_session_and_prs() {
        let mut issue = issue_with_session_state(IssueKind::Agentic);
        assert!(issue.set_kind(IssueKind::Orchestrator));
        assert_eq!(issue.kind, IssueKind::Orchestrator);
        assert!(issue.worktree.is_none());
        assert!(issue.sessions.is_empty());
        assert!(issue.github_pr_links.is_empty());
    }

    #[test]
    fn set_kind_from_orchestrator_clears_session_only() {
        let mut issue = issue_with_session_state(IssueKind::Orchestrator);
        assert!(issue.set_kind(IssueKind::Agentic));
        assert!(issue.sessions.is_empty());
        assert_eq!(issue.worktree, Some("bork-1-fix-bug".into()));
        assert_eq!(issue.github_pr_links.len(), 1);
    }

    #[test]
    fn set_kind_between_agentic_and_todo_keeps_state() {
        let mut issue = issue_with_session_state(IssueKind::Agentic);
        assert!(!issue.set_kind(IssueKind::NonAgentic));
        assert_eq!(issue.worktree, Some("bork-1-fix-bug".into()));
        assert_eq!(issue.current_session_id(), Some("ses_abc"));
        assert_eq!(issue.github_pr_links.len(), 1);
    }

    #[test]
    fn set_agent_kind_reports_change_and_keeps_sessions() {
        let mut issue = issue_with_session_state(IssueKind::Agentic);
        let original_agent = issue.agent_kind;
        assert!(!issue.set_agent_kind(original_agent));
        assert!(issue.set_agent_kind(AgentKind::Codex));
        assert_eq!(issue.agent_kind, AgentKind::Codex);
        assert_eq!(
            issue.sessions.get(&original_agent).map(String::as_str),
            Some("ses_abc")
        );
    }

    #[test]
    fn attach_worktree_resets_setup_for_fresh_checkout() {
        // A re-attached worktree is a fresh checkout: the setup script must
        // run again even though a session was recorded in the old one.
        let mut issue = issue_with_session_state(IssueKind::Agentic);
        issue.setup_ran = true;
        issue.pruned_at = Some(123);
        issue.attach_worktree("bork-1-redo".into());
        assert!(!issue.setup_ran);
        assert!(issue.pruned_at.is_none());
        assert_eq!(issue.worktree.as_deref(), Some("bork-1-redo"));
    }

    #[test]
    fn set_kind_same_kind_is_noop() {
        let mut issue = issue_with_session_state(IssueKind::Orchestrator);
        assert!(!issue.set_kind(IssueKind::Orchestrator));
        assert_eq!(issue.worktree, Some("bork-1-fix-bug".into()));
        assert_eq!(issue.current_session_id(), Some("ses_abc"));
    }

    // --- Issue session_name ---

    #[test]
    fn session_name_uses_project_name_and_lowercases_id() {
        let issue = test_issue("BORK-3", Column::Todo);
        assert_eq!(issue.session_name("bork"), "bork-bork-3");
        assert_eq!(issue.session_name("myapp"), "myapp-bork-3");
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
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
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
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null,
            "done_at": 1700000000
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.done_at, Some(1700000000));
    }

    #[test]
    fn issue_deserializes_without_pruned_at_defaults_to_none() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.pruned_at, None);
    }

    #[test]
    fn issue_roundtrips_pruned_at() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.pruned_at = Some(1_700_000_000);
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"pruned_at\":1700000000"));
        let roundtrip: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.pruned_at, Some(1_700_000_000));
    }

    #[test]
    fn column_deserializes_planning_alias_as_todo() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Planning",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
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
            "prompt": null,
            "github_pr_number": 42,
            "github_pr_url": "https://example.com",
            "github_pr_title": "Some PR",
            "linear_state": "In Progress",
            "linear_branch": "feature/x"
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.id, "bork-1");
    }

    // --- AgentMode ---

    #[test]
    fn agent_mode_toggle_cycles_plan_build() {
        assert_eq!(AgentMode::Plan.toggle(), AgentMode::Build);
        assert_eq!(AgentMode::Build.toggle(), AgentMode::Plan);
    }

    #[test]
    fn agent_mode_toggle_yolo_returns_to_plan() {
        // Yolo falls back to Plan via toggle (OpenCode path)
        assert_eq!(AgentMode::Yolo.toggle(), AgentMode::Plan);
    }

    #[test]
    fn agent_mode_next_for_yolo_agents_full_cycle() {
        assert_eq!(AgentMode::Plan.next_for_yolo_agents(), AgentMode::Build);
        assert_eq!(AgentMode::Build.next_for_yolo_agents(), AgentMode::Yolo);
        assert_eq!(AgentMode::Yolo.next_for_yolo_agents(), AgentMode::Plan);
    }

    #[test]
    fn agent_mode_display() {
        assert_eq!(AgentMode::Plan.to_string(), "plan");
        assert_eq!(AgentMode::Build.to_string(), "build");
        assert_eq!(AgentMode::Yolo.to_string(), "yolo");
    }

    // --- AgentKind ---

    #[test]
    fn agent_kind_parse_and_display_pi() {
        assert_eq!(AgentKind::parse("pi"), Some(AgentKind::Pi));
        assert_eq!(AgentKind::parse("PI"), Some(AgentKind::Pi));
        assert_eq!(AgentKind::Pi.to_string(), "pi");
        assert_eq!(AgentKind::Pi.command(), "pi");
    }

    #[test]
    fn agent_kind_pi_has_no_modes() {
        assert!(!AgentKind::Pi.has_modes());
        assert!(AgentKind::OpenCode.has_modes());
        assert!(AgentKind::Claude.has_modes());
        assert!(AgentKind::Codex.has_modes());
    }

    #[test]
    fn agent_kind_all_includes_pi() {
        assert!(AgentKind::ALL.contains(&AgentKind::Pi));
        assert_eq!(AgentKind::ALL.len(), 4);
    }

    // --- Issue sessions ---

    #[test]
    fn issue_deserializes_without_sessions_defaults_to_empty() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "worktree": "main",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert!(issue.sessions.is_empty());
        assert_eq!(issue.current_session_id(), None);
    }

    #[test]
    fn issue_serializes_and_deserializes_sessions() {
        let mut issue = test_issue("bork-1", Column::InProgress);
        issue
            .sessions
            .insert(AgentKind::OpenCode, "ses_abc123xyz".to_string());
        let json = serde_json::to_string(&issue).unwrap();
        // On-disk shape: a map keyed by agent variant name.
        assert!(json.contains(r#""sessions":{"OpenCode":"ses_abc123xyz"}"#));
        let roundtrip: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.sessions, issue.sessions);
    }

    #[test]
    fn migration_yields_legacy_session_id_for_attribution() {
        // Keying is the caller's job (opencode::LegacySessionStores); types
        // only surrenders the id, marks the launch, and clears the field.
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null,
            "session_id": "ses_legacy"
        }"#;
        let mut issue: Issue = serde_json::from_str(json).unwrap();
        let legacy = issue.migrate_legacy_fields();
        assert_eq!(legacy.as_deref(), Some("ses_legacy"));
        assert_eq!(issue.session_id, None);
        // A legacy id proves a launch happened, so setup must not re-run.
        assert!(issue.setup_ran);
        // Legacy field never serializes back out.
        let out = serde_json::to_string(&issue).unwrap();
        assert!(!out.contains("\"session_id\""));
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

    #[test]
    fn github_imported_alias_deserializes_as_pr_imported() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null,
            "github_imported": true
        }"#;
        let issue: Issue = serde_json::from_str(json).unwrap();
        assert!(issue.pr_imported);
    }

    #[test]
    fn migrate_legacy_linear_fields() {
        let json = r#"{
            "id": "vil-123",
            "title": "Fix auth",
            "column": "Todo",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null,
            "linear_id": "uuid-abc",
            "linear_identifier": "VIL-123",
            "linear_url": "https://linear.app/issue/VIL-123",
            "linear_imported": true
        }"#;
        let mut issue: Issue = serde_json::from_str(json).unwrap();
        let _ = issue.migrate_legacy_fields();
        assert_eq!(issue.linear_links.len(), 1);
        assert_eq!(issue.linear_links[0].id, "uuid-abc");
        assert_eq!(issue.linear_links[0].identifier, "VIL-123");
        assert!(issue.linear_links[0].imported);
    }

    #[test]
    fn migrate_legacy_pr_fields() {
        let json = r#"{
            "id": "bork-1",
            "title": "Fix bug",
            "column": "CodeReview",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null,
            "pr_number": 42,
            "pr_imported": true,
            "pr_import_source": "Authored"
        }"#;
        let mut issue: Issue = serde_json::from_str(json).unwrap();
        let _ = issue.migrate_legacy_fields();
        assert_eq!(issue.github_pr_links.len(), 1);
        assert_eq!(issue.github_pr_links[0].number, 42);
        assert!(issue.github_pr_links[0].imported);
        assert_eq!(
            issue.github_pr_links[0].import_source,
            Some(PrImportSource::Authored)
        );
    }

    #[test]
    fn migrate_does_not_overwrite_new_fields() {
        let json = r#"{
            "id": "bork-1",
            "title": "Test",
            "column": "Todo",
            "agent_kind": "OpenCode",
            "agent_mode": "Plan",
            "prompt": null,
            "linear_links": [{"id": "uuid-1", "identifier": "VIL-1", "url": "https://a"}],
            "linear_id": "uuid-2",
            "linear_identifier": "VIL-2",
            "linear_url": "https://b"
        }"#;
        let mut issue: Issue = serde_json::from_str(json).unwrap();
        let _ = issue.migrate_legacy_fields();
        assert_eq!(issue.linear_links.len(), 1);
        assert_eq!(issue.linear_links[0].identifier, "VIL-1");
    }

    #[test]
    fn new_format_serializes_without_legacy_fields() {
        let mut issue = test_issue("bork-1", Column::Todo);
        issue.linear_links.push(LinkedLinear {
            id: "uuid".into(),
            identifier: "VIL-1".into(),
            url: "https://a".into(),
            imported: false,
        });
        issue.github_pr_links.push(LinkedGithubPr {
            number: 42,
            imported: false,
            import_source: None,
        });
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"linear_links\""));
        assert!(json.contains("\"github_pr_links\""));
        assert!(!json.contains("\"linear_id\""));
        assert!(!json.contains("\"pr_number\""));
    }
}
