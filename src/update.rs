use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::external::tmux::BORK_TUI_SESSION;
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

/// Returns how many commits `main` on GitHub is ahead of `local_sha`.
/// Returns `None` on any error (gh not available, network issue, sha unknown to GitHub).
fn fetch_commits_behind(repo: &str, local_sha: &str) -> Option<u32> {
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{repo}/compare/{local_sha}...main"),
            "--jq",
            ".ahead_by",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()
}

/// Compare the running binary's commit against `origin/main` on GitHub.
/// Returns the verdict plus the local sha and (for display) the remote sha.
/// `Failed` on any error so callers can preserve the previous cache.
///
/// Uses a directional compare (`compare/{local}...main`) instead of simple SHA
/// equality so that dev builds that are ahead of or diverged from main do not
/// produce false-positive `UpdateAvailable` results.
pub fn run_check() -> (CheckResult, Option<String>, Option<String>) {
    let local = current_commit();

    let result = match &local {
        None => CheckResult::Failed,
        Some(sha) => match github_repo().and_then(|repo| fetch_commits_behind(repo, sha)) {
            Some(0) => CheckResult::UpToDate,
            Some(_) => CheckResult::UpdateAvailable,
            None => CheckResult::Failed,
        },
    };

    // Remote SHA is not needed for the check itself; run_check_command fetches
    // it separately for display purposes.
    (result, local, None)
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
    let (result, local, _) = run_check();

    if matches!(result, CheckResult::UpToDate | CheckResult::UpdateAvailable) {
        save_cache(&UpdateCache {
            last_check: unix_now(),
            update_available: matches!(result, CheckResult::UpdateAvailable),
        });
    }

    // Fetch the remote SHA separately for display (run_check uses the compare
    // API which doesn't return the remote sha directly).
    let remote = github_repo().and_then(fetch_remote_head_sha);

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

/// Path to the single-instance PID file written by `lock::acquire_lock`.
fn pid_file_path() -> PathBuf {
    global_config::global_config_dir().join("bork.pid")
}

/// Returns true if a process with this PID currently exists.
fn process_alive(pid: i32) -> bool {
    // kill(pid, 0) is the standard way to check existence without sending a signal.
    // Returns 0 on success (process exists, we can signal it), -1 with errno=ESRCH
    // when the process is gone.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Kill the bork TUI process recorded in `~/.config/bork/bork.pid` (if any) and
/// tear down the `bork-tui` tmux wrapper session, so the next `bork` launch
/// starts fresh on the rebuilt binary.
///
/// Best-effort: any failure here is logged but doesn't abort the update.
fn stop_running_instance() {
    let pid_path = pid_file_path();
    let Ok(contents) = std::fs::read_to_string(&pid_path) else {
        return;
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        // Corrupt PID file - remove it so the next launch isn't blocked.
        let _ = std::fs::remove_file(&pid_path);
        return;
    };

    // Don't shoot ourselves in the foot.
    if pid as u32 == std::process::id() {
        return;
    }

    if !process_alive(pid) {
        // Stale PID file (process crashed without releasing the lock).
        let _ = std::fs::remove_file(&pid_path);
        let _ = kill_tui_tmux_session();
        return;
    }

    println!("Stopping running bork (PID {pid})...");

    // SIGTERM triggers the handler in lock.rs which sets SIGNAL_RECEIVED; the
    // main loop polls that flag and exits cleanly, dropping the flock and
    // removing the PID file.
    unsafe { libc::kill(pid, libc::SIGTERM) };

    // Wait up to 2s for the process to exit (40 * 50ms).
    for _ in 0..40 {
        if !process_alive(pid) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    if process_alive(pid) {
        // Stuck. Escalate to SIGKILL.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        for _ in 0..20 {
            if !process_alive(pid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    // Either path may have left the PID file behind (release_lock didn't run
    // on SIGKILL, or the file was recreated). Make sure it's gone.
    let _ = std::fs::remove_file(&pid_path);
    let _ = kill_tui_tmux_session();
}

/// Tear down the tmux wrapper session that hosts the bork TUI. Errors are
/// ignored - if the session is already gone, that's the desired state.
fn kill_tui_tmux_session() -> std::io::Result<()> {
    Command::new("tmux")
        .args(["kill-session", "-t", BORK_TUI_SESSION])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

pub fn run_update() -> anyhow::Result<()> {
    let repo = resolve_source_repo()?;
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    println!("Source: {}", repo.display());
    println!();

    stop_running_instance();

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

    // Mark the cache as checked now with no update pending. Using unix_now()
    // as last_check prevents the background worker in a running TUI session
    // from immediately re-checking (and re-showing the banner) just because
    // last_check was reset to 0. The worker will re-check after CHECK_INTERVAL_SECS,
    // by which point the user should have restarted bork with the new binary.
    save_cache(&UpdateCache {
        last_check: unix_now(),
        update_available: false,
    });

    println!();
    println!("Updated to latest. Restart bork to use the new version.");

    Ok(())
}
