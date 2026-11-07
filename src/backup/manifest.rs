//! `manifest.json` at the top of a backup archive: what the archive holds and
//! what `bork import` needs to put it back (where each project lived, how to
//! clone its repo, which issues it had).
//!
//! These types are the archive's public contract. Add fields with
//! `#[serde(default)]` so older archives keep loading; bump
//! [`MANIFEST_VERSION`] only for changes an older reader can't handle.
//!
//! Archive layout (all paths relative to the archive root):
//!
//! ```text
//! manifest.json
//! projects/<archive_dir>/.bork/state.json
//! projects/<archive_dir>/.bork/config.toml
//! projects/<archive_dir>/AGENTS.md, plans/, .claude/, ...   container files
//! projects/<archive_dir>/_issues/<issue-id>/planning.md      live worktree plans
//! _global/projects.json
//! _global/config.toml
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::AppState;
use crate::types::Column;

pub const MANIFEST_VERSION: u32 = 1;
pub const MANIFEST_FILE: &str = "manifest.json";
/// Archive directory holding one subdirectory per project.
pub const PROJECTS_DIR: &str = "projects";
/// Archive directory holding the global config files.
pub const GLOBAL_DIR: &str = "_global";

/// Top-level description of a backup archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version, see [`MANIFEST_VERSION`].
    pub version: u32,
    /// Unix seconds when the archive was written.
    pub created_at: u64,
    /// Version of the bork that wrote it (`CARGO_PKG_VERSION`).
    pub bork_version: String,
    pub projects: Vec<ProjectManifest>,
    /// Archive paths of the global files (under `_global/`), empty for a
    /// single-project archive.
    #[serde(default)]
    pub global_files: Vec<String>,
}

/// One backed-up project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectManifest {
    /// `project_name` from the project's config (issue id prefix).
    pub name: String,
    /// Sidebar label from `projects.json`, if renamed.
    #[serde(default)]
    pub label: Option<String>,
    /// Absolute container path at backup time; the default restore location.
    pub path: PathBuf,
    /// Directory under `projects/` holding this project's files. Usually
    /// `name`, with a suffix when two registered projects share a name.
    pub archive_dir: String,
    /// The repo checked out in `main/`.
    pub repo: RepoManifest,
    /// Files stored for this project, relative to `projects/<archive_dir>/`.
    pub files: Vec<String>,
    /// Every issue on the board (Done included). The full data is in the
    /// archived `.bork/state.json`; this is the summary import needs.
    pub issues: Vec<IssueManifest>,
}

/// How to get the project's code back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoManifest {
    /// URL to clone: `origin`'s fetch URL, else the first remote's.
    pub clone_url: Option<String>,
    pub remotes: Vec<Remote>,
    /// Remote default branch (`origin/HEAD`), else the branch checked out in `main/`.
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    /// Fetch URL.
    pub url: String,
}

/// One issue as it was on the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueManifest {
    pub id: String,
    pub title: String,
    pub column: Column,
    /// Worktree directory name under the container, if the issue had one.
    #[serde(default)]
    pub worktree: Option<String>,
    /// Branch checked out in that worktree at backup time.
    #[serde(default)]
    pub branch: Option<String>,
    /// Archive path of the issue's plan (`projects/<dir>/_issues/<id>/planning.md`),
    /// when its worktree had one.
    #[serde(default)]
    pub plan: Option<String>,
}

/// Git facts about a project's `main/` repo, gathered once per backup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoFacts {
    pub remotes: Vec<Remote>,
    pub default_branch: Option<String>,
    /// Every worktree of the repo: absolute path and branch (`None` when detached).
    pub worktrees: Vec<(PathBuf, Option<String>)>,
}

impl RepoFacts {
    /// Read from `<project_root>/main`. Missing pieces stay empty so a broken
    /// repo still gets its board and docs backed up.
    pub fn gather(project_root: &Path) -> Self {
        let main = project_root.join("main");
        let remotes = git_stdout(&main, &["remote", "-v"])
            .map(|s| parse_remotes(&s))
            .unwrap_or_default();
        let worktrees = git_stdout(&main, &["worktree", "list", "--porcelain"])
            .map(|s| parse_worktrees(&s))
            .unwrap_or_default();
        let default_branch = default_branch(&main, &remotes);
        Self {
            remotes,
            default_branch,
            worktrees,
        }
    }

    pub fn worktree_paths(&self) -> Vec<PathBuf> {
        self.worktrees
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    pub fn repo_manifest(&self) -> RepoManifest {
        let clone_url = self
            .remotes
            .iter()
            .find(|r| r.name == "origin")
            .or_else(|| self.remotes.first())
            .map(|r| r.url.clone());
        RepoManifest {
            clone_url,
            remotes: self.remotes.clone(),
            default_branch: self.default_branch.clone(),
        }
    }
}

/// Summaries of every issue on the board, with branches and archived plans.
/// `plans` maps issue id to the plan's archive path.
pub fn issue_manifests(
    project_root: &Path,
    state: &AppState,
    facts: &RepoFacts,
    plans: &HashMap<String, String>,
) -> Vec<IssueManifest> {
    let branches: HashMap<&Path, &str> = facts
        .worktrees
        .iter()
        .filter_map(|(path, branch)| Some((path.as_path(), branch.as_deref()?)))
        .collect();
    state
        .issues
        .iter()
        .map(|issue| {
            let worktree = issue.worktree.clone().filter(|w| !w.is_empty());
            let branch = worktree
                .as_deref()
                .and_then(|dir| branches.get(project_root.join(dir).as_path()))
                .map(|b| b.to_string());
            IssueManifest {
                id: issue.id.clone(),
                title: issue.title.clone(),
                column: issue.column,
                worktree,
                branch,
                plan: plans.get(&issue.id).cloned(),
            }
        })
        .collect()
}

pub fn parse_worktrees(porcelain: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut out: Vec<(PathBuf, Option<String>)> = Vec::new();
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            out.push((PathBuf::from(path), None));
        } else if let (Some(branch), Some(last)) = (line.strip_prefix("branch "), out.last_mut()) {
            last.1 = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
            );
        }
    }
    out
}

