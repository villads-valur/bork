use std::process::Command;

use anyhow::{bail, Context};

use crate::config;
use crate::types::{AgentMode, Column, Issue, IssueKind};

/// Create a git worktree and register it with bork's state.json.
pub fn run_worktree(issue_id: &str, slug: Option<&str>, title: Option<&str>) -> anyhow::Result<()> {
    let config = config::load_config();
    run_worktree_in(&config, issue_id, slug, title)
}

fn run_worktree_in(
    config: &config::AppConfig,
    issue_id: &str,
    slug: Option<&str>,
    title: Option<&str>,
) -> anyhow::Result<()> {
    let mut state = config::load_state(&config.project_root);

    let main_dir = config.project_root.join("main");
    if !main_dir.join(".git").exists() {
        bail!(
            "No git repo found at {}/main. Are you in a bork project?",
            config.project_root.display()
        );
    }

    let worktree_dir = match slug {
        Some(s) => format!("{}-{}", issue_id, s),
        None => issue_id.to_string(),
    };
    if config.project_root.join(&worktree_dir).exists() {
        bail!(
            "Directory '{}' already exists. Use the existing worktree or remove it first.",
            worktree_dir
        );
    }

    let branch_name = match slug {
        Some(s) => format!("{}/{}", issue_id, s),
        None => issue_id.to_string(),
    };

    let status = Command::new("git")
        .args([
            "worktree",
            "add",
            &format!("../{}", worktree_dir),
            "-b",
            &branch_name,
        ])
        .current_dir(&main_dir)
        .status()
        .context("Failed to run git worktree add")?;

    if !status.success() {
        bail!("git worktree add failed");
    }

    if let Some(issue) = state.issues.iter_mut().find(|i| i.id == issue_id) {
        issue.worktree = Some(worktree_dir.to_string());
    } else if let Some(title) = title {
        let issue = Issue {
            id: issue_id.to_string(),
            title: title.to_string(),
            kind: IssueKind::Agentic,
            column: Column::Todo,
            agent_kind: config.agent_kind,
            agent_mode: AgentMode::Plan,
            prompt: None,
            worktree: Some(worktree_dir.to_string()),
            done_at: None,
            session_id: None,
            linear_id: None,
            linear_identifier: None,
            linear_url: None,
            linear_imported: false,
            pr_number: None,
            pr_imported: false,
            pr_import_source: None,
        };
        state.issues.push(issue);
    } else {
        println!(
            "Note: Issue '{}' not found in state.json. \
             The worktree was created but not linked to an issue. \
             Use --title to create the issue, or create it in the bork TUI.",
            issue_id
        );
    }

    config::save_state(&state, &config.project_root)?;

    println!(
        "Created worktree: {}/ on branch {}",
        worktree_dir, branch_name
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn setup_test_project() -> (std::path::PathBuf, std::path::PathBuf, config::AppConfig) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("bork-wt-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Create a bare repo and clone into main/
        let bare = tmp.join("bare.git");
        fs::create_dir_all(&bare).unwrap();
        Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&bare)
            .output()
            .unwrap();

        let main_dir = tmp.join("project").join("main");
        Command::new("git")
            .args(["clone", bare.to_str().unwrap(), main_dir.to_str().unwrap()])
            .output()
            .unwrap();

        // Create an initial commit so branches can be created
        fs::write(main_dir.join("README.md"), "# test").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&main_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args([
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(&main_dir)
            .output()
            .unwrap();

        let project = tmp.join("project");

        // Create .bork dir with config and state
        let bork_dir = project.join(".bork");
        fs::create_dir_all(&bork_dir).unwrap();
        fs::write(
            bork_dir.join("config.toml"),
            "project_name = \"bork\"\nagent_kind = \"opencode\"\n",
        )
        .unwrap();
        fs::write(
            bork_dir.join("state.json"),
            r#"{"issues": [{"id": "bork-1", "title": "Test issue", "column": "InProgress", "tmux_session": null, "agent_kind": "OpenCode", "agent_mode": "Plan", "agent_status": "Stopped", "prompt": null, "worktree": null, "done_at": null}]}"#,
        )
        .unwrap();

        let cfg = config::AppConfig {
            project_name: "bork".into(),
            project_root: project.clone(),
            agent_kind: crate::types::AgentKind::OpenCode,
            default_prompt: None,
            done_session_ttl: 300,
            debug: false,
        };

        (tmp, project, cfg)
    }

    #[test]
    fn test_worktree_creates_dir_and_updates_state() {
        let (tmp, project, cfg) = setup_test_project();

        let result = run_worktree_in(&cfg, "bork-1", Some("fix-bug"), None);
        assert!(result.is_ok(), "run_worktree failed: {:?}", result.err());

        assert!(project.join("bork-1-fix-bug").exists());
        assert!(project.join("bork-1-fix-bug/.git").exists());

        let state = config::load_state(&project);
        let issue = state.issues.iter().find(|i| i.id == "bork-1").unwrap();
        assert_eq!(issue.worktree, Some("bork-1-fix-bug".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worktree_creates_issue_with_title() {
        let (tmp, _project, cfg) = setup_test_project();

        let result = run_worktree_in(&cfg, "bork-2", Some("new-feature"), Some("New feature"));
        assert!(result.is_ok(), "run_worktree failed: {:?}", result.err());

        let state = config::load_state(&cfg.project_root);
        let issue = state.issues.iter().find(|i| i.id == "bork-2").unwrap();
        assert_eq!(issue.title, "New feature");
        assert_eq!(issue.worktree, Some("bork-2-new-feature".to_string()));
        assert_eq!(issue.column, Column::Todo);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worktree_fails_if_dir_exists() {
        let (tmp, project, cfg) = setup_test_project();

        fs::create_dir_all(project.join("bork-1-fix-bug")).unwrap();

        let result = run_worktree_in(&cfg, "bork-1", Some("fix-bug"), None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("already exists"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worktree_without_slug_uses_id_as_branch() {
        let (tmp, project, cfg) = setup_test_project();

        let result = run_worktree_in(&cfg, "bork-1", None, None);
        assert!(result.is_ok(), "run_worktree failed: {:?}", result.err());

        let output = Command::new("git")
            .args(["branch", "--list", "bork-1"])
            .current_dir(project.join("bork-1"))
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(
            branches.contains("bork-1"),
            "Branch 'bork-1' should exist, got: {}",
            branches
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
