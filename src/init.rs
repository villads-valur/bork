use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};

use crate::types::AgentKind;

const WORKTREE_SKILL: &str = r#"---
name: worktree
description: Create git worktrees and register them with bork for issue tracking
---

# Worktree Management

Create and manage git worktrees in a bork project.

## Context

This is a bork project. The directory layout is:

```
project/                    # container root (this is where you are)
├── main/                   # main branch worktree (owns .git/)
├── {issue-id}-{slug}/      # issue worktrees (siblings of main/)
├── .bork/                  # bork state (do not modify directly)
├── AGENTS.md               # project instructions
└── opencode.jsonc           # opencode config
```

The `main/` directory is a regular git repo. All worktrees are created from it and live as sibling directories inside this container.

## Creating a worktree

**Always use `bork worktree` to create worktrees.** This creates the git worktree AND registers it with bork's state so the TUI can track it.

```bash
bork worktree {issue-id} {slug}
```

**Example:**
```bash
bork worktree bork-14 add-worktree-support
```

This creates:
- Directory: `bork-14-add-worktree-support/` (sibling of `main/`)
- Branch: `bork-14/add-worktree-support` (branched from current HEAD of main)
- Updates `.bork/state.json` to link the issue to the worktree

To also create the issue if it doesn't exist:
```bash
bork worktree bork-14 add-worktree-support --title "Add worktree support"
```

## Naming conventions

- **Worktree directory**: `{issue-id}-{slug}` when a slug is provided (e.g. `bork-14-add-worktree-support`), or just `{issue-id}` without a slug
- **Branch name**: `{issue-id}/{kebab-case-slug}` (e.g. `bork-14/add-worktree-support`)
- Issue IDs follow the pattern `{project-name}-{number}` (e.g. `bork-1`, `bork-14`)

## Linear issues

When an issue was imported from Linear, the bork issue ID is the Linear identifier in lowercase (e.g. `vil-123` for Linear issue `VIL-123`). The worktree follows the same naming pattern:

```bash
bork worktree vil-123 fix-auth-flow
```

This creates:
- Directory: `vil-123-fix-auth-flow/`
- Branch: `vil-123/fix-auth-flow`

When Linear is not used, issue IDs are regular bork IDs (e.g. `bork-14`).

## After creating a worktree

1. Create a planning file:
   ```bash
   mkdir -p {worktree-dir}/.claude
   ```
   Then create `{worktree-dir}/.claude/planning.md` with the task plan.

2. Do all work for the issue inside the worktree directory, not in `main/`.

## Listing worktrees

```bash
git -C main worktree list
```

## Removing a worktree

When work is done and merged:

```bash
git -C main worktree remove ../{worktree-dir}
```

Or forcefully if there are uncommitted changes:

```bash
git -C main worktree remove --force ../{worktree-dir}
```

## Important

- Never commit directly to `main` from a worktree. Always use feature branches.
- The `main/` worktree should stay on the `main` branch.
- Multiple worktrees can exist simultaneously for parallel work streams.
- Each worktree is a full checkout sharing the same git objects (disk efficient).
"#;

const AGENTS_MD_TEMPLATE: &str = r#"# {project_name}

Managed with bork across git worktrees and tmux.

## Project Layout

```
{project_name}/                     # Container directory (NOT a git repo)
├── .bork/                          # Bork state (config.toml, state.json)
├── AGENTS.md
├── opencode.jsonc
├── main/                           # Main branch worktree (git repo)
│   └── CLAUDE.md                   # Project instructions
└── {issue-id}-{slug}/              # Issue worktrees
```

## Worktree Conventions

- Use `bork worktree <issue-id> <slug>` to create worktrees
- Worktree directory: `{issue-id}-{slug}` (e.g. `{project_name}-1-fix-bug`)
- Branch: `{issue-id}/{slug}` (e.g. `{project_name}-1/fix-bug`)
- Issue IDs: `{project_name}-{number}`
- Linear-imported issue IDs: lowercase Linear identifier (e.g. `abc-123` for `ABC-123`)
- Tmux sessions: `{project_name}-{issue-id}`
"#;

const CLAUDE_MD_TEMPLATE: &str = r#"# {project_name}

## Build & Run

```bash
# TODO: Add build commands
```

## Testing

```bash
# TODO: Add test commands
```
"#;

const OPENCODE_JSONC: &str = r#"{
  "$schema": "https://opencode.ai/config.json"
}
"#;

