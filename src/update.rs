use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context};

fn resolve_source_repo() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to determine executable path")?;
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);

    // Walk up from e.g. /path/to/main/target/release/bork to find the directory with Cargo.toml
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

fn parse_version_from_cargo_toml(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("version") {
            continue;
        }
        let version = trimmed.split('=').nth(1)?.trim().trim_matches('"').trim();
        if !version.is_empty() {
            return Some(version.to_string());
        }
    }
    None
}

/// Check if a newer version is available on origin/main.
/// Returns true if behind, false if up to date or on error.
pub fn check_for_update() -> bool {
    let Ok(repo) = resolve_source_repo() else {
        return false;
    };

    let Ok(fetch) = Command::new("git")
        .args(["fetch", "origin", "main", "--quiet"])
        .current_dir(&repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    else {
        return false;
    };

    if !fetch.success() {
        return false;
    }

    let Ok(output) = Command::new("git")
        .args(["rev-list", "HEAD..origin/main", "--count"])
        .current_dir(&repo)
        .output()
    else {
        return false;
    };

    let count: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    count > 0
}

pub fn run_update() -> anyhow::Result<()> {
    let repo = resolve_source_repo()?;
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    println!("Source: {}", repo.display());
    println!();

    // Fetch
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

    // Pull
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

    // Build
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

    // Read the new version from Cargo.toml on disk (post-pull)
    let cargo_path = repo.join("Cargo.toml");
    let cargo_content =
        std::fs::read_to_string(&cargo_path).context("failed to read Cargo.toml")?;
    let new_version = parse_version_from_cargo_toml(&cargo_content).unwrap_or_else(|| "?".into());

    println!();
    if new_version == current {
        println!("Rebuilt v{current} with latest changes.");
    } else {
        println!("Updated: v{current} -> v{new_version}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_standard() {
        let toml = r#"
[package]
name = "bork"
version = "1.2.3"
edition = "2021"
"#;
        assert_eq!(
            parse_version_from_cargo_toml(toml),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn parse_version_with_spaces() {
        let toml = r#"version  =  "0.5.0""#;
        assert_eq!(
            parse_version_from_cargo_toml(toml),
            Some("0.5.0".to_string())
        );
    }

    #[test]
    fn parse_version_missing() {
        let toml = r#"
[package]
name = "bork"
"#;
        assert_eq!(parse_version_from_cargo_toml(toml), None);
    }
}
