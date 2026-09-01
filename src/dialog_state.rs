//! New/edit issue dialog state: field ordering, focus, and the plain-text
//! editing primitives used by the title field.

use std::collections::HashMap;

use ratatui::style::{Modifier, Style};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::external::linear::LinearIssue;
use crate::types::{AgentKind, AgentMode, Column, Issue, IssueKind, PrStatus};
use crate::ui::styles;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogField {
    Kind,
    Agent,
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
    pub available_agents: Vec<AgentKind>,
    pub agent_mode: AgentMode,
    pub agent_kind: AgentKind,
    /// The agent shown when the dialog opened (post-normalization). Submit
    /// compares against this to tell a user-made switch from the silent
    /// fallback `resolve_agent_kind` applies when the stored agent isn't
    /// available — only the former should kill a live session.
    pub initial_agent_kind: AgentKind,
    pub focused_field: usize,
    pub editing_index: Option<usize>,
    /// ID of the issue being edited. Submit resolves by this, not by
    /// `editing_index`, since background merges can reorder issues while
    /// the dialog is open.
    pub editing_issue_id: Option<String>,
    pub target_column: Option<Column>,
    pub linear_issues: Vec<LinearIssue>,
    pub linear_detached: bool,
    pub linear_available: bool,
    pub github_prs: Vec<PrStatus>,
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
    pub fn new(
        agent_kind: AgentKind,
        agent_mode: AgentMode,
        available_agents: Vec<AgentKind>,
        linear_available: bool,
        github_available: bool,
    ) -> Self {
        let kind = IssueKind::Agentic;
        let resolved_agent = Self::resolve_agent_kind(agent_kind, &available_agents);
        let title_idx = Self::compute_title_index(
            kind,
            resolved_agent,
            &available_agents,
            linear_available,
            github_available,
        );
        DialogState {
            kind,
            title: String::new(),
            title_cursor: 0,
            prompt: make_prompt_textarea(""),
            available_agents,
            agent_mode: Self::normalize_mode_for_agent(agent_mode, resolved_agent),
            agent_kind: resolved_agent,
            initial_agent_kind: resolved_agent,
            focused_field: title_idx,
            editing_index: None,
            editing_issue_id: None,
            target_column: None,
            linear_issues: Vec::new(),
            linear_detached: false,
            linear_available,
            github_prs: Vec::new(),
            github_pr_cleared: false,
            github_available,
        }
    }

    pub fn from_issue(
        issue: &Issue,
        index: usize,
        available_agents: Vec<AgentKind>,
        linear_available: bool,
        github_available: bool,
        all_prs: &HashMap<String, PrStatus>,
        user_prs: &[PrStatus],
    ) -> Self {
        let prompt_text = issue.prompt.as_deref().unwrap_or("");

        let linear_issues: Vec<LinearIssue> = issue
            .linear_links
            .iter()
            .map(|link| LinearIssue {
                id: link.id.clone(),
                identifier: link.identifier.clone(),
                title: issue.title.clone(),
                url: link.url.clone(),
                branch_name: String::new(),
                priority: 0,
                state_name: String::new(),
                team_key: String::new(),
            })
            .collect();

        let github_prs: Vec<PrStatus> = issue
            .github_pr_links
            .iter()
            .filter_map(|link| {
                all_prs
                    .values()
                    .chain(user_prs.iter())
                    .find(|pr| pr.number == link.number)
                    .cloned()
            })
            .collect();

        let mut prompt = make_prompt_textarea(prompt_text);
        prompt.move_cursor(CursorMove::Bottom);
        prompt.move_cursor(CursorMove::End);

        let resolved_agent = Self::resolve_agent_kind(issue.agent_kind, &available_agents);
        let title_idx = Self::compute_title_index(
            issue.kind,
            resolved_agent,
            &available_agents,
            linear_available,
            github_available,
        );
        DialogState {
            kind: issue.kind,
            title: issue.title.clone(),
            title_cursor: issue.title.chars().count(),
            prompt,
            available_agents,
            agent_mode: Self::normalize_mode_for_agent(issue.agent_mode, resolved_agent),
            agent_kind: resolved_agent,
            initial_agent_kind: resolved_agent,
            focused_field: title_idx,
            editing_index: Some(index),
            editing_issue_id: Some(issue.id.clone()),
            target_column: None,
            linear_issues,
            linear_detached: false,
            linear_available,
            github_prs,
            github_pr_cleared: false,
            github_available,
        }
    }

    fn resolve_agent_kind(preferred: AgentKind, available_agents: &[AgentKind]) -> AgentKind {
        if available_agents.contains(&preferred) {
            return preferred;
        }
        available_agents.first().copied().unwrap_or(preferred)
    }

    fn normalize_mode_for_agent(mode: AgentMode, agent_kind: AgentKind) -> AgentMode {
        // Pi has a single mode; pin it to Build so a stale Plan/Yolo from
        // another agent never carries over (and no mode picker is shown).
        if !agent_kind.has_modes() {
            return AgentMode::Build;
        }
        if agent_kind == AgentKind::OpenCode && mode == AgentMode::Yolo {
            AgentMode::Build
        } else {
            mode
        }
    }

    fn set_agent_at(&mut self, index: usize) {
        self.agent_kind = self.available_agents[index];
        self.agent_mode = Self::normalize_mode_for_agent(self.agent_mode, self.agent_kind);
    }

    fn cycle_agent_next(&mut self) {
        let len = self.available_agents.len();
        if len == 0 {
            return;
        }
        let idx = self.agent_index();
        self.set_agent_at((idx + 1) % len);
    }

    fn cycle_agent_prev(&mut self) {
        let len = self.available_agents.len();
        if len == 0 {
            return;
        }
        let idx = self.agent_index();
        self.set_agent_at(if idx == 0 { len - 1 } else { idx - 1 });
    }

    fn agent_index(&self) -> usize {
        self.available_agents
            .iter()
            .position(|kind| *kind == self.agent_kind)
            .unwrap_or(0)
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

    pub(crate) fn ordered_fields(&self) -> Vec<DialogField> {
        let mut fields = vec![DialogField::Kind];
        if self.linear_available {
            fields.push(DialogField::Linear);
        }
        // Orchestrators coordinate other issues and have no PR of their own.
        if self.github_available && self.kind != IssueKind::Orchestrator {
            fields.push(DialogField::GithubPr);
        }
        if self.kind.is_agentic() {
            fields.push(DialogField::Agent);
            if !self.available_agents.is_empty() && self.agent_kind.has_modes() {
                fields.push(DialogField::Mode);
            }
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
        agent_kind: AgentKind,
        available_agents: &[AgentKind],
        linear_available: bool,
        github_available: bool,
    ) -> usize {
        let mut idx = 1;
        if linear_available {
            idx += 1;
        }
        if github_available && kind != IssueKind::Orchestrator {
            idx += 1;
        }
        if kind.is_agentic() {
            idx += 1;
            // Mirror `ordered_fields`: the Mode field only exists for agents
            // with modes (Pi is single-mode and hides it).
            if !available_agents.is_empty() && agent_kind.has_modes() {
                idx += 1;
            }
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
                const KIND_ORDER: [IssueKind; 3] = [
                    IssueKind::Agentic,
                    IssueKind::Orchestrator,
                    IssueKind::NonAgentic,
                ];
                let idx = KIND_ORDER.iter().position(|k| *k == self.kind).unwrap_or(0);
                match c {
                    ' ' => self.kind = KIND_ORDER[(idx + 1) % KIND_ORDER.len()],
                    'h' => self.kind = KIND_ORDER[idx.saturating_sub(1)],
                    'l' => self.kind = KIND_ORDER[(idx + 1).min(KIND_ORDER.len() - 1)],
                    _ => {}
                }
                if self.kind.is_agentic() {
                    self.agent_kind =
                        Self::resolve_agent_kind(self.agent_kind, &self.available_agents);
                    self.agent_mode =
                        Self::normalize_mode_for_agent(self.agent_mode, self.agent_kind);
                }
                self.clamp_focused_field();
            }
            DialogField::Agent => match c {
                ' ' | 'l' => self.cycle_agent_next(),
                'h' => self.cycle_agent_prev(),
                _ => {}
            },
            DialogField::Mode => {
                if self.available_agents.is_empty() {
                    return;
                }
                if c == ' ' || c == 'h' || c == 'l' {
                    self.agent_mode = match self.agent_kind {
                        AgentKind::Claude | AgentKind::Codex => {
                            self.agent_mode.next_for_yolo_agents()
                        }
                        // Pi has a single mode and no Mode field, so this arm
                        // is unreachable in practice; toggle is a safe no-op.
                        AgentKind::OpenCode | AgentKind::Pi => self.agent_mode.toggle(),
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
