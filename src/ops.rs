use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{self, AppState};
use crate::external::tmux;
use crate::types::{AgentKind, AgentMode, Column, Issue, IssueKind};
use crate::ui::styles::truncate;
use crate::worktree;

pub fn next_issue_id(issues: &[Issue], project_name: &str) -> String {
    next_issue_id_after(issues, project_name, 0)
}

/// Next free `{project}-{n}` ID, skipping `offset` extra slots. The offset
/// supports batch-creating issues before any of them is pushed to the list.
pub fn next_issue_id_after(issues: &[Issue], project_name: &str, offset: u32) -> String {
    let prefix = format!("{}-", project_name);
    let max_num = issues
        .iter()
        .filter_map(|issue| {
            issue
                .id
                .strip_prefix(&prefix)
                .and_then(|s| s.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0);

    format!("{}{}", prefix, max_num + 1 + offset)
}

fn find_issue_index(issues: &[Issue], id: &str) -> Option<usize> {
    let lower = id.to_lowercase();
    issues.iter().position(|i| i.id.to_lowercase() == lower)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn move_issue_in_state(issue: &mut Issue, column: Column) {
    let was_done = issue.column == Column::Done;
    let now_done = column == Column::Done;
    issue.column = column;

    if now_done && !was_done {
        issue.done_at = Some(now_epoch());
    } else if !now_done && was_done {
        issue.done_at = None;
    }
}

pub struct ListOptions {
    pub column: Option<Column>,
    pub json: bool,
    /// When set, restrict output to the connected component of links that
    /// contains this issue id (the anchor plus everything reachable via links).
    pub linked: Option<String>,
}

/// Connected component of the link graph containing `anchor` (BFS over
/// `linked_issues`). Returns lowercased ids, including the anchor itself.
pub fn linked_component(issues: &[Issue], anchor: &str) -> HashSet<String> {
    let by_id: HashMap<String, &Issue> = issues.iter().map(|i| (i.id.to_lowercase(), i)).collect();
    let mut seen = HashSet::new();
    let mut queue = vec![anchor.to_lowercase()];
    while let Some(id) = queue.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(issue) = by_id.get(&id) {
            for linked in &issue.linked_issues {
                let next = linked.to_lowercase();
                if !seen.contains(&next) {
                    queue.push(next);
                }
            }
        }
    }
    seen
}

pub fn list_issues(project_root: &Path, opts: &ListOptions) -> anyhow::Result<String> {
    let state = config::load_state(project_root);
    let config = config::load_config_from(project_root);

    let component = opts
        .linked
        .as_deref()
        .map(|anchor| linked_component(&state.issues, anchor));

    let issues: Vec<&Issue> = state
        .issues
        .iter()
        .filter(|i| opts.column.is_none() || Some(i.column) == opts.column)
        .filter(|i| {
            component
                .as_ref()
                .is_none_or(|c| c.contains(&i.id.to_lowercase()))
        })
        .collect();

    if opts.json {
        return Ok(serde_json::to_string_pretty(&issues)?);
    }

    if issues.is_empty() {
        return Ok("No issues found.".to_string());
    }

    format_issue_table(&issues, &config.project_name)
}

fn format_issue_table(issues: &[&Issue], _project_name: &str) -> anyhow::Result<String> {
    let headers = ["ID", "TITLE", "COLUMN", "AGENT", "MODE", "WORKTREE"];

    let rows: Vec<[String; 6]> = issues
        .iter()
        .map(|i| {
            [
                i.id.clone(),
                truncate(&i.title, 40),
                i.column.to_string(),
                i.agent_kind.to_string(),
                i.agent_mode.to_string(),
                i.worktree.clone().unwrap_or_default(),
            ]
        })
        .collect();

    let widths: [usize; 6] = std::array::from_fn(|col| {
        let header_w = headers[col].len();
        let max_row_w = rows.iter().map(|r| r[col].len()).max().unwrap_or(0);
        header_w.max(max_row_w)
    });

    let mut out = String::new();

    for (i, header) in headers.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let _ = write!(out, "{:<width$}", header, width = widths[i]);
    }
    out.push('\n');

    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let _ = write!(out, "{:<width$}", cell, width = widths[i]);
        }
        out.push('\n');
    }

    Ok(out.trim_end().to_string())
}

