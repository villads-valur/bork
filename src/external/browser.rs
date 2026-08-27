use std::process::Command;

/// Open a URL in the default browser. The error is a single line suitable
/// for the one-row status bar.
pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";

    let output = Command::new(opener)
        .arg(url)
        .output()
        .map_err(|e| format!("failed to run {opener}: {e}"))?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{opener} {url} failed")))
}