/// Parse `git remote -v`, keeping each remote's fetch URL.
pub fn parse_remotes(output: &str) -> Vec<Remote> {
    let mut remotes: Vec<Remote> = Vec::new();
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url)) = (parts.next(), parts.next()) else {
            continue;
        };
        if parts.next() == Some("(push)") || remotes.iter().any(|r| r.name == name) {
            continue;
        }
        remotes.push(Remote {
            name: name.to_string(),
            url: url.to_string(),
        });
    }
    remotes
}

fn default_branch(main: &Path, remotes: &[Remote]) -> Option<String> {
    let remote = remotes
        .iter()
        .find(|r| r.name == "origin")
        .or_else(|| remotes.first());
    if let Some(remote) = remote {
        let head_ref = format!("refs/remotes/{}/HEAD", remote.name);
        if let Some(full) = git_stdout(main, &["symbolic-ref", "--short", &head_ref]) {
            let full = full.trim();
            let prefix = format!("{}/", remote.name);
            return Some(full.strip_prefix(&prefix).unwrap_or(full).to_string());
        }
    }
    git_stdout(main, &["symbolic-ref", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = crate::external::git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentKind, Issue};

    const PORCELAIN: &str = "\
worktree /p/main
HEAD aaa
branch refs/heads/main

worktree /p/p-1-fix
HEAD bbb
branch refs/heads/p-1/fix

worktree /p/detached
HEAD ccc
detached
";

    #[test]
    fn parses_worktrees_with_branches() {
        assert_eq!(
            parse_worktrees(PORCELAIN),
            vec![
                (PathBuf::from("/p/main"), Some("main".to_string())),
                (PathBuf::from("/p/p-1-fix"), Some("p-1/fix".to_string())),
                (PathBuf::from("/p/detached"), None),
            ]
        );
    }

    #[test]
    fn parses_remotes_once_each() {
        let out = "origin\tgit@github.com:me/bork.git (fetch)\norigin\tgit@github.com:me/bork.git (push)\nfork\thttps://x/y.git (fetch)\nfork\thttps://x/y.git (push)\n";
        let facts = RepoFacts {
            remotes: parse_remotes(out),
            ..RepoFacts::default()
        };
        assert_eq!(facts.remotes.len(), 2);
        assert_eq!(
            facts.repo_manifest().clone_url.as_deref(),
            Some("git@github.com:me/bork.git")
        );
    }

    #[test]
    fn summarizes_issues() {
        let mut state = AppState::default();
        let mut fix = Issue::new("p-1", "Fix", Column::InProgress, AgentKind::Claude);
        fix.worktree = Some("p-1-fix".into());
        state.issues.push(fix);
        state
            .issues
            .push(Issue::new("p-2", "Done", Column::Done, AgentKind::Claude));
        let facts = RepoFacts {
            worktrees: parse_worktrees(PORCELAIN),
            ..RepoFacts::default()
        };
        let plans = HashMap::from([(
            "p-1".to_string(),
            "projects/p/_issues/p-1/planning.md".to_string(),
        )]);

        let issues = issue_manifests(Path::new("/p"), &state, &facts, &plans);

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].branch.as_deref(), Some("p-1/fix"));
        assert_eq!(
            issues[0].plan.as_deref(),
            Some("projects/p/_issues/p-1/planning.md")
        );
        assert_eq!(issues[1].column, Column::Done);
        assert_eq!(issues[1].worktree, None);
    }

    #[test]
    fn gathers_facts_from_a_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main = root.join("main");
        let git = |args: &[&str]| {
            let ok = crate::external::git_command()
                .arg("-C")
                .arg(&main)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        std::fs::create_dir_all(&main).unwrap();
        git(&["init", "-q", "-b", "trunk"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&["remote", "add", "origin", "https://example.com/r.git"]);
        let wt = root.join("p-1-x");
        git(&["worktree", "add", "-q", "-b", "p-1/x", wt.to_str().unwrap()]);

        let facts = RepoFacts::gather(&root);
        // No origin/HEAD, so it falls back to main's checked-out branch.
        assert_eq!(facts.default_branch.as_deref(), Some("trunk"));
        assert_eq!(
            facts.repo_manifest().clone_url.as_deref(),
            Some("https://example.com/r.git")
        );
        assert!(facts.worktrees.contains(&(wt, Some("p-1/x".to_string()))));
    }
}