pub struct CreateOptions {
    pub title: String,
    pub column: Option<Column>,
    pub agent_kind: Option<AgentKind>,
    pub agent_mode: Option<AgentMode>,
    pub prompt: Option<String>,
    pub kind: Option<IssueKind>,
}

pub fn create_issue(project_root: &Path, opts: CreateOptions) -> anyhow::Result<Issue> {
    let config = config::load_config_from(project_root);
    let mut state = config::load_state(project_root);

    let id = next_issue_id(&state.issues, &config.project_name);
    let column = opts.column.unwrap_or(Column::Todo);
    let kind = opts.kind.unwrap_or(IssueKind::Agentic);
    let agent_kind = opts.agent_kind.unwrap_or(config.agent_kind);
    let agent_mode = opts.agent_mode.unwrap_or(AgentMode::Plan);

    let issue = Issue {
        kind,
        agent_mode,
        prompt: opts.prompt,
        done_at: if column == Column::Done {
            Some(now_epoch())
        } else {
            None
        },
        ..Issue::new(id, opts.title, column, agent_kind)
    };

    state.issues.push(issue.clone());
    config::save_state(&state, project_root)?;

    Ok(issue)
}

pub struct UpdateOptions {
    pub title: Option<String>,
    pub column: Option<Column>,
    pub agent_kind: Option<AgentKind>,
    pub agent_mode: Option<AgentMode>,
    pub prompt: Option<String>,
    pub kind: Option<IssueKind>,
}

pub fn update_issue(
    project_root: &Path,
    issue_id: &str,
    opts: UpdateOptions,
) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let issue = &mut state.issues[idx];

    if let Some(title) = opts.title {
        issue.title = title;
    }
    if let Some(column) = opts.column {
        move_issue_in_state(issue, column);
    }
    if let Some(agent_kind) = opts.agent_kind {
        issue.agent_kind = agent_kind;
    }
    if let Some(agent_mode) = opts.agent_mode {
        issue.agent_mode = agent_mode;
    }
    if let Some(prompt) = opts.prompt {
        if prompt.is_empty() {
            issue.prompt = None;
        } else {
            issue.prompt = Some(prompt);
        }
    }
    if let Some(kind) = opts.kind {
        if issue.set_kind(kind) {
            // Kill any live session so it isn't re-attached with the old
            // kind's prompt and cwd. Best effort; the session may not exist.
            let config = config::load_config_from(project_root);
            let _ = tmux::kill_session(&issue.session_name(&config.project_name));
        }
    }

    let updated = issue.clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub fn delete_issue(project_root: &Path, issue_id: &str) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let removed = state.issues.remove(idx);
    remove_link_references(&mut state.issues, &removed.id);
    config::save_state(&state, project_root)?;

    Ok(removed)
}

/// Drop `removed_id` from every issue's `linked_issues`, keeping links symmetric
/// after an issue is deleted or archived.
pub fn remove_link_references(issues: &mut [Issue], removed_id: &str) {
    for issue in issues.iter_mut() {
        issue
            .linked_issues
            .retain(|l| !l.eq_ignore_ascii_case(removed_id));
    }
}

/// Tie two issues together. Links are symmetric: each issue records the other's
/// id. Both issues must exist in the same project; self-links are rejected.
pub fn link_issues(project_root: &Path, a: &str, b: &str) -> anyhow::Result<(String, String)> {
    let mut state = config::load_state(project_root);

    let idx_a = find_issue_index(&state.issues, a)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", a))?;
    let idx_b = find_issue_index(&state.issues, b)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", b))?;

    if idx_a == idx_b {
        anyhow::bail!("Cannot link an issue to itself");
    }

    let id_a = state.issues[idx_a].id.clone();
    let id_b = state.issues[idx_b].id.clone();

    if !state.issues[idx_a].is_linked_to(&id_b) {
        state.issues[idx_a].linked_issues.push(id_b.clone());
    }
    if !state.issues[idx_b].is_linked_to(&id_a) {
        state.issues[idx_b].linked_issues.push(id_a.clone());
    }

    config::save_state(&state, project_root)?;
    Ok((id_a, id_b))
}

