use std::process::Command;

fn main() {
    // Embed the current git commit hash
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=BORK_GIT_COMMIT={}", commit.trim());

    // Embed the GitHub repo slug (owner/repo) from the remote URL
    let remote = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let slug = parse_repo_slug(remote.trim());
    println!("cargo:rustc-env=BORK_GITHUB_REPO={slug}");

    // Only rerun if HEAD changes (new commits)
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/");
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
