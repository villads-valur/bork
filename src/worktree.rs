use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};

use crate::config;
use crate::types::{AgentMode, Column, Issue, IssueKind};

/// Create a git worktree and register it with bork's state.json.
pub fn run_worktree(
    issue_id: &str,
    slug: Option<&str>,
    title: Option<&str>,
    base_branch: Option<&str>,
) -> anyhow::Result<()> {
    let config = config::load_config();
    let result = create_worktree_in(&config, issue_id, slug, title, base_branch)?;

    println!(
        "Created worktree: {}/ on branch {}",
        result.worktree_dir, result.branch_name
    );

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeResult {
    pub worktree_dir: String,
    pub branch_name: String,
}

const SLUG_MAX_LEN: usize = 48;

pub fn slugify_title(title: &str) -> String {
    let mut slug = String::new();

    for ch in title.chars().flat_map(char::to_lowercase) {
        if slug.len() >= SLUG_MAX_LEN {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    if slug.is_empty() {
        "issue".to_string()
    } else {
        slug
    }
}

/// Best-effort `git fetch origin` so new branches (and `--base origin/xyz`)
/// resolve against fresh remote refs. Failure (offline, no remote) is
/// non-fatal and must never block worktree creation.
fn fetch_origin(main_dir: &Path) -> bool {
    Command::new("git")
        .args(["fetch", "origin", "--quiet"])
        .current_dir(main_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a git command in `dir` and return its trimmed stdout, or `None` if
/// the command failed.
fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Count commits the current branch is behind its upstream. Returns the
/// upstream name and count, or `None` if there is no upstream or git fails.
fn commits_behind_upstream(main_dir: &Path) -> Option<(String, u32)> {
    let upstream = git_stdout(main_dir, &["rev-parse", "--abbrev-ref", "@{upstream}"])
        .filter(|name| !name.is_empty())?;
    let behind = git_stdout(main_dir, &["rev-list", "HEAD..@{upstream}", "--count"])?
        .parse()
        .ok()?;
    Some((upstream, behind))
}

pub fn create_worktree_in(
    config: &config::AppConfig,
    issue_id: &str,
    slug: Option<&str>,
    title: Option<&str>,
    base_branch: Option<&str>,
) -> anyhow::Result<WorktreeResult> {
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

    // Refresh remote refs before branching so the new worktree isn't based on
    // a stale view of the remote. Only warn about being behind when branching
    // off main/'s HEAD (the default); a custom --base picks its own start.
    if fetch_origin(&main_dir) && base_branch.is_none() {
        let behind_upstream = commits_behind_upstream(&main_dir).filter(|(_, count)| *count > 0);
        if let Some((upstream, behind)) = behind_upstream {
            println!(
                "Warning: main is {} commit(s) behind {}. \
                 New branch will be based on the local HEAD. \
                 Run 'git pull' in main/ to sync first.",
                behind, upstream
            );
        }
    }

    let branch_name = match slug {
        Some(s) => format!("{}/{}", issue_id, s),
        None => issue_id.to_string(),
    };

    let worktree_path = format!("../{}", worktree_dir);
    let mut args = vec!["worktree", "add", &worktree_path, "-b", &branch_name];
    // When a base branch is given, pass it as the start-point so the new branch
    // is created from it. Without one, git branches off main/'s current HEAD
    // (the main/master branch by bork convention).
    if let Some(base) = base_branch {
        args.push(base);
    }

    let status = Command::new("git")
        .args(&args)
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

    Ok(WorktreeResult {
        worktree_dir,
        branch_name,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(["-c", "user.email=test@test.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
    }

    /// Clone the test project's bare repo into `{tmp}/other` and return its path.
    fn clone_bare_to_other(tmp: &Path) -> std::path::PathBuf {
        let other = tmp.join("other");
        Command::new("git")
            .args([
                "clone",
                tmp.join("bare.git").to_str().unwrap(),
                other.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        other
    }

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
        git_in(&main_dir, &["add", "."]);
        git_in(&main_dir, &["commit", "-m", "init"]);

        let project = tmp.join("project");
        write_bork_files(&project);

        let cfg = test_config(&project);

        (tmp, project, cfg)
    }

    fn write_bork_files(project: &Path) {
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
    }

    fn test_config(project: &Path) -> config::AppConfig {
        config::AppConfig {
            project_name: "bork".into(),
            project_root: project.to_path_buf(),
            agent_kind: crate::types::AgentKind::OpenCode,
            default_prompt: None,
            review_prompt: None,
            done_session_ttl: 300,
            debug: false,
            auto_import_reviews: true,
            auto_import_authored_prs: true,
            agents_allowlist: None,
            agent_launch: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_worktree_creates_dir_and_updates_state() {
        let (tmp, project, cfg) = setup_test_project();

        let result = create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None);
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

        let result = create_worktree_in(
            &cfg,
            "bork-2",
            Some("new-feature"),
            Some("New feature"),
            None,
        );
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

        let result = create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("already exists"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worktree_without_slug_uses_id_as_branch() {
        let (tmp, project, cfg) = setup_test_project();

        let result = create_worktree_in(&cfg, "bork-1", None, None, None);
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

    #[test]
    fn test_worktree_bases_new_branch_on_given_branch() {
        let (tmp, project, cfg) = setup_test_project();
        let main_dir = project.join("main");

        // Create a base branch with a distinct commit that doesn't exist on the
        // default branch, so we can prove the new worktree was based on it.
        git_in(&main_dir, &["checkout", "-b", "feature-base"]);
        fs::write(main_dir.join("BASE_MARKER.md"), "marker").unwrap();
        git_in(&main_dir, &["add", "."]);
        git_in(&main_dir, &["commit", "-m", "base marker"]);
        // Switch main back off the base branch so HEAD differs from it.
        git_in(&main_dir, &["checkout", "-"]);

        let result =
            create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, Some("feature-base"));
        assert!(result.is_ok(), "create failed: {:?}", result.err());

        // The new worktree should contain the marker file only present on the base.
        assert!(project.join("bork-1-fix-bug/BASE_MARKER.md").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worktree_succeeds_when_main_is_behind_origin() {
        let (tmp, project, cfg) = setup_test_project();
        let main_dir = project.join("main");

        // Publish main's branch, then advance origin from a second clone so
        // main/ is one commit behind after the fetch.
        git_in(&main_dir, &["push", "-u", "origin", "HEAD"]);
        let other = clone_bare_to_other(&tmp);
        fs::write(other.join("AHEAD.md"), "ahead").unwrap();
        git_in(&other, &["add", "."]);
        git_in(&other, &["commit", "-m", "ahead of local main"]);
        git_in(&other, &["push", "origin", "HEAD"]);

        // Behind-state must only warn, never block worktree creation.
        let result = create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None);
        assert!(result.is_ok(), "create failed: {:?}", result.err());
        assert!(project.join("bork-1-fix-bug").exists());
        // Default base is still local HEAD, not origin: remote-only file absent.
        assert!(!project.join("bork-1-fix-bug/AHEAD.md").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worktree_succeeds_without_remote() {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!(
            "bork-wt-test-noremote-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&tmp);
        let project = tmp.join("project");
        let main_dir = project.join("main");
        fs::create_dir_all(&main_dir).unwrap();

        // Plain git init, no origin: the fetch must fail silently.
        git_in(&main_dir, &["init"]);
        fs::write(main_dir.join("README.md"), "# test").unwrap();
        git_in(&main_dir, &["add", "."]);
        git_in(&main_dir, &["commit", "-m", "init"]);
        write_bork_files(&project);
        let cfg = test_config(&project);

        let result = create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None);
        assert!(result.is_ok(), "create failed: {:?}", result.err());
        assert!(project.join("bork-1-fix-bug").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_fetch_makes_origin_base_resolvable() {
        let (tmp, project, cfg) = setup_test_project();

        // Push a branch to origin from a second clone. main/ has never
        // fetched it, so --base origin/feature-remote only resolves if
        // create_worktree_in fetches first.
        let other = clone_bare_to_other(&tmp);
        git_in(&other, &["checkout", "-b", "feature-remote"]);
        fs::write(other.join("REMOTE_MARKER.md"), "marker").unwrap();
        git_in(&other, &["add", "."]);
        git_in(&other, &["commit", "-m", "remote marker"]);
        git_in(&other, &["push", "origin", "feature-remote"]);

        let result = create_worktree_in(
            &cfg,
            "bork-1",
            Some("fix-bug"),
            None,
            Some("origin/feature-remote"),
        );
        assert!(result.is_ok(), "create failed: {:?}", result.err());
        assert!(project.join("bork-1-fix-bug/REMOTE_MARKER.md").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn slugify_title_uses_kebab_case() {
        assert_eq!(slugify_title("Add agent spawning!"), "add-agent-spawning");
    }

    #[test]
    fn slugify_title_collapses_separators() {
        assert_eq!(slugify_title("Fix: auth/API bug"), "fix-auth-api-bug");
    }

    #[test]
    fn slugify_title_falls_back_for_empty_slug() {
        assert_eq!(slugify_title("!!!"), "issue");
    }
}