/// Remove a symmetric link between two issues.
pub fn unlink_issues(project_root: &Path, a: &str, b: &str) -> anyhow::Result<(String, String)> {
    let mut state = config::load_state(project_root);

    let idx_a = find_issue_index(&state.issues, a)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", a))?;
    let idx_b = find_issue_index(&state.issues, b)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", b))?;

    let id_a = state.issues[idx_a].id.clone();
    let id_b = state.issues[idx_b].id.clone();

    state.issues[idx_a]
        .linked_issues
        .retain(|l| !l.eq_ignore_ascii_case(&id_b));
    state.issues[idx_b]
        .linked_issues
        .retain(|l| !l.eq_ignore_ascii_case(&id_a));

    config::save_state(&state, project_root)?;
    Ok((id_a, id_b))
}

pub fn show_issue(project_root: &Path, issue_id: &str, json: bool) -> anyhow::Result<String> {
    let state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let issue = &state.issues[idx];

    if json {
        return Ok(serde_json::to_string_pretty(issue)?);
    }

    let mut out = String::new();
    let _ = writeln!(out, "ID:       {}", issue.id);
    let _ = writeln!(out, "Title:    {}", issue.title);
    let _ = writeln!(out, "Kind:     {}", issue.kind);
    let _ = writeln!(out, "Column:   {}", issue.column);
    let _ = writeln!(out, "Agent:    {}", issue.agent_kind);
    let _ = writeln!(out, "Mode:     {}", issue.agent_mode);
    if let Some(ref prompt) = issue.prompt {
        let _ = writeln!(out, "Prompt:   {}", prompt);
    }
    if let Some(ref wt) = issue.worktree {
        let _ = writeln!(out, "Worktree: {}", wt);
    }
    if !issue.linear_links.is_empty() {
        let ids: Vec<&str> = issue.linear_identifiers();
        let _ = writeln!(out, "Linear:   {}", ids.join(", "));
    }
    if !issue.github_pr_links.is_empty() {
        let nums: Vec<String> = issue
            .pr_numbers()
            .iter()
            .map(|n| format!("#{}", n))
            .collect();
        let _ = writeln!(out, "PR:       {}", nums.join(", "));
    }
    if !issue.linked_issues.is_empty() {
        let _ = writeln!(out, "Linked:   {}", issue.linked_issues.join(", "));
    }

    Ok(out.trim_end().to_string())
}

pub fn attach_linear(
    project_root: &Path,
    issue_id: &str,
    linear_identifier: &str,
) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let identifier = linear_identifier.to_uppercase();
    let issue = &mut state.issues[idx];

    if !issue
        .linear_links
        .iter()
        .any(|l| l.identifier == identifier)
    {
        issue.linear_links.push(crate::types::LinkedLinear {
            id: String::new(),
            identifier,
            url: String::new(),
            imported: false,
        });
    }

    let updated = issue.clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub fn attach_pr(project_root: &Path, issue_id: &str, pr_number: u32) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let issue = &mut state.issues[idx];

    if issue.kind == IssueKind::Orchestrator {
        anyhow::bail!(
            "Cannot attach a PR to '{}': orchestrator issues have no PR links",
            issue.id
        );
    }

    if !issue.has_pr_number(pr_number) {
        issue.github_pr_links.push(crate::types::LinkedGithubPr {
            number: pr_number,
            imported: false,
            import_source: None,
        });
    }

    let updated = issue.clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub fn detach_linear(
    project_root: &Path,
    issue_id: &str,
    linear_identifier: &str,
) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let identifier = linear_identifier.to_uppercase();
    let issue = &mut state.issues[idx];

    let before = issue.linear_links.len();
    issue.linear_links.retain(|l| l.identifier != identifier);
    if issue.linear_links.len() == before {
        anyhow::bail!("Issue '{}' has no Linear link '{}'", issue.id, identifier);
    }

    let updated = issue.clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub fn detach_pr(project_root: &Path, issue_id: &str, pr_number: u32) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let issue = &mut state.issues[idx];

    let before = issue.github_pr_links.len();
    issue.github_pr_links.retain(|l| l.number != pr_number);
    if issue.github_pr_links.len() == before {
        anyhow::bail!("Issue '{}' has no PR link #{}", issue.id, pr_number);
    }

    let updated = issue.clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub fn clear_linear(project_root: &Path, issue_id: &str) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    state.issues[idx].linear_links.clear();

    let updated = state.issues[idx].clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub fn clear_pr(project_root: &Path, issue_id: &str) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    state.issues[idx].github_pr_links.clear();

    let updated = state.issues[idx].clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub struct MoveIssuesReport {
    pub moved: Vec<Issue>,
    pub skipped: Vec<String>,
}