/// Normalize a repo argument into a clone-able URL.
///
/// Supports:
/// - `owner/repo` -> `https://github.com/owner/repo.git`
/// - Full HTTPS URLs (passthrough)
/// - SSH URLs like `git@github.com:owner/repo.git` (passthrough)
pub fn normalize_repo_url(repo: &str) -> String {
    if repo.contains("://")
        || repo.starts_with("git@")
        || repo.starts_with('/')
        || repo.starts_with('.')
    {
        return repo.to_string();
    }
    format!("https://github.com/{}.git", repo)
}

/// Extract the repository name from any supported repo format.
///
/// Strips `.git` suffix and takes the last path/segment.
pub fn extract_repo_name(repo: &str) -> anyhow::Result<String> {
    let path_part = if let Some(rest) = repo.strip_prefix("git@") {
        // git@github.com:owner/repo.git -> owner/repo.git
        rest.rsplit(':').next().unwrap_or("")
    } else if repo.contains("://") {
        // https://github.com/owner/repo.git -> repo.git
        repo.split("://")
            .nth(1)
            .and_then(|s| s.rsplit('/').next())
            .unwrap_or("")
    } else if repo.starts_with('/') || repo.starts_with('.') {
        // Local path: /path/to/repo.git or ./repo -> last segment
        repo.trim_end_matches('/').rsplit('/').next().unwrap_or("")
    } else if repo.contains('/') {
        // owner/repo -> repo
        repo.rsplit('/').next().unwrap_or("")
    } else {
        repo
    };

    let name = path_part.strip_suffix(".git").unwrap_or(path_part);

    // For SSH URLs, take just the last segment (repo name)
    let name = name.rsplit('/').next().unwrap_or(name);

    if name.is_empty() {
        bail!("Could not extract repository name from: {}", repo);
    }

    Ok(name.to_string())
}

