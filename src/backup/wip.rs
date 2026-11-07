//! WIP snapshots: back up in-flight work in issue worktrees (uncommitted
//! changes, untracked files, unpushed commits) to hidden refs on the repo's
//! own remote, `refs/bork-backup/<issue-id>`.
//!
//! A snapshot is a commit built from a throwaway index, so the worktree's
//! files, real index and stash list are never touched. Its parent is HEAD, so
//! the ref also carries any unpushed commits.
//!
//! Pushes only ever go to the repo's own `origin`, which is the trust
//! boundary the code already lives in (this matters for work repos).
//!
//! The local `refs/bork-backup/<issue-id>` mirrors the last pushed snapshot.
//! It is the "unchanged" cache and keeps the snapshot objects alive across gc.
//! A ref exists only while its worktree has something worth restoring.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config;
use crate::toml_lite;
use crate::types::{Column, Issue};

pub const REMOTE: &str = "origin";
pub const REF_PREFIX: &str = "refs/bork-backup/";

const SNAPSHOT_NAME: &str = "bork backup";
const SNAPSHOT_EMAIL: &str = "bork-backup@localhost";

#[derive(Debug, Clone, Copy, Default)]
pub struct WipOptions {
    /// Work out what would happen without creating commits, pushing or
    /// changing refs.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WipOutcome {
    /// A new snapshot was pushed to the backup ref.
    Pushed {
        commit: String,
    },
    /// Dry run: a snapshot would have been pushed.
    WouldPush,
    /// Snapshot matches the last pushed one, nothing to do.
    Unchanged,
    /// No uncommitted changes and HEAD is on a remote branch.
    Clean,
    Skipped(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WipEntry {
    pub worktree: String,
    pub issue_id: Option<String>,
    pub outcome: WipOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    Deleted,
    WouldDelete,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedRef {
    pub issue_id: String,
    pub outcome: PruneOutcome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WipReport {
    /// Set when the whole project was skipped (disabled, no remote, ...).
    pub skipped: Option<String>,
    pub entries: Vec<WipEntry>,
    pub pruned: Vec<PrunedRef>,
}

impl WipReport {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            skipped: Some(reason.into()),
            ..Self::default()
        }
    }

    pub fn error_count(&self) -> usize {
        let entries = self
            .entries
            .iter()
            .filter(|e| matches!(e.outcome, WipOutcome::Error(_)))
            .count();
        let pruned = self
            .pruned
            .iter()
            .filter(|p| matches!(p.outcome, PruneOutcome::Error(_)))
            .count();
        entries + pruned
    }
}

pub fn backup_ref(issue_id: &str) -> String {
    format!("{REF_PREFIX}{issue_id}")
}

/// Whether WIP backups are on for this project: `[backup] enabled = true` in
/// the project's `.bork/config.toml`, with `wip` defaulting to true. Project
/// layer only, since backups are a per-project opt-in.
pub fn wip_enabled(project_root: &Path) -> bool {
    let path = project_root.join(".bork").join("config.toml");
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let table = toml_lite::parse(&contents);
    let flag = |key: &str| table.get(key).and_then(|v| v.as_bool());
    flag("backup.enabled").unwrap_or(false) && flag("backup.wip").unwrap_or(true)
}

/// Snapshot every issue worktree of the project at `project_root` and prune
/// backup refs that are no longer needed. Never fails as a whole: problems
/// are reported per worktree.
pub fn snapshot_project(project_root: &Path, opts: WipOptions) -> WipReport {
    if !wip_enabled(project_root) {
        return WipReport::skipped(
            "WIP backups are off ([backup] enabled/wip in .bork/config.toml)",
        );
    }
    let state = config::load_state(project_root);
    snapshot_worktrees(project_root, &state.issues, opts)
}

fn snapshot_worktrees(project_root: &Path, issues: &[Issue], opts: WipOptions) -> WipReport {
    let main_dir = project_root.join("main");
    if let Err(err) = git(&main_dir, &["remote", "get-url", REMOTE]) {
        return WipReport::skipped(format!("no `{REMOTE}` remote: {err}"));
    }

    let mut report = WipReport::default();
    let mut active: Vec<String> = Vec::new();
    let mut clean: Vec<String> = Vec::new();

    for name in crate::prune::discover_worktree_names(project_root) {
        let issue = issues
            .iter()
            .find(|i| i.worktree.as_deref() == Some(name.as_str()));
        let Some(issue) = issue else {
            report.entries.push(WipEntry {
                worktree: name,
                issue_id: None,
                outcome: WipOutcome::Skipped("not linked to an issue".into()),
            });
            continue;
        };
        if issue.column == Column::Done {
            // Done work isn't backed up; its ref is pruned below.
            continue;
        }
        if !is_safe_ref_component(&issue.id) {
            report.entries.push(WipEntry {
                worktree: name,
                issue_id: Some(issue.id.clone()),
                outcome: WipOutcome::Skipped(format!("issue id `{}` is not ref-safe", issue.id)),
            });
            continue;
        }

        let dir = project_root.join(&name);
        let outcome = match snapshot_worktree(&dir, &issue.id, opts) {
            Ok(Step::Done(outcome)) => outcome,
            Ok(Step::Clean) => {
                // Nothing worth restoring: drop the stale ref (if any) so a
                // restore falls back to the branch.
                clean.push(issue.id.clone());
                WipOutcome::Clean
            }
            Err(outcome) => outcome,
        };
        active.push(issue.id.clone());
        report.entries.push(WipEntry {
            worktree: name,
            issue_id: Some(issue.id.clone()),
            outcome,
        });
    }

    // Only refs this machine created (mirrored locally) are pruned, never
    // unknown ids on the remote: on a shared repo they may be someone else's.
    let mut to_delete: Vec<String> = local_backup_ids(&main_dir)
        .into_iter()
        .filter(|id| !active.contains(id) || clean.contains(id))
        .collect();
    to_delete.sort();
    report.pruned = delete_refs(&main_dir, &to_delete, opts);
    report
}

enum Step {
    Done(WipOutcome),
    Clean,
}

fn snapshot_worktree(dir: &Path, issue_id: &str, opts: WipOptions) -> Result<Step, WipOutcome> {
    let error = WipOutcome::Error;

    let head = match git(dir, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]) {
        Ok(head) => head,
        Err(_) => return Err(WipOutcome::Skipped("no commits yet (unborn HEAD)".into())),
    };
    let git_dir = PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"]).map_err(error)?);
    if let Some(op) = operation_in_progress(&git_dir) {
        return Err(WipOutcome::Skipped(format!("{op} in progress")));
    }

    let tree = build_tree(dir, &git_dir).map_err(error)?;
    let head_tree = git(dir, &["rev-parse", "HEAD^{tree}"]).map_err(error)?;
    let remote_glob = format!("--remotes={REMOTE}");
    let unpushed = git(dir, &["rev-list", "-n1", "HEAD", "--not", &remote_glob]).map_err(error)?;
    if tree == head_tree && unpushed.is_empty() {
        return Ok(Step::Clean);
    }

    let ref_name = backup_ref(issue_id);
    let last = git(dir, &["log", "-1", "--format=%T %P", &ref_name, "--"]).ok();
    if last.as_deref() == Some(format!("{tree} {head}").as_str()) {
        return Ok(Step::Done(WipOutcome::Unchanged));
    }
    if opts.dry_run {
        return Ok(Step::Done(WipOutcome::WouldPush));
    }

    let branch = git(dir, &["symbolic-ref", "-q", "--short", "HEAD"])
        .unwrap_or_else(|_| "(detached)".into());
    let message = format!(
        "bork backup: {issue_id} {branch} {}",
        format_utc(crate::app::unix_now())
    );
    let commit = git_with_env(
        dir,
        &["commit-tree", &tree, "-p", &head, "-m", &message],
        &identity_env(),
    )
    .map_err(error)?;

    // Force (`+`) is fine here: the destination is our hidden backup ref,
    // which only ever holds the latest snapshot. Never use a forced refspec
    // for anything outside REF_PREFIX, and never for branches.
    let refspec = format!("+{commit}:{ref_name}");
    git(dir, &["push", "--no-verify", "--quiet", REMOTE, &refspec]).map_err(error)?;
    git(dir, &["update-ref", &ref_name, &commit]).map_err(error)?;
    Ok(Step::Done(WipOutcome::Pushed { commit }))
}

/// Write the worktree's full content (tracked + untracked, minus ignored)
/// as a tree, via a throwaway copy of its index. Copying the index (rather
/// than `read-tree HEAD`) keeps git's stat cache, so unchanged files aren't
/// rehashed.
fn build_tree(dir: &Path, git_dir: &Path) -> Result<String, String> {
    let tmp = TempIndex(git_dir.join(format!("bork-backup-index.{}", std::process::id())));
    let real = git_dir.join("index");
    if real.exists() {
        fs::copy(&real, &tmp.0).map_err(|e| format!("copy index: {e}"))?;
    } else {
        git_with_env(dir, &["read-tree", "HEAD"], &tmp.env())?;
    }
    git_with_env(dir, &["add", "-A"], &tmp.env())?;
    git_with_env(dir, &["write-tree"], &tmp.env())
}

struct TempIndex(PathBuf);

impl TempIndex {
    fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![("GIT_INDEX_FILE", self.0.clone().into_os_string())]
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }
}