#[cfg(test)]
pub fn move_issue(project_root: &Path, issue_id: &str, column: Column) -> anyhow::Result<Issue> {
    let report = move_issues(project_root, &[issue_id.to_string()], None, column)?;
    report
        .moved
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))
}

pub fn move_issues(
    project_root: &Path,
    issue_ids: &[String],
    linked: Option<&str>,
    column: Column,
) -> anyhow::Result<MoveIssuesReport> {
    let mut state = config::load_state(project_root);
    let mut targets: HashSet<String> = issue_ids.iter().map(|id| id.to_lowercase()).collect();
    let mut skipped = Vec::new();

    if let Some(anchor) = linked {
        if find_issue_index(&state.issues, anchor).is_some() {
            targets.extend(linked_component(&state.issues, anchor));
        } else {
            skipped.push(anchor.to_string());
        }
    }

    let mut moved = Vec::new();
    for id in targets {
        let Some(idx) = find_issue_index(&state.issues, &id) else {
            skipped.push(id);
            continue;
        };
        move_issue_in_state(&mut state.issues[idx], column);
        moved.push(state.issues[idx].clone());
    }

    if !moved.is_empty() {
        config::save_state(&state, project_root)?;
    }

    Ok(MoveIssuesReport { moved, skipped })
}

pub struct ArchiveReport {
    pub issue_id: String,
    pub title: String,
    pub session_name: String,
    pub session_killed: bool,
    pub worktree_removed: Option<String>,
}

/// Archive an issue: kill its agent session, run the teardown script, remove
/// its worktree, and move it to Done.
///
/// `force` lets the archive proceed past a failing teardown script and
/// discards uncommitted changes in the worktree.
pub fn archive_issue(
    project_root: &Path,
    issue_id: &str,
    force: bool,
) -> anyhow::Result<ArchiveReport> {
    let app_config = config::load_config_from(project_root);
    let state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;
    let issue = state.issues[idx].clone();

    let session_name = issue.session_name(&app_config.project_name);
    let session_killed = tmux::session_exists(&session_name);
    if session_killed {
        let _ = tmux::kill_session(&session_name);
    }

    let worktree_removed = match issue.worktree.as_deref() {
        Some(dir) => {
            worktree::remove_worktree_in(&app_config, dir, force)?;
            Some(dir.to_string())
        }
        None => None,
    };

    // Reload state so we don't clobber concurrent updates made while the
    // teardown script and git removal were running.
    let mut state = config::load_state(project_root);
    if let Some(saved) = state.issues.iter_mut().find(|i| i.id == issue.id) {
        saved.worktree = None;
        if saved.column != Column::Done {
            saved.column = Column::Done;
            saved.done_at = Some(now_epoch());
        }
    }
    config::save_state(&state, project_root)?;

    Ok(ArchiveReport {
        issue_id: issue.id,
        title: issue.title,
        session_name,
        session_killed,
        worktree_removed,
    })
}

