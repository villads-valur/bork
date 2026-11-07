//! `manifest.json`: everything `bork restore` needs to rebuild a project
//! besides the copied files: where the repo lives, and which worktrees existed
//! at which commit.
//!
//! Holds no timestamps, so an unchanged project produces a byte-identical
//! manifest and the git destination has nothing to commit.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::AppState;

pub const MANIFEST_VERSION: u32 = 1;
pub const MANIFEST_FILE: &str = "manifest.json";

/// Hidden ref the WIP snapshot step pushes each worktree's work to.
pub fn wip_ref_name(issue_id: &str) -> String {
    format!("refs/bork-backup/{issue_id}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub project_name: String,
    /// Absolute container path at backup time.
    pub container_path: PathBuf,
    pub repo: RepoManifest,
    pub worktrees: Vec<WorktreeManifest>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoManifest {
    pub remotes: Vec<Remote>,
    pub default_branch: Option<String>,
    /// Branch checked out in `main/`.
    pub main_branch: Option<String>,
    pub main_head: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeManifest {
    /// Directory name under the container, `None` when the worktree lives elsewhere.
    pub dir: Option<String>,
    pub path: PathBuf,
    pub issue_id: Option<String>,
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
    pub head: Option<String>,
    /// Where the WIP snapshot for this worktree is pushed, if it belongs to an issue.
    pub wip_ref: Option<String>,
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
}

pub fn parse_worktree_porcelain(output: &str) -> Vec<GitWorktree> {
    let mut out = Vec::new();
    let mut current: Option<GitWorktree> = None;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            out.extend(current.take());
            current = Some(GitWorktree {
                path: PathBuf::from(path),
                ..GitWorktree::default()
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(sha) = line.strip_prefix("HEAD ") {
                entry.head = Some(sha.to_string());
            } else if let Some(branch) = line.strip_prefix("branch ") {
                let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                entry.branch = Some(branch.to_string());
            }
        }
    }
    out.extend(current);
    out
}

/// Parse `git remote -v`, keeping the fetch URL of each remote.
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

/// Git facts about `main/`, gathered once per backup.
#[derive(Debug, Clone, Default)]
pub struct RepoFacts {
    pub remotes: Vec<Remote>,
    pub default_branch: Option<String>,
    pub worktrees: Vec<GitWorktree>,
}

impl RepoFacts {
    /// Read from the repo in `<project_root>/main`. Missing pieces stay empty
    /// so a broken repo still produces a manifest for the container files.
    pub fn gather(project_root: &Path) -> Self {
        let main = project_root.join("main");
        let remotes = git_stdout(&main, &["remote", "-v"])
            .map(|s| parse_remotes(&s))
            .unwrap_or_default();
        let worktrees = git_stdout(&main, &["worktree", "list", "--porcelain"])
            .map(|s| parse_worktree_porcelain(&s))
            .unwrap_or_default();
        let default_branch = default_branch(&main, &remotes);
        Self {
            remotes,
            default_branch,
            worktrees,
        }
    }
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

pub(super) fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
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

pub fn build(
    project_name: &str,
    project_root: &Path,
    facts: &RepoFacts,
    state: &AppState,
) -> Manifest {
    let main_path = project_root.join("main");
    let main = facts.worktrees.iter().find(|w| w.path == main_path);

    let mut worktrees: Vec<WorktreeManifest> = facts
        .worktrees
        .iter()
        .filter(|w| w.path != main_path)
        .map(|w| {
            let dir = if w.path.parent() == Some(project_root) {
                w.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(String::from)
            } else {
                None
            };
            let issue_id = dir.as_deref().and_then(|dir| {
                state
                    .issues
                    .iter()
                    .find(|i| i.worktree.as_deref() == Some(dir))
                    .map(|i| i.id.clone())
            });
            WorktreeManifest {
                wip_ref: issue_id.as_deref().map(wip_ref_name),
                dir,
                path: w.path.clone(),
                issue_id,
                branch: w.branch.clone(),
                head: w.head.clone(),
            }
        })
        .collect();
    worktrees.sort_by(|a, b| a.path.cmp(&b.path));

    Manifest {
        version: MANIFEST_VERSION,
        project_name: project_name.to_string(),
        container_path: project_root.to_path_buf(),
        repo: RepoManifest {
            remotes: facts.remotes.clone(),
            default_branch: facts.default_branch.clone(),
            main_branch: main.and_then(|w| w.branch.clone()),
            main_head: main.and_then(|w| w.head.clone()),
        },
        worktrees,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentKind, Column, Issue};

    const PORCELAIN: &str = "\
worktree /p/main
HEAD aaa
branch refs/heads/main

worktree /p/bork-1-fix
HEAD bbb
branch refs/heads/bork-1/fix

worktree /p/detached
HEAD ccc
detached

worktree /elsewhere/wt
HEAD ddd
branch refs/heads/other
";

    #[test]
    fn parses_porcelain() {
        let wts = parse_worktree_porcelain(PORCELAIN);
        assert_eq!(wts.len(), 4);
        assert_eq!(wts[1].branch.as_deref(), Some("bork-1/fix"));
        assert_eq!(wts[1].head.as_deref(), Some("bbb"));
        assert_eq!(wts[2].branch, None);
    }

    #[test]
    fn parses_remotes_once_each() {
        let out = "origin\tgit@github.com:me/bork.git (fetch)\norigin\tgit@github.com:me/bork.git (push)\nfork\thttps://x/y.git (fetch)\nfork\thttps://x/y.git (push)\n";
        assert_eq!(
            parse_remotes(out),
            vec![
                Remote {
                    name: "origin".into(),
                    url: "git@github.com:me/bork.git".into()
                },
                Remote {
                    name: "fork".into(),
                    url: "https://x/y.git".into()
                },
            ]
        );
    }

    #[test]
    fn builds_manifest_with_issue_mapping() {
        let mut state = AppState::default();
        let mut issue = Issue::new("bork-1", "Fix", Column::InProgress, AgentKind::Claude);
        issue.worktree = Some("bork-1-fix".into());
        state.issues.push(issue);
        let facts = RepoFacts {
            remotes: vec![Remote {
                name: "origin".into(),
                url: "u".into(),
            }],
            default_branch: Some("main".into()),
            worktrees: parse_worktree_porcelain(PORCELAIN),
        };

        let m = build("bork", Path::new("/p"), &facts, &state);

        assert_eq!(m.version, 1);
        assert_eq!(m.repo.main_branch.as_deref(), Some("main"));
        assert_eq!(m.repo.main_head.as_deref(), Some("aaa"));
        assert_eq!(m.worktrees.len(), 3);
        let fix = m
            .worktrees
            .iter()
            .find(|w| w.dir.as_deref() == Some("bork-1-fix"))
            .unwrap();
        assert_eq!(fix.issue_id.as_deref(), Some("bork-1"));
        assert_eq!(fix.wip_ref.as_deref(), Some("refs/bork-backup/bork-1"));
        assert_eq!(fix.head.as_deref(), Some("bbb"));
        let detached = m
            .worktrees
            .iter()
            .find(|w| w.dir.as_deref() == Some("detached"))
            .unwrap();
        assert_eq!(detached.issue_id, None);
        assert_eq!(detached.wip_ref, None);
        let elsewhere = m
            .worktrees
            .iter()
            .find(|w| w.path == Path::new("/elsewhere/wt"))
            .unwrap();
        assert_eq!(elsewhere.dir, None);

        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
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
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "bork-1/x",
            root.join("bork-1-x").to_str().unwrap(),
        ]);

        let facts = RepoFacts::gather(&root);
        assert_eq!(facts.remotes.len(), 1);
        // No origin/HEAD, so it falls back to main's checked-out branch.
        assert_eq!(facts.default_branch.as_deref(), Some("trunk"));
        let m = build("p", &root, &facts, &AppState::default());
        assert_eq!(m.repo.main_branch.as_deref(), Some("trunk"));
        assert!(m.repo.main_head.is_some());
        assert_eq!(m.worktrees.len(), 1);
        assert_eq!(m.worktrees[0].branch.as_deref(), Some("bork-1/x"));
    }
}
