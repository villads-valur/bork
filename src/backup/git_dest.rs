//! Git destination: a local clone (usually of a private repo). Each scope's
//! subdirectory is committed only when it changed, then pushed if the clone
//! has a remote. Never force pushes.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use super::manifest::git_stdout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    NoRemote,
    UpToDate,
    Pushed { remote: String },
    Failed { remote: String, error: String },
}

/// Make sure `repo` is the top of a git repository. A missing or empty
/// directory gets `git init`; a non-empty directory that is not a repo root
/// is refused, so a typo in `path` can't commit into an unrelated repo.
pub fn prepare(repo: &Path) -> anyhow::Result<()> {
    if repo.exists() && is_repo_root(repo) {
        return Ok(());
    }
    let is_empty = match fs::read_dir(repo) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => !repo.exists(),
    };
    if !is_empty {
        bail!(
            "{} is not a git repository; clone your backup repo there or use destination = \"folder\"",
            repo.display()
        );
    }
    fs::create_dir_all(repo)?;
    run(repo, &["init", "-q"])?;
    Ok(())
}

fn is_repo_root(repo: &Path) -> bool {
    let Some(top) = git_stdout(repo, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    let top = PathBuf::from(top.trim());
    match (top.canonicalize(), repo.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Stage `subdir` and commit it alone if anything in it changed. Returns
/// whether a commit was made.
pub fn commit_subdir(repo: &Path, subdir: &str, message: &str) -> anyhow::Result<bool> {
    run(repo, &["add", "-A", "--", subdir])?;
    let status = crate::external::git_command()
        .arg("-C")
        .arg(repo)
        .args(["diff", "--cached", "--quiet", "--", subdir])
        .status()
        .context("running git diff")?;
    match status.code() {
        Some(0) => return Ok(false),
        Some(1) => {}
        _ => bail!("git diff --cached failed in {}", repo.display()),
    }
    // Backup commits are machine-made; signing could block on a pinentry.
    run(
        repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            message,
            "--",
            subdir,
        ],
    )?;
    Ok(true)
}

/// Push the current branch to `origin` (or the first remote) when it is
/// ahead of the remote-tracking ref. Plain `git push`, never forced.
pub fn push(repo: &Path) -> PushOutcome {
    let remotes = git_stdout(repo, &["remote"]).unwrap_or_default();
    let remotes: Vec<&str> = remotes
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect();
    let Some(remote) = remotes
        .iter()
        .find(|r| **r == "origin")
        .or_else(|| remotes.first())
        .map(|r| r.to_string())
    else {
        return PushOutcome::NoRemote;
    };

    let Some(head) = git_stdout(repo, &["rev-parse", "--verify", "-q", "HEAD"]) else {
        return PushOutcome::UpToDate;
    };
    let branch = git_stdout(repo, &["symbolic-ref", "--short", "HEAD"])
        .map(|b| b.trim().to_string())
        .unwrap_or_default();
    if branch.is_empty() {
        return PushOutcome::Failed {
            remote,
            error: "detached HEAD in backup repo".to_string(),
        };
    }
    let tracking = format!("refs/remotes/{remote}/{branch}");
    if git_stdout(repo, &["rev-parse", "--verify", "-q", &tracking]).as_deref()
        == Some(head.as_str())
    {
        return PushOutcome::UpToDate;
    }

    let refspec = format!("HEAD:refs/heads/{branch}");
    match run(repo, &["push", "-q", &remote, &refspec]) {
        Ok(()) => {
            // Keep the tracking ref current so the next run can skip the network.
            let _ = run(repo, &["update-ref", &tracking, head.trim()]);
            PushOutcome::Pushed { remote }
        }
        Err(err) => PushOutcome::Failed {
            remote,
            error: err.to_string(),
        },
    }
}

fn run(repo: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = crate::external::git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run with a throwaway identity so tests don't depend on global git config.
    fn with_identity(repo: &Path) {
        run(repo, &["config", "user.name", "t"]).unwrap();
        run(repo, &["config", "user.email", "t@t"]).unwrap();
    }

    #[test]
    fn inits_missing_dir_and_refuses_unrelated_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("backups");
        prepare(&repo).unwrap();
        assert!(repo.join(".git").exists());
        prepare(&repo).unwrap();

        let other = dir.path().join("stuff");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("f"), "x").unwrap();
        assert!(prepare(&other).is_err());

        // A subdir of a repo is not a repo root.
        let nested = repo.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("f"), "x").unwrap();
        assert!(prepare(&nested).is_err());
    }

    #[test]
    fn commits_only_when_changed_and_pushes() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "--bare"]).unwrap();

        let repo = dir.path().join("clone");
        prepare(&repo).unwrap();
        with_identity(&repo);
        assert_eq!(push(&repo), PushOutcome::NoRemote);

        fs::create_dir_all(repo.join("proj")).unwrap();
        fs::write(repo.join("proj/a"), "1").unwrap();
        assert!(commit_subdir(&repo, "proj", "backup: proj 1").unwrap());
        assert!(!commit_subdir(&repo, "proj", "backup: proj 2").unwrap());

        // Other scopes' changes stay out of this scope's commit.
        fs::create_dir_all(repo.join("other")).unwrap();
        fs::write(repo.join("other/b"), "x").unwrap();
        fs::write(repo.join("proj/a"), "2").unwrap();
        assert!(commit_subdir(&repo, "proj", "backup: proj 3").unwrap());
        let files = git_stdout(&repo, &["show", "--name-only", "--format=", "HEAD"]).unwrap();
        assert_eq!(files.trim(), "proj/a");

        run(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .unwrap();
        assert!(matches!(push(&repo), PushOutcome::Pushed { .. }));
        assert_eq!(push(&repo), PushOutcome::UpToDate);
        let log = git_stdout(&remote, &["log", "--format=%s"]).unwrap();
        assert!(log.contains("backup: proj 3"));
        assert!(!log.contains("backup: proj 2"));
    }

    #[test]
    fn rejected_push_is_reported_not_forced() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "-q", "--bare"]).unwrap();

        let make_clone = |name: &str, content: &str| {
            let repo = dir.path().join(name);
            prepare(&repo).unwrap();
            with_identity(&repo);
            run(&repo, &["checkout", "-q", "-b", "main"]).unwrap();
            run(
                &repo,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            )
            .unwrap();
            fs::create_dir_all(repo.join("p")).unwrap();
            fs::write(repo.join("p/f"), content).unwrap();
            commit_subdir(&repo, "p", name).unwrap();
            repo
        };
        let first = make_clone("first", "a");
        assert!(matches!(push(&first), PushOutcome::Pushed { .. }));
        let second = make_clone("second", "b");
        assert!(matches!(push(&second), PushOutcome::Failed { .. }));
        let log = git_stdout(&remote, &["log", "--format=%s", "main"]).unwrap();
        assert_eq!(log.trim(), "first");
    }
}