/// Load state and return a JSON snapshot of all issues (for machine consumption).
#[allow(dead_code)] // Useful debugging/scripting utility; not yet wired to a CLI subcommand
pub fn dump_state(project_root: &Path) -> anyhow::Result<String> {
    let state = config::load_state(project_root);
    Ok(serde_json::to_string_pretty(&AppState {
        issues: state.issues,
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_project() -> TempDir {
        let dir = TempDir::new().unwrap();
        let bork_dir = dir.path().join(".bork");
        fs::create_dir_all(&bork_dir).unwrap();
        fs::write(
            bork_dir.join("config.toml"),
            "project_name = \"test\"\nagent_kind = \"opencode\"\n",
        )
        .unwrap();
        fs::write(bork_dir.join("state.json"), r#"{"issues":[]}"#).unwrap();
        dir
    }

    #[test]
    fn next_id_empty() {
        assert_eq!(next_issue_id(&[], "bork"), "bork-1");
    }

    #[test]
    fn next_id_increments() {
        let issues = vec![
            test_issue("bork-1", Column::Todo),
            test_issue("bork-3", Column::Todo),
        ];
        assert_eq!(next_issue_id(&issues, "bork"), "bork-4");
    }

    #[test]
    fn next_id_ignores_non_matching_prefix() {
        let issues = vec![test_issue("vil-123", Column::Todo)];
        assert_eq!(next_issue_id(&issues, "bork"), "bork-1");
    }

    #[test]
    fn next_id_after_skips_offset_slots() {
        let issues = vec![test_issue("bork-2", Column::Todo)];
        assert_eq!(next_issue_id_after(&issues, "bork", 0), "bork-3");
        assert_eq!(next_issue_id_after(&issues, "bork", 2), "bork-5");
    }

    #[test]
    fn list_table_handles_non_ascii_titles() {
        // Regression: byte-slice truncation panicked on multi-byte UTF-8.
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Fix résumé parsing — naïve solution breaks on émojis 🎉🎉🎉🎉🎉🎉🎉".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let output = list_issues(
            root,
            &ListOptions {
                column: None,
                json: false,
                linked: None,
            },
        )
        .unwrap();
        assert!(output.contains("test-1"));
    }

    #[test]
    fn create_and_list() {
        let dir = setup_project();
        let root = dir.path();

        let issue = create_issue(
            root,
            CreateOptions {
                title: "Test issue".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: Some("Do the thing".into()),
                kind: None,
            },
        )
        .unwrap();

        assert_eq!(issue.id, "test-1");
        assert_eq!(issue.title, "Test issue");
        assert_eq!(issue.column, Column::Todo);
        assert_eq!(issue.prompt, Some("Do the thing".into()));

        let output = list_issues(
            root,
            &ListOptions {
                column: None,
                json: false,
                linked: None,
            },
        )
        .unwrap();
        assert!(output.contains("test-1"));
        assert!(output.contains("Test issue"));
    }

    #[test]
    fn create_in_done_sets_done_at() {
        let dir = setup_project();

        let issue = create_issue(
            dir.path(),
            CreateOptions {
                title: "Done issue".into(),
                column: Some(Column::Done),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        assert!(issue.done_at.is_some());
    }

    #[test]
    fn update_fields() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Original".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let updated = update_issue(
            root,
            "test-1",
            UpdateOptions {
                title: Some("Updated title".into()),
                column: Some(Column::InProgress),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        assert_eq!(updated.title, "Updated title");
        assert_eq!(updated.column, Column::InProgress);
    }

    #[test]
    fn update_to_done_sets_done_at() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Move me".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let updated = update_issue(
            root,
            "test-1",
            UpdateOptions {
                title: None,
                column: Some(Column::Done),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        assert!(updated.done_at.is_some());
    }

    #[test]
    fn update_from_done_clears_done_at() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Done then not".into(),
                column: Some(Column::Done),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let updated = update_issue(
            root,
            "test-1",
            UpdateOptions {
                title: None,
                column: Some(Column::Todo),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        assert!(updated.done_at.is_none());
    }

    #[test]
    fn update_kind_to_orchestrator_clears_stale_state() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Convert me".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        // Simulate an issue that already ran: worktree, session, and a PR link.
        let mut state = config::load_state(root);
        state.issues[0].worktree = Some("test-1-convert-me".into());
        state.issues[0].session_id = Some("ses_abc".into());
        state.issues[0]
            .github_pr_links
            .push(crate::types::LinkedGithubPr {
                number: 42,
                imported: false,
                import_source: None,
            });
        config::save_state(&state, root).unwrap();

        let updated = update_issue(
            root,
            "test-1",
            UpdateOptions {
                title: None,
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: Some(IssueKind::Orchestrator),
            },
        )
        .unwrap();

        assert_eq!(updated.kind, IssueKind::Orchestrator);
        assert!(updated.worktree.is_none());
        assert!(updated.session_id.is_none());
        assert!(updated.github_pr_links.is_empty());
    }

    #[test]
    fn attach_pr_rejects_orchestrator_issue() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Coordinate".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: Some(IssueKind::Orchestrator),
            },
        )
        .unwrap();

        let result = attach_pr(root, "test-1", 42);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("orchestrator"));

        let state = config::load_state(root);
        assert!(state.issues[0].github_pr_links.is_empty());
    }

    #[test]
    fn delete_removes_issue() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Delete me".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let deleted = delete_issue(root, "test-1").unwrap();
        assert_eq!(deleted.title, "Delete me");

        let output = list_issues(
            root,
            &ListOptions {
                column: None,
                json: false,
                linked: None,
            },
        )
        .unwrap();
        assert_eq!(output, "No issues found.");
    }

    #[test]
    fn delete_nonexistent_fails() {
        let dir = setup_project();
        assert!(delete_issue(dir.path(), "nope-99").is_err());
    }

    #[test]
    fn show_human_readable() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Show me".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: Some("A prompt".into()),
                kind: None,
            },
        )
        .unwrap();

        let output = show_issue(root, "test-1", false).unwrap();
        assert!(output.contains("ID:       test-1"));
        assert!(output.contains("Title:    Show me"));
        assert!(output.contains("Prompt:   A prompt"));
    }

    #[test]
    fn show_json() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "JSON me".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let output = show_issue(root, "test-1", true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["id"], "test-1");
        assert_eq!(parsed["title"], "JSON me");
    }

    #[test]
    fn list_filter_by_column() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Todo issue".into(),
                column: Some(Column::Todo),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();
        create_issue(
            root,
            CreateOptions {
                title: "InProgress issue".into(),
                column: Some(Column::InProgress),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let output = list_issues(
            root,
            &ListOptions {
                column: Some(Column::Todo),
                json: false,
                linked: None,
            },
        )
        .unwrap();
        assert!(output.contains("Todo issue"));
        assert!(!output.contains("InProgress issue"));
    }

    #[test]
    fn list_json() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "JSON list".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let output = list_issues(
            root,
            &ListOptions {
                column: None,
                json: true,
                linked: None,
            },
        )
        .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["id"], "test-1");
    }

    #[test]
    fn attach_linear_sets_identifier() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Link to Linear".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let updated = attach_linear(root, "test-1", "VIL-456").unwrap();
        assert_eq!(updated.linear_links.len(), 1);
        assert_eq!(updated.linear_links[0].identifier, "VIL-456");
    }

    #[test]
    fn attach_pr_sets_number() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Link to PR".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let updated = attach_pr(root, "test-1", 42).unwrap();
        assert_eq!(updated.github_pr_links.len(), 1);
        assert_eq!(updated.github_pr_links[0].number, 42);
    }

    #[test]
    fn update_nonexistent_fails() {
        let dir = setup_project();
        let result = update_issue(
            dir.path(),
            "nope-1",
            UpdateOptions {
                title: Some("X".into()),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn case_insensitive_lookup() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Case test".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        assert!(show_issue(root, "TEST-1", false).is_ok());
    }

    #[test]
    fn archive_without_worktree_moves_to_done() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Archive me".into(),
                column: Some(Column::InProgress),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let report = archive_issue(root, "test-1", false).unwrap();
        assert_eq!(report.issue_id, "test-1");
        assert_eq!(report.title, "Archive me");
        assert!(report.worktree_removed.is_none());

        let state = config::load_state(root);
        let issue = state.issues.iter().find(|i| i.id == "test-1").unwrap();
        assert_eq!(issue.column, Column::Done);
        assert!(issue.done_at.is_some());
        assert!(issue.worktree.is_none());
    }

    #[test]
    fn archive_orchestrator_issue_moves_to_done() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Coordinate".into(),
                column: Some(Column::InProgress),
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: Some(IssueKind::Orchestrator),
            },
        )
        .unwrap();

        let report = archive_issue(root, "test-1", false).unwrap();
        assert!(report.worktree_removed.is_none());

        let state = config::load_state(root);
        let issue = state.issues.iter().find(|i| i.id == "test-1").unwrap();
        assert_eq!(issue.column, Column::Done);
        assert_eq!(issue.kind, IssueKind::Orchestrator);
    }

    #[test]
    fn archive_nonexistent_fails() {
        let dir = setup_project();
        assert!(archive_issue(dir.path(), "nope-99", false).is_err());
    }

    #[test]
    fn archive_is_case_insensitive() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Case test".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        assert!(archive_issue(root, "TEST-1", false).is_ok());
    }

    #[test]
    fn move_issue_changes_column() {
        let dir = setup_project();
        let root = dir.path();

        create_issue(
            root,
            CreateOptions {
                title: "Move me".into(),
                column: None,
                agent_kind: None,
                agent_mode: None,
                prompt: None,
                kind: None,
            },
        )
        .unwrap();

        let moved = move_issue(root, "test-1", Column::CodeReview).unwrap();
        assert_eq!(moved.column, Column::CodeReview);
    }

    fn test_issue(id: &str, column: Column) -> Issue {
        Issue::new(id, format!("Test {}", id), column, AgentKind::OpenCode)
    }

    fn seed_issues(root: &Path, ids: &[&str]) {
        let issues = ids.iter().map(|id| test_issue(id, Column::Todo)).collect();
        config::save_state(&AppState { issues }, root).unwrap();
    }

    #[test]
    fn link_is_symmetric() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1", "test-2"]);

        link_issues(root, "test-1", "test-2").unwrap();

        let state = config::load_state(root);
        assert!(state.issues[0].is_linked_to("test-2"));
        assert!(state.issues[1].is_linked_to("test-1"));
    }

    #[test]
    fn link_is_idempotent_and_case_insensitive() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1", "test-2"]);

        link_issues(root, "test-1", "test-2").unwrap();
        link_issues(root, "TEST-1", "TEST-2").unwrap();

        let state = config::load_state(root);
        assert_eq!(state.issues[0].linked_issues.len(), 1);
        assert_eq!(state.issues[1].linked_issues.len(), 1);
    }

    #[test]
    fn link_rejects_self() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1"]);

        assert!(link_issues(root, "test-1", "test-1").is_err());
    }

    #[test]
    fn link_rejects_missing_issue() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1"]);

        assert!(link_issues(root, "test-1", "test-99").is_err());
    }

    #[test]
    fn unlink_removes_both_sides() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1", "test-2"]);
        link_issues(root, "test-1", "test-2").unwrap();

        unlink_issues(root, "test-1", "test-2").unwrap();

        let state = config::load_state(root);
        assert!(state.issues[0].linked_issues.is_empty());
        assert!(state.issues[1].linked_issues.is_empty());
    }

    #[test]
    fn delete_strips_link_references() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1", "test-2", "test-3"]);
        link_issues(root, "test-1", "test-2").unwrap();
        link_issues(root, "test-2", "test-3").unwrap();

        delete_issue(root, "test-2").unwrap();

        let state = config::load_state(root);
        assert!(state.issues.iter().all(|i| !i.is_linked_to("test-2")));
    }

    #[test]
    fn linked_component_spans_chain() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1", "test-2", "test-3", "test-4"]);
        link_issues(root, "test-1", "test-2").unwrap();
        link_issues(root, "test-2", "test-3").unwrap();

        let state = config::load_state(root);
        let component = linked_component(&state.issues, "test-1");

        assert!(component.contains("test-1"));
        assert!(component.contains("test-2"));
        assert!(component.contains("test-3"));
        assert!(!component.contains("test-4"));
    }

    #[test]
    fn detach_linear_removes_matching_link() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1"]);
        attach_linear(root, "test-1", "VIL-1").unwrap();
        attach_linear(root, "test-1", "VIL-2").unwrap();

        let updated = detach_linear(root, "test-1", "vil-1").unwrap();

        assert_eq!(updated.linear_identifiers(), vec!["VIL-2"]);
    }

    #[test]
    fn detach_linear_errors_when_not_linked() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1"]);

        assert!(detach_linear(root, "test-1", "VIL-1").is_err());
    }

    #[test]
    fn detach_pr_removes_matching_link() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1"]);
        attach_pr(root, "test-1", 41).unwrap();
        attach_pr(root, "test-1", 42).unwrap();

        let updated = detach_pr(root, "test-1", 41).unwrap();

        assert_eq!(updated.pr_numbers(), vec![42]);
    }

    #[test]
    fn detach_pr_errors_when_not_linked() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1"]);

        assert!(detach_pr(root, "test-1", 42).is_err());
    }

    #[test]
    fn clear_linear_and_pr_remove_all_links() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1"]);
        attach_linear(root, "test-1", "VIL-1").unwrap();
        attach_linear(root, "test-1", "VIL-2").unwrap();
        attach_pr(root, "test-1", 42).unwrap();

        let updated = clear_linear(root, "test-1").unwrap();
        assert!(updated.linear_links.is_empty());

        let updated = clear_pr(root, "test-1").unwrap();
        assert!(updated.github_pr_links.is_empty());
    }

    #[test]
    fn list_linked_filters_to_component() {
        let dir = setup_project();
        let root = dir.path();
        seed_issues(root, &["test-1", "test-2", "test-3"]);
        link_issues(root, "test-1", "test-2").unwrap();

        let output = list_issues(
            root,
            &ListOptions {
                column: None,
                json: false,
                linked: Some("test-1".into()),
            },
        )
        .unwrap();

        assert!(output.contains("test-1"));
        assert!(output.contains("test-2"));
        assert!(!output.contains("test-3"));
    }
}