/// Initialize a new bork project by cloning a git repo and scaffolding the
/// `.bork/` directory structure.
///
/// `work_dir` overrides the working directory (defaults to cwd). This exists
/// so tests can run in a temp directory without side effects.
pub fn run_init(
    repo: &str,
    directory: Option<&str>,
    agent_kind: AgentKind,
    work_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let cwd = match work_dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().context("Failed to get current directory")?,
    };

    let repo_name = extract_repo_name(repo)?;
    let dir_name = directory.unwrap_or(&repo_name);
    let container = cwd.join(dir_name);

    if container.exists() {
        bail!(
            "Directory '{}' already exists. Remove it first or choose a different name.",
            dir_name
        );
    }

    let clone_url = normalize_repo_url(repo);
    let main_dir = container.join("main");

    // Clone the repository into <container>/main with live progress output
    println!("Cloning {} into {}/main ...", clone_url, dir_name);
    let status = Command::new("git")
        .args([
            "clone",
            "--progress",
            &clone_url,
            main_dir.to_str().unwrap_or("main"),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed (exit code: {})", status);
    }

    // Create .bork/ directory structure
    let bork_dir = container.join(".bork");
    fs::create_dir_all(bork_dir.join("agent-status"))
        .context("Failed to create .bork/agent-status directory")?;

    // Write config.toml
    let config_content = format!(
        "project_name = \"{}\"\nagent_kind = \"{}\"\n",
        dir_name, agent_kind,
    );
    fs::write(bork_dir.join("config.toml"), config_content)
        .context("Failed to write config.toml")?;

    // Write empty state.json
    let state_content = serde_json::to_string_pretty(&serde_json::json!({
        "issues": []
    }))?;
    fs::write(bork_dir.join("state.json"), state_content).context("Failed to write state.json")?;

    // Scaffold opencode.jsonc
    fs::write(container.join("opencode.jsonc"), OPENCODE_JSONC)
        .context("Failed to write opencode.jsonc")?;

    // Scaffold AGENTS.md at container root (unless one already exists)
    let agents_path = container.join("AGENTS.md");
    if !agents_path.exists() {
        let agents_content = AGENTS_MD_TEMPLATE.replace("{project_name}", dir_name);
        fs::write(&agents_path, agents_content).context("Failed to write AGENTS.md")?;
    }

    // Scaffold CLAUDE.md inside the git repo (unless one already exists)
    let claude_path = main_dir.join("CLAUDE.md");
    if !claude_path.exists() {
        let claude_content = CLAUDE_MD_TEMPLATE.replace("{project_name}", dir_name);
        fs::write(&claude_path, claude_content).context("Failed to write CLAUDE.md")?;
    }

    // Scaffold worktree skill for Claude Code
    let skill_dir = container.join(".claude/skills/worktree");
    fs::create_dir_all(&skill_dir).context("Failed to create .claude/skills/worktree")?;
    fs::write(skill_dir.join("SKILL.md"), WORKTREE_SKILL)
        .context("Failed to write worktree SKILL.md")?;

    // Install agent hooks
    println!("Installing agent hooks...");
    if let Err(e) = crate::external::hooks::install() {
        eprintln!("Warning: hook installation failed: {}", e);
        eprintln!("You can run 'bork install' later to set up hooks.");
    }

    println!();
    println!("Initialized bork project in ./{}/", dir_name);
    println!();
    println!("Next steps:");
    println!("  cd {}", dir_name);
    println!("  bork");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_repo_url ---

    #[test]
    fn normalize_github_shorthand() {
        assert_eq!(
            normalize_repo_url("villads-valur/bork"),
            "https://github.com/villads-valur/bork.git"
        );
    }

    #[test]
    fn normalize_shorthand_with_dashes() {
        assert_eq!(
            normalize_repo_url("my-org/my-cool-project"),
            "https://github.com/my-org/my-cool-project.git"
        );
    }

    #[test]
    fn normalize_https_url_passthrough() {
        let url = "https://github.com/owner/repo.git";
        assert_eq!(normalize_repo_url(url), url);
    }

    #[test]
    fn normalize_https_url_without_git_suffix() {
        let url = "https://github.com/owner/repo";
        assert_eq!(normalize_repo_url(url), url);
    }

    #[test]
    fn normalize_ssh_url_passthrough() {
        let url = "git@github.com:owner/repo.git";
        assert_eq!(normalize_repo_url(url), url);
    }

    #[test]
    fn normalize_gitlab_https_passthrough() {
        let url = "https://gitlab.com/owner/repo.git";
        assert_eq!(normalize_repo_url(url), url);
    }

    #[test]
    fn normalize_absolute_path_passthrough() {
        let path = "/tmp/my-repo.git";
        assert_eq!(normalize_repo_url(path), path);
    }

    #[test]
    fn normalize_relative_path_passthrough() {
        let path = "./my-repo";
        assert_eq!(normalize_repo_url(path), path);
    }

    // --- extract_repo_name ---

    #[test]
    fn extract_name_from_shorthand() {
        assert_eq!(extract_repo_name("owner/repo").unwrap(), "repo");
    }

    #[test]
    fn extract_name_from_shorthand_with_dashes() {
        assert_eq!(
            extract_repo_name("my-org/my-project").unwrap(),
            "my-project"
        );
    }

    #[test]
    fn extract_name_from_https_with_git_suffix() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/my-project.git").unwrap(),
            "my-project"
        );
    }

    #[test]
    fn extract_name_from_https_without_git_suffix() {
        assert_eq!(
            extract_repo_name("https://github.com/owner/my-project").unwrap(),
            "my-project"
        );
    }

    #[test]
    fn extract_name_from_ssh() {
        assert_eq!(
            extract_repo_name("git@github.com:owner/repo.git").unwrap(),
            "repo"
        );
    }

    #[test]
    fn extract_name_from_ssh_without_git_suffix() {
        assert_eq!(
            extract_repo_name("git@github.com:owner/repo").unwrap(),
            "repo"
        );
    }

    #[test]
    fn extract_name_from_absolute_path() {
        assert_eq!(
            extract_repo_name("/tmp/cool-project.git").unwrap(),
            "cool-project"
        );
    }

    #[test]
    fn extract_name_from_relative_path() {
        assert_eq!(extract_repo_name("./my-repo").unwrap(), "my-repo");
    }

    #[test]
    fn extract_name_empty_input_fails() {
        assert!(extract_repo_name("").is_err());
    }

    // --- run_init integration tests ---

    #[test]
    fn init_creates_bork_structure() {
        let tmp = std::env::temp_dir().join(format!("bork-test-init-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Create a local bare repo so we don't need network access
        let bare_repo = tmp.join("test-repo.git");
        fs::create_dir_all(&bare_repo).unwrap();
        let output = Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&bare_repo)
            .output()
            .unwrap();
        assert!(output.status.success(), "Failed to create bare repo");

        let result = run_init(
            bare_repo.to_str().unwrap(),
            Some("my-project"),
            AgentKind::OpenCode,
            Some(&tmp),
        );
        assert!(result.is_ok(), "run_init failed: {:?}", result.err());

        let container = tmp.join("my-project");

        // Verify directory structure
        assert!(container.join("main").exists(), "main/ should exist");
        assert!(
            container.join("main/.git").exists(),
            "main/.git should exist"
        );
        assert!(
            container.join(".bork/config.toml").exists(),
            "config.toml should exist"
        );
        assert!(
            container.join(".bork/state.json").exists(),
            "state.json should exist"
        );
        assert!(
            container.join(".bork/agent-status").is_dir(),
            "agent-status/ should be a directory"
        );

        // Verify config content
        let config = fs::read_to_string(container.join(".bork/config.toml")).unwrap();
        assert!(
            config.contains("project_name = \"my-project\""),
            "config should contain project name"
        );
        assert!(
            config.contains("agent_kind = \"opencode\""),
            "config should contain agent kind"
        );

        // Verify state is empty
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(container.join(".bork/state.json")).unwrap())
                .unwrap();
        assert_eq!(state["issues"], serde_json::json!([]));

        // Verify opencode.jsonc
        assert!(
            container.join("opencode.jsonc").exists(),
            "opencode.jsonc should exist"
        );
        let oc = fs::read_to_string(container.join("opencode.jsonc")).unwrap();
        assert!(oc.contains("opencode.ai/config.json"));

        // Verify worktree skill
        assert!(
            container.join(".claude/skills/worktree/SKILL.md").exists(),
            "worktree SKILL.md should exist"
        );
        let skill = fs::read_to_string(container.join(".claude/skills/worktree/SKILL.md")).unwrap();
        assert!(skill.contains("git worktree"));

        // Verify AGENTS.md at container root
        assert!(
            container.join("AGENTS.md").exists(),
            "AGENTS.md should exist"
        );
        let agents = fs::read_to_string(container.join("AGENTS.md")).unwrap();
        assert!(
            agents.contains("my-project"),
            "AGENTS.md should contain project name"
        );
        assert!(
            agents.contains("bork worktree"),
            "AGENTS.md should mention bork worktree"
        );

        // Verify CLAUDE.md inside git repo
        assert!(
            container.join("main/CLAUDE.md").exists(),
            "CLAUDE.md should exist in main/"
        );
        let claude = fs::read_to_string(container.join("main/CLAUDE.md")).unwrap();
        assert!(
            claude.contains("my-project"),
            "CLAUDE.md should contain project name"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn init_with_claude_agent() {
        let tmp = std::env::temp_dir().join(format!("bork-test-claude-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let bare_repo = tmp.join("test-repo.git");
        fs::create_dir_all(&bare_repo).unwrap();
        Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&bare_repo)
            .output()
            .unwrap();

        let result = run_init(
            bare_repo.to_str().unwrap(),
            Some("claude-project"),
            AgentKind::Claude,
            Some(&tmp),
        );
        assert!(result.is_ok(), "run_init failed: {:?}", result.err());

        let config = fs::read_to_string(tmp.join("claude-project/.bork/config.toml")).unwrap();
        assert!(config.contains("agent_kind = \"claude\""));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn init_defaults_directory_to_repo_name() {
        let tmp =
            std::env::temp_dir().join(format!("bork-test-default-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let bare_repo = tmp.join("cool-project.git");
        fs::create_dir_all(&bare_repo).unwrap();
        Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&bare_repo)
            .output()
            .unwrap();

        // Pass the bare repo path as the repo arg (no directory override)
        let result = run_init(
            bare_repo.to_str().unwrap(),
            None,
            AgentKind::OpenCode,
            Some(&tmp),
        );
        assert!(result.is_ok(), "run_init failed: {:?}", result.err());

        // Should have created a directory named "cool-project" (stripped .git)
        assert!(
            tmp.join("cool-project/main").exists(),
            "Should create directory from repo name"
        );
        assert!(tmp.join("cool-project/.bork/config.toml").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn init_fails_if_directory_exists() {
        let tmp = std::env::temp_dir().join(format!("bork-test-exists-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("existing")).unwrap();

        let result = run_init(
            "owner/repo",
            Some("existing"),
            AgentKind::OpenCode,
            Some(&tmp),
        );

        assert!(result.is_err(), "Should fail when directory exists");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("already exists"),
            "Error should mention directory exists, got: {}",
            err
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn init_fails_with_bad_clone_url() {
        let tmp = std::env::temp_dir().join(format!("bork-test-bad-url-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let result = run_init(
            "https://github.com/nonexistent/repo-that-does-not-exist-12345.git",
            Some("bad-clone"),
            AgentKind::OpenCode,
            Some(&tmp),
        );

        assert!(result.is_err(), "Should fail with bad clone URL");

        // Container dir should not be left behind on clone failure
        // (git clone creates and cleans up on failure)

        let _ = fs::remove_dir_all(&tmp);
    }
}
