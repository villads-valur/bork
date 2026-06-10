use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};

use crate::config;
use crate::types::{Column, Issue, IssueKind};

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

    if state
        .issues
        .iter()
        .any(|i| i.id.eq_ignore_ascii_case(issue_id) && i.kind == IssueKind::Orchestrator)
    {
        bail!(
            "'{}' is an orchestrator issue; orchestrators run at the project root and have no worktree",
            issue_id
        );
    }

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

    // Match case-insensitively: stored ids are lowercase but users may type
    // e.g. `bork worktree BORK-1` or a Linear identifier like `VIL-123`.
    if let Some(issue) = state
        .issues
        .iter_mut()
        .find(|i| i.id.eq_ignore_ascii_case(issue_id))
    {
        issue.worktree = Some(worktree_dir.to_string());
    } else if let Some(title) = title {
        let issue = Issue {
            worktree: Some(worktree_dir.to_string()),
            ..Issue::new(issue_id, title, Column::Todo, config.agent_kind)
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

/// Remove an issue worktree: run the configured teardown script inside it,
/// then `git worktree remove` it from main/.
///
/// Teardown failure aborts the removal unless `force` is set. `force` is also
/// passed through to git, so worktrees with uncommitted changes are removed.
/// A missing worktree directory is not an error; stale git metadata is pruned.
pub fn remove_worktree_in(
    config: &config::AppConfig,
    worktree_dir: &str,
    force: bool,
) -> anyhow::Result<()> {
    let main_dir = config.project_root.join("main");
    if !main_dir.join(".git").exists() {
        bail!(
            "No git repo found at {}/main. Are you in a bork project?",
            config.project_root.display()
        );
    }

    let worktree_path = config.project_root.join(worktree_dir);
    if !worktree_path.exists() {
        // Directory already gone; clean up any stale worktree metadata.
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&main_dir)
            .status();
        return Ok(());
    }

    if let Some(script) = config.teardown_script.as_deref() {
        let status = Command::new("sh")
            .args(["-c", script])
            .current_dir(&worktree_path)
            .status()
            .context("Failed to run teardown_script")?;
        if !status.success() && !force {
            bail!(
                "teardown_script failed (exit {}). Fix it or re-run with --force to remove anyway.",
                status.code().unwrap_or(-1)
            );
        }
    }

    let worktree_arg = format!("../{}", worktree_dir);
    let mut args = vec!["worktree", "remove", worktree_arg.as_str()];
    if force {
        args.push("--force");
    }

    let status = Command::new("git")
        .args(&args)
        .current_dir(&main_dir)
        .status()
        .context("Failed to run git worktree remove")?;

    if !status.success() {
        bail!(
            "git worktree remove failed. The worktree may have uncommitted changes; re-run with --force to discard them."
        );
    }

    Ok(())
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
            orchestrator_prompt: None,
            setup_script: None,
            teardown_script: None,
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
    fn test_worktree_rejects_orchestrator_issue() {
        let (tmp, project, cfg) = setup_test_project();

        let mut state = config::load_state(&project);
        state.issues[0].kind = crate::types::IssueKind::Orchestrator;
        config::save_state(&state, &project).unwrap();

        let result = create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("orchestrator"), "unexpected error: {}", err);
        assert!(!project.join("bork-1-fix-bug").exists());

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
    fn test_remove_worktree_removes_dir() {
        let (tmp, project, cfg) = setup_test_project();

        create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None).unwrap();
        assert!(project.join("bork-1-fix-bug").exists());

        let result = remove_worktree_in(&cfg, "bork-1-fix-bug", false);
        assert!(result.is_ok(), "remove failed: {:?}", result.err());
        assert!(!project.join("bork-1-fix-bug").exists());

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
    fn test_remove_worktree_runs_teardown_script() {
        let (tmp, project, mut cfg) = setup_test_project();
        let marker = tmp.join("teardown-ran");
        cfg.teardown_script = Some(format!("touch '{}'", marker.display()));

        create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None).unwrap();
        remove_worktree_in(&cfg, "bork-1-fix-bug", false).unwrap();

        assert!(marker.exists(), "teardown_script should have run");
        assert!(!project.join("bork-1-fix-bug").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_remove_worktree_teardown_failure_aborts() {
        let (tmp, project, mut cfg) = setup_test_project();
        cfg.teardown_script = Some("exit 1".to_string());

        create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None).unwrap();

        let result = remove_worktree_in(&cfg, "bork-1-fix-bug", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("teardown_script"));
        assert!(
            project.join("bork-1-fix-bug").exists(),
            "worktree should survive a failed teardown"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_remove_worktree_teardown_failure_force_removes() {
        let (tmp, project, mut cfg) = setup_test_project();
        cfg.teardown_script = Some("exit 1".to_string());

        create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None).unwrap();

        let result = remove_worktree_in(&cfg, "bork-1-fix-bug", true);
        assert!(result.is_ok(), "force remove failed: {:?}", result.err());
        assert!(!project.join("bork-1-fix-bug").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_remove_worktree_dirty_requires_force() {
        let (tmp, project, cfg) = setup_test_project();

        create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None).unwrap();
        fs::write(project.join("bork-1-fix-bug/dirty.txt"), "uncommitted").unwrap();

        let without_force = remove_worktree_in(&cfg, "bork-1-fix-bug", false);
        assert!(without_force.is_err());
        assert!(project.join("bork-1-fix-bug").exists());

        let with_force = remove_worktree_in(&cfg, "bork-1-fix-bug", true);
        assert!(with_force.is_ok(), "force failed: {:?}", with_force.err());
        assert!(!project.join("bork-1-fix-bug").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_archive_issue_end_to_end() {
        let (tmp, project, cfg) = setup_test_project();
        let marker = tmp.join("teardown-ran");

        // archive_issue loads config from disk. Use a unique project name so
        // the derived tmux session name can never collide with (and kill) a
        // real running session.
        fs::write(
            project.join(".bork/config.toml"),
            format!(
                "project_name = \"bork-wt-test-{}\"\nagent_kind = \"opencode\"\nteardown_script = \"touch '{}'\"\n",
                std::process::id(),
                marker.display()
            ),
        )
        .unwrap();

        create_worktree_in(&cfg, "bork-1", Some("fix-bug"), None, None).unwrap();
        assert!(project.join("bork-1-fix-bug").exists());

        let report = crate::ops::archive_issue(&project, "bork-1", false).unwrap();
        assert_eq!(report.issue_id, "bork-1");
        assert_eq!(report.worktree_removed.as_deref(), Some("bork-1-fix-bug"));
        assert!(!project.join("bork-1-fix-bug").exists());
        assert!(marker.exists(), "teardown_script should have run");

        let state = config::load_state(&project);
        let issue = state.issues.iter().find(|i| i.id == "bork-1").unwrap();
        assert_eq!(issue.column, crate::types::Column::Done);
        assert!(issue.worktree.is_none());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_remove_worktree_missing_dir_is_ok() {
        let (tmp, _project, cfg) = setup_test_project();

        let result = remove_worktree_in(&cfg, "bork-9-never-existed", false);
        assert!(result.is_ok());

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