fn operation_in_progress(git_dir: &Path) -> Option<&'static str> {
    const MARKERS: [(&str, &str); 6] = [
        ("MERGE_HEAD", "merge"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ];
    MARKERS
        .iter()
        .find(|(file, _)| git_dir.join(file).exists())
        .map(|(_, op)| *op)
}

fn local_backup_ids(repo: &Path) -> Vec<String> {
    git(repo, &["for-each-ref", "--format=%(refname)", REF_PREFIX])
        .unwrap_or_default()
        .lines()
        .filter_map(|r| r.strip_prefix(REF_PREFIX))
        .map(str::to_string)
        .collect()
}

/// Delete backup refs on the remote (one batched push), then locally.
fn delete_refs(repo: &Path, ids: &[String], opts: WipOptions) -> Vec<PrunedRef> {
    if ids.is_empty() {
        return Vec::new();
    }
    let outcome = if opts.dry_run {
        PruneOutcome::WouldDelete
    } else {
        let mut args = vec!["push", "--no-verify", "--quiet", REMOTE];
        let refspecs: Vec<String> = ids
            .iter()
            .map(|id| format!(":{}", backup_ref(id)))
            .collect();
        args.extend(refspecs.iter().map(String::as_str));
        match git(repo, &args) {
            Ok(_) => PruneOutcome::Deleted,
            Err(err) => PruneOutcome::Error(err),
        }
    };
    ids.iter()
        .map(|id| {
            let mut outcome = outcome.clone();
            if outcome == PruneOutcome::Deleted {
                if let Err(err) = git(repo, &["update-ref", "-d", &backup_ref(id)]) {
                    outcome = PruneOutcome::Error(err);
                }
            }
            PrunedRef {
                issue_id: id.clone(),
                outcome,
            }
        })
        .collect()
}

