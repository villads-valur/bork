use std::process::Command;

fn main() {
    // Embed the current git commit hash. Empty if we're not in a git tree
    // (e.g. source tarball builds); the runtime falls back to `git rev-parse`
    // in the resolved source repo so this is a soft failure.
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let commit = commit.trim();
    if commit.is_empty() {
        println!(
            "cargo:warning=BORK_GIT_COMMIT is empty (git rev-parse HEAD failed). \
             Update check will fall back to runtime resolution."
        );
    }
    println!("cargo:rustc-env=BORK_GIT_COMMIT={commit}");

    // Embed the GitHub repo slug (owner/repo) from the remote URL
    let remote = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let slug = parse_repo_slug(remote.trim());
    if slug.is_empty() {
        println!(
            "cargo:warning=BORK_GITHUB_REPO is empty (no origin remote). \
             Update check will be disabled."
        );
    }
    println!("cargo:rustc-env=BORK_GITHUB_REPO={slug}");

    // Rerun when HEAD or the index changes. `.git/refs/heads/` as a directory
    // trigger doesn't catch nested branch refs reliably, so prefer .git/index
    // which mutates on every checkout/commit.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=build.rs");
}

fn parse_repo_slug(url: &str) -> String {
    // Handle SSH: git@github.com:owner/repo.git
    // Handle HTTPS: https://github.com/owner/repo.git
    let path = url
        .trim_end_matches(".git")
        .rsplit_once(':')
        .map(|(_, path)| path)
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .unwrap_or("");

    // For SSH, the split on ':' gives "owner/repo"
    // For HTTPS strip, we already have "owner/repo"
    path.to_string()
}
