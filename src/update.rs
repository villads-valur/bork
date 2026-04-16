use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::global_config;

const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60; // 6 hours

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UpdateCache {
    last_check: u64,
    update_available: bool,
}

fn cache_path() -> PathBuf {
    global_config::global_config_dir().join("update_check.json")
}

fn load_cache() -> UpdateCache {
    let path = cache_path();
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &UpdateCache) {
    let path = cache_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    let _ = std::fs::write(&path, serde_json::to_string(cache).unwrap_or_default());
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

fn query_github_for_update() -> bool {
    let build_commit = env!("BORK_GIT_COMMIT");
    let repo = env!("BORK_GITHUB_REPO");

    if build_commit.is_empty() || repo.is_empty() {
        return false;
    }

    let Ok(output) = Command::new("gh")
        .args(["api", &format!("repos/{repo}/commits/main"), "--jq", ".sha"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };

    let latest = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if latest.is_empty() {
        return false;
    }

    latest != build_commit
}

/// Check if a newer version is available. Uses a cache file so we only
/// hit the GitHub API once every 6 hours.
pub fn check_for_update() -> bool {
    let cache = load_cache();
    let now = unix_now();

    if now.saturating_sub(cache.last_check) < CHECK_INTERVAL_SECS {
        return cache.update_available;
    }

    let available = query_github_for_update();

    save_cache(&UpdateCache {
        last_check: now,
        update_available: available,
    });

    available
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

    // Clear the cache so next TUI launch re-checks with the new binary's commit
    save_cache(&UpdateCache::default());

    println!();
    println!("Updated to latest. Restart bork to use the new version.");

    Ok(())
}