fn is_safe_ref_component(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains("..")
        && !id.ends_with(".lock")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn identity_env() -> Vec<(&'static str, OsString)> {
    vec![
        ("GIT_AUTHOR_NAME", SNAPSHOT_NAME.into()),
        ("GIT_AUTHOR_EMAIL", SNAPSHOT_EMAIL.into()),
        ("GIT_COMMITTER_NAME", SNAPSHOT_NAME.into()),
        ("GIT_COMMITTER_EMAIL", SNAPSHOT_EMAIL.into()),
    ]
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_with_env(dir, args, &[])
}

/// Run git without optional locks (so reads never rewrite the real index
/// under a running agent) and without prompting for credentials.
fn git_with_env(dir: &Path, args: &[&str], env: &[(&str, OsString)]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(env.iter().map(|(k, v)| (*k, v)))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("git {} failed", args.join(" ")))
    } else {
        Err(stderr)
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a unix timestamp.
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentKind;

    struct Fixture {
        _tmp: tempfile::TempDir,
        project: PathBuf,
        bare: PathBuf,
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.email=test@test.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn setup() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare.git");
        let project = tmp.path().join("project");
        let main = project.join("main");
        fs::create_dir_all(&bare).unwrap();
        run_git(&bare, &["init", "--bare", "-q"]);
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                main.to_str().unwrap(),
            ],
        );
        fs::write(main.join("README.md"), "# test\n").unwrap();
        fs::write(main.join(".gitignore"), "target/\n").unwrap();
        run_git(&main, &["add", "."]);
        run_git(&main, &["commit", "-q", "-m", "init"]);
        run_git(&main, &["push", "-q", "origin", "HEAD"]);
        fs::create_dir_all(project.join(".bork")).unwrap();
        fs::write(
            project.join(".bork/config.toml"),
            "[backup]\nenabled = true\nwip = true\n",
        )
        .unwrap();
        Fixture {
            _tmp: tmp,
            project,
            bare,
        }
    }

    impl Fixture {
        fn main(&self) -> PathBuf {
            self.project.join("main")
        }

        fn add_worktree(&self, id: &str) -> PathBuf {
            let name = format!("{id}-work");
            let dir = self.project.join(&name);
            run_git(
                &self.main(),
                &[
                    "worktree",
                    "add",
                    "-q",
                    dir.to_str().unwrap(),
                    "-b",
                    &format!("{id}/work"),
                ],
            );
            dir
        }

        fn remote_ref(&self, id: &str) -> Option<String> {
            let out = Command::new("git")
                .args(["rev-parse", "-q", "--verify", &backup_ref(id)])
                .current_dir(&self.bare)
                .output()
                .unwrap();
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        }

        fn run(&self, issues: &[Issue]) -> WipReport {
            snapshot_worktrees(&self.project, issues, WipOptions::default())
        }
    }

    fn issue(id: &str, column: Column) -> Issue {
        Issue {
            worktree: Some(format!("{id}-work")),
            ..Issue::new(id, "t", column, AgentKind::Claude)
        }
    }

    fn outcome_for<'a>(report: &'a WipReport, id: &str) -> &'a WipOutcome {
        &report
            .entries
            .iter()
            .find(|e| e.issue_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no entry for {id}: {report:?}"))
            .outcome
    }

    #[test]
    fn snapshots_dirty_and_untracked_files_but_not_ignored_ones() {
        let fx = setup();
        let wt = fx.add_worktree("p-1");
        fs::write(wt.join("README.md"), "# changed\n").unwrap();
        fs::write(wt.join("new.txt"), "untracked\n").unwrap();
        fs::create_dir_all(wt.join("target")).unwrap();
        fs::write(wt.join("target/big.bin"), "ignored\n").unwrap();

        let report = fx.run(&[issue("p-1", Column::InProgress)]);

        let WipOutcome::Pushed { commit } = outcome_for(&report, "p-1") else {
            panic!("expected push: {report:?}");
        };
        assert_eq!(fx.remote_ref("p-1").as_deref(), Some(commit.as_str()));
        assert_eq!(
            run_git(&wt, &["show", &format!("{commit}:README.md")]),
            "# changed"
        );
        assert_eq!(
            run_git(&wt, &["show", &format!("{commit}:new.txt")]),
            "untracked"
        );
        let files = run_git(&wt, &["ls-tree", "-r", "--name-only", commit]);
        assert!(!files.contains("target/"), "ignored file captured: {files}");
        assert_eq!(
            run_git(&wt, &["rev-parse", &format!("{commit}^")]),
            run_git(&wt, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn leaves_working_tree_index_and_stash_untouched() {
        let fx = setup();
        let wt = fx.add_worktree("p-1");
        fs::write(wt.join("README.md"), "# staged\n").unwrap();
        run_git(&wt, &["add", "README.md"]);
        fs::write(wt.join("README.md"), "# unstaged on top\n").unwrap();
        fs::write(wt.join("new.txt"), "untracked\n").unwrap();
        let git_dir = PathBuf::from(run_git(&wt, &["rev-parse", "--absolute-git-dir"]));
        let index_before = fs::read(git_dir.join("index")).unwrap();
        let status_before = run_git(&wt, &["status", "--porcelain"]);

        let report = fx.run(&[issue("p-1", Column::InProgress)]);
        assert!(matches!(
            outcome_for(&report, "p-1"),
            WipOutcome::Pushed { .. }
        ));

        assert_eq!(fs::read(git_dir.join("index")).unwrap(), index_before);
        assert_eq!(run_git(&wt, &["status", "--porcelain"]), status_before);
        assert_eq!(
            fs::read_to_string(wt.join("README.md")).unwrap(),
            "# unstaged on top\n"
        );
        assert_eq!(run_git(&wt, &["stash", "list"]), "");
        let leftovers: Vec<_> = fs::read_dir(&git_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("bork-backup-index")
            })
            .collect();
        assert!(leftovers.is_empty(), "temp index left behind");
    }

    #[test]
    fn snapshots_unpushed_commits_on_a_clean_tree() {
        let fx = setup();
        let wt = fx.add_worktree("p-1");
        fs::write(wt.join("feature.rs"), "fn main() {}\n").unwrap();
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "local only"]);

        let report = fx.run(&[issue("p-1", Column::InProgress)]);

        let WipOutcome::Pushed { commit } = outcome_for(&report, "p-1") else {
            panic!("expected push: {report:?}");
        };
        let head = run_git(&wt, &["rev-parse", "HEAD"]);
        assert_eq!(run_git(&wt, &["rev-parse", &format!("{commit}^")]), head);
        run_git(&fx.bare, &["cat-file", "-e", &head]);
    }

    #[test]
    fn unchanged_snapshot_is_not_pushed_again() {
        let fx = setup();
        let wt = fx.add_worktree("p-1");
        fs::write(wt.join("new.txt"), "v1\n").unwrap();
        let issues = [issue("p-1", Column::InProgress)];

        let first = fx.run(&issues);
        let WipOutcome::Pushed { commit } = outcome_for(&first, "p-1").clone() else {
            panic!("expected push: {first:?}");
        };
        let second = fx.run(&issues);
        assert_eq!(outcome_for(&second, "p-1"), &WipOutcome::Unchanged);
        assert_eq!(fx.remote_ref("p-1"), Some(commit.clone()));

        fs::write(wt.join("new.txt"), "v2\n").unwrap();
        let third = fx.run(&issues);
        let WipOutcome::Pushed { commit: newer } = outcome_for(&third, "p-1") else {
            panic!("expected push: {third:?}");
        };
        assert_ne!(newer, &commit);
        assert_eq!(fx.remote_ref("p-1").as_deref(), Some(newer.as_str()));
    }

    #[test]
    fn clean_pushed_worktree_drops_its_backup_ref() {
        let fx = setup();
        let wt = fx.add_worktree("p-1");
        let issues = [issue("p-1", Column::InProgress)];
        assert_eq!(outcome_for(&fx.run(&issues), "p-1"), &WipOutcome::Clean);

        fs::write(wt.join("new.txt"), "wip\n").unwrap();
        fx.run(&issues);
        assert!(fx.remote_ref("p-1").is_some());

        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "done"]);
        run_git(&wt, &["push", "-q", "origin", "HEAD"]);
        let report = fx.run(&issues);
        assert_eq!(outcome_for(&report, "p-1"), &WipOutcome::Clean);
        assert_eq!(report.pruned[0].outcome, PruneOutcome::Deleted);
        assert!(fx.remote_ref("p-1").is_none());
        assert!(local_backup_ids(&fx.main()).is_empty());
    }

    #[test]
    fn prunes_refs_of_done_issues_and_missing_worktrees_only() {
        let fx = setup();
        let wt1 = fx.add_worktree("p-1");
        let wt2 = fx.add_worktree("p-2");
        let wt3 = fx.add_worktree("p-3");
        for wt in [&wt1, &wt2, &wt3] {
            fs::write(wt.join("new.txt"), "wip\n").unwrap();
        }
        fx.run(&[
            issue("p-1", Column::InProgress),
            issue("p-2", Column::InProgress),
            issue("p-3", Column::InProgress),
        ]);
        // Someone else's ref on a shared remote must survive pruning.
        run_git(
            &fx.main(),
            &["push", "-q", "origin", "HEAD:refs/bork-backup/p-9"],
        );

        run_git(
            &fx.main(),
            &["worktree", "remove", "--force", wt2.to_str().unwrap()],
        );
        let report = fx.run(&[
            issue("p-1", Column::Done),
            issue("p-2", Column::InProgress),
            issue("p-3", Column::InProgress),
        ]);

        let pruned: Vec<_> = report.pruned.iter().map(|p| p.issue_id.as_str()).collect();
        assert_eq!(pruned, ["p-1", "p-2"]);
        assert!(fx.remote_ref("p-1").is_none());
        assert!(fx.remote_ref("p-2").is_none());
        assert!(fx.remote_ref("p-3").is_some());
        assert!(fx.remote_ref("p-9").is_some());
        assert_eq!(local_backup_ids(&fx.main()), ["p-3"]);
    }

    #[test]
    fn skips_worktree_mid_merge_and_keeps_its_ref() {
        let fx = setup();
        let wt = fx.add_worktree("p-1");
        fs::write(wt.join("new.txt"), "wip\n").unwrap();
        let issues = [issue("p-1", Column::InProgress)];
        fx.run(&issues);

        let git_dir = PathBuf::from(run_git(&wt, &["rev-parse", "--absolute-git-dir"]));
        fs::write(
            git_dir.join("MERGE_HEAD"),
            run_git(&wt, &["rev-parse", "HEAD"]),
        )
        .unwrap();
        let report = fx.run(&issues);

        assert_eq!(
            outcome_for(&report, "p-1"),
            &WipOutcome::Skipped("merge in progress".into())
        );
        assert!(report.pruned.is_empty());
        assert!(fx.remote_ref("p-1").is_some());
    }

    #[test]
    fn dry_run_changes_nothing() {
        let fx = setup();
        let wt = fx.add_worktree("p-1");
        fs::write(wt.join("new.txt"), "wip\n").unwrap();

        let report = snapshot_worktrees(
            &fx.project,
            &[issue("p-1", Column::InProgress)],
            WipOptions { dry_run: true },
        );

        assert_eq!(outcome_for(&report, "p-1"), &WipOutcome::WouldPush);
        assert!(fx.remote_ref("p-1").is_none());
        assert!(local_backup_ids(&fx.main()).is_empty());
    }

    #[test]
    fn orphan_worktrees_are_skipped() {
        let fx = setup();
        fx.add_worktree("p-1");
        let report = fx.run(&[]);
        assert_eq!(
            report.entries[0].outcome,
            WipOutcome::Skipped("not linked to an issue".into())
        );
    }

    #[test]
    fn respects_project_opt_in() {
        let fx = setup();
        assert!(wip_enabled(&fx.project));
        assert!(snapshot_project(&fx.project, WipOptions::default())
            .skipped
            .is_none());

        fs::write(
            fx.project.join(".bork/config.toml"),
            "[backup]\nenabled = true\nwip = false\n",
        )
        .unwrap();
        assert!(!wip_enabled(&fx.project));
        fs::write(
            fx.project.join(".bork/config.toml"),
            "[backup]\nwip = true\n",
        )
        .unwrap();
        assert!(!wip_enabled(&fx.project));
        fs::write(
            fx.project.join(".bork/config.toml"),
            "backup.enabled = true\n",
        )
        .unwrap();
        assert!(wip_enabled(&fx.project));
        fs::remove_file(fx.project.join(".bork/config.toml")).unwrap();
        assert!(snapshot_project(&fx.project, WipOptions::default())
            .skipped
            .is_some());
    }

    #[test]
    fn ref_safe_issue_ids() {
        assert!(is_safe_ref_component("bork-14"));
        assert!(is_safe_ref_component("abc-123"));
        assert!(!is_safe_ref_component("a..b"));
        assert!(!is_safe_ref_component("a/b"));
        assert!(!is_safe_ref_component("x.lock"));
        assert!(!is_safe_ref_component(""));
    }

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }
}
