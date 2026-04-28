use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::global_config;

pub const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60; // 6 hours

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UpdateCache {
    last_check: u64,
    update_available: bool,
}

/// Outcome of a single GitHub query. `Failed` means we couldn't determine the
/// answer (no embedded commit, no `gh`, network error, etc.) and the cache
/// must be left alone so we retry next time instead of suppressing the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    UpdateAvailable,
    Failed,
}

fn cache_path() -> PathBuf {
    global_config::global_config_dir().join("update_check.json")
}

fn load_cache() -> UpdateCache {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &UpdateCache) {
    let path = cache_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    let _ = std::fs::write(&path, serde_json::to_string(cache).unwrap_or_default());
}

/// Modification time of the cache file as a unix timestamp (seconds).
/// Returns 0 if the file doesn't exist yet. Cheap stat call; safe to poll often.
pub fn cache_mtime_secs() -> u64 {
    std::fs::metadata(cache_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True if the most recent successful check found an update.
pub fn cached_update_available() -> bool {
    load_cache().update_available
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn resolve_source_repo() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to determine executable path")?;
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);

    let mut dir = resolved.parent();
    while let Some(d) = dir {
        if d.join("Cargo.toml").exists() {
            return Ok(d.to_path_buf());
        }
        dir = d.parent();
    }

    bail!(
        "Could not find source repository. \
         bork update requires a source install (symlink from cargo build)."
    )
}

/// The git commit this binary was built from. Prefers the value embedded by
/// `build.rs`; if that's empty (build.rs couldn't reach git), falls back to
/// `git rev-parse HEAD` in the resolved source repo. Memoized for the session.
fn current_commit() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            if let Some(embedded) = non_empty(env!("BORK_GIT_COMMIT").trim().to_string()) {
                return Some(embedded);
            }

            let repo = resolve_source_repo().ok()?;
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            non_empty(String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .clone()
}

fn github_repo() -> Option<&'static str> {
    let repo = env!("BORK_GITHUB_REPO").trim();
    (!repo.is_empty()).then_some(repo)
}

/// Query GitHub for the current `main` HEAD sha.
fn fetch_remote_head_sha(repo: &str) -> Option<String> {
    let output = Command::new("gh")
        .args(["api", &format!("repos/{repo}/commits/main"), "--jq", ".sha"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    non_empty(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Compare the running binary's commit against `origin/main` on GitHub.
/// Returns the verdict plus both shas (for display); `Failed` on any error so
/// callers can preserve the previous cache.
pub fn run_check() -> (CheckResult, Option<String>, Option<String>) {
    let local = current_commit();
    let remote = github_repo().and_then(fetch_remote_head_sha);

    let result = match (&local, &remote) {
        (Some(l), Some(r)) if l == r => CheckResult::UpToDate,
        (Some(_), Some(_)) => CheckResult::UpdateAvailable,
        _ => CheckResult::Failed,
    };

    (result, local, remote)
}

/// Periodic check used by the background worker. Honours the cache TTL and
/// only writes the cache on success - failed checks leave the previous result
/// intact and don't bump `last_check`, so we retry on the next interval.
pub fn check_for_update() -> bool {
    let cache = load_cache();
    let now = unix_now();

    if now.saturating_sub(cache.last_check) < CHECK_INTERVAL_SECS {
        return cache.update_available;
    }

    let (result, _, _) = run_check();
    let available = match result {
        CheckResult::UpdateAvailable => true,
        CheckResult::UpToDate => false,
        CheckResult::Failed => return cache.update_available,
    };

    save_cache(&UpdateCache {
        last_check: now,
        update_available: available,
    });
    available
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// `bork update --check` entry point. Bypasses the cache, prints both shas,
/// and writes the result back so a running TUI picks it up via the cache mtime
/// poller.
pub fn run_check_command() -> anyhow::Result<()> {
    let (result, local, remote) = run_check();

    if matches!(result, CheckResult::UpToDate | CheckResult::UpdateAvailable) {
        save_cache(&UpdateCache {
            last_check: unix_now(),
            update_available: matches!(result, CheckResult::UpdateAvailable),
        });
    }

    let local_str = local.as_deref().map(short_sha).unwrap_or("unknown");
    let remote_str = remote.as_deref().map(short_sha).unwrap_or("unknown");
    let repo = github_repo().unwrap_or("(unknown repo)");

    println!("Repo:           {repo}");
    println!("Current build:  {local_str}");
    println!("Latest on main: {remote_str}");
    println!();

    match result {
        CheckResult::UpToDate => {
            println!("Up to date.");
            Ok(())
        }
        CheckResult::UpdateAvailable => {
            println!("Update available. Run `bork update` to upgrade.");
            Ok(())
        }
        CheckResult::Failed => bail!(
            "Could not determine update status. Ensure `gh` is installed and authenticated, \
             and that this binary was built from a git checkout."
        ),
    }
}

pub fn run_update() -> anyhow::Result<()> {
    let repo = resolve_source_repo()?;
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    println!("Source: {}", repo.display());
    println!();

    println!("Fetching latest...");
    let fetch = Command::new("git")
        .args(["fetch", "origin", "main", "--quiet"])
        .current_dir(&repo)
        .status()
        .context("failed to run git fetch")?;

    if !fetch.success() {
        bail!("git fetch failed");
    }

    let output = Command::new("git")
        .args(["rev-list", "HEAD..origin/main", "--count"])
        .current_dir(&repo)
        .output()
        .context("failed to check for updates")?;

    let behind: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    if behind == 0 {
        println!("Already up to date (v{current}).");
        return Ok(());
    }

    println!("{behind} new commit(s) available. Pulling...");
    println!();

    let pull = Command::new("git")
        .args(["pull", "origin", "main"])
        .current_dir(&repo)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git pull")?;

    if !pull.success() {
        bail!("git pull failed");
    }

    println!();
    println!("Building...");
    println!();

    let build = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&repo)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run cargo build")?;

    if !build.success() {
        bail!("cargo build failed");
    }

    save_cache(&UpdateCache::default());

    println!();
    println!("Updated to latest. Restart bork to use the new version.");

    Ok(())
}
