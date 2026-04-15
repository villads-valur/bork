use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{self, AppState};
use crate::types::{AgentKind, AgentMode, Column, Issue, IssueKind};

pub fn next_issue_id(issues: &[Issue], project_name: &str) -> String {
    let prefix = project_name;
    let max_num = issues
        .iter()
        .filter_map(|issue| {
            issue
                .id
                .strip_prefix(&format!("{}-", prefix))
                .and_then(|s| s.parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0);

    format!("{}-{}", prefix, max_num + 1)
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

pub struct ListOptions {
    pub column: Option<Column>,
    pub json: bool,
}

pub fn list_issues(project_root: &Path, opts: &ListOptions) -> anyhow::Result<String> {
    let state = config::load_state(project_root);
    let config = config::load_config_from(project_root);

    let issues: Vec<&Issue> = state
        .issues
        .iter()
        .filter(|i| opts.column.is_none() || Some(i.column) == opts.column)
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
        out.push_str(&format!("{:<width$}", header, width = widths[i]));
    }
    out.push('\n');

    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(&format!("{:<width$}", cell, width = widths[i]));
        }
        out.push('\n');
    }

    Ok(out.trim_end().to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
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
        id,
        title: opts.title,
        kind,
        column,
        agent_kind,
        agent_mode,
        prompt: opts.prompt,
        worktree: None,
        done_at: if column == Column::Done {
            Some(now_epoch())
        } else {
            None
        },
        session_id: None,
        linear_links: Vec::new(),
        github_pr_links: Vec::new(),
        linear_id: None,
        linear_identifier: None,
        linear_url: None,
        linear_imported: false,
        pr_number: None,
        pr_imported: false,
        pr_import_source: None,
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
        let was_done = issue.column == Column::Done;
        let now_done = column == Column::Done;
        issue.column = column;

        if now_done && !was_done {
            issue.done_at = Some(now_epoch());
        } else if !now_done && was_done {
            issue.done_at = None;
        }
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

    let updated = issue.clone();
    config::save_state(&state, project_root)?;

    Ok(updated)
}

pub fn delete_issue(project_root: &Path, issue_id: &str) -> anyhow::Result<Issue> {
    let mut state = config::load_state(project_root);

    let idx = find_issue_index(&state.issues, issue_id)
        .ok_or_else(|| anyhow::anyhow!("Issue '{}' not found", issue_id))?;

    let removed = state.issues.remove(idx);
    config::save_state(&state, project_root)?;

    Ok(removed)
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
    out.push_str(&format!("ID:       {}\n", issue.id));
    out.push_str(&format!("Title:    {}\n", issue.title));
    out.push_str(&format!("Kind:     {}\n", issue.kind));
    out.push_str(&format!("Column:   {}\n", issue.column));
    out.push_str(&format!("Agent:    {}\n", issue.agent_kind));
    out.push_str(&format!("Mode:     {}\n", issue.agent_mode));
    if let Some(ref prompt) = issue.prompt {
        out.push_str(&format!("Prompt:   {}\n", prompt));
    }
    if let Some(ref wt) = issue.worktree {
        out.push_str(&format!("Worktree: {}\n", wt));
    }
    if !issue.linear_links.is_empty() {
        let ids: Vec<&str> = issue.linear_identifiers();
        out.push_str(&format!("Linear:   {}\n", ids.join(", ")));
    }
    if !issue.github_pr_links.is_empty() {
        let nums: Vec<String> = issue
            .pr_numbers()
            .iter()
            .map(|n| format!("#{}", n))
            .collect();
        out.push_str(&format!("PR:       {}\n", nums.join(", ")));
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

pub fn move_issue(project_root: &Path, issue_id: &str, column: Column) -> anyhow::Result<Issue> {
    update_issue(
        project_root,
        issue_id,
        UpdateOptions {
            title: None,
            column: Some(column),
            agent_kind: None,
            agent_mode: None,
            prompt: None,
        },
    )
}

/// Load state and return a JSON snapshot of all issues (for machine consumption).
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
            },
        )
        .unwrap();

        assert!(updated.done_at.is_none());
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
        Issue {
            id: id.to_string(),
            title: format!("Test {}", id),
            kind: IssueKind::Agentic,
            column,
            agent_kind: AgentKind::OpenCode,
            agent_mode: AgentMode::Plan,
            prompt: None,
            worktree: None,
            done_at: None,
            session_id: None,
            linear_links: Vec::new(),
            github_pr_links: Vec::new(),
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
            pr_import_source: None,
        }
    }
}
