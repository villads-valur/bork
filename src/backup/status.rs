//! Last backup result, kept small and cheap to read for `bork backup status`
//! and the TUI indicator: `<project>/.bork/backup.json` per project,
//! `~/.config/bork/backup.json` for the global files.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATUS_FILE: &str = "backup.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupStatus {
    /// Unix seconds of the last attempt, successful or not.
    pub last_run_at: u64,
    /// Unix seconds of the last successful run. Survives later failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<u64>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Push problem in git mode. The local commit still counts as a success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub destination: String,
    /// Directory this scope was written to (`<path>/<scope>`).
    pub target: PathBuf,
    pub files: usize,
    /// Whether the last run changed anything at the destination.
    pub changed: bool,
}

pub fn project_status_path(project_root: &Path) -> PathBuf {
    project_root.join(".bork").join(STATUS_FILE)
}

pub fn global_status_path() -> PathBuf {
    crate::global_config::global_config_dir().join(STATUS_FILE)
}

pub fn read(path: &Path) -> Option<BackupStatus> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Write `status`, carrying `last_success_at` over from the previous record
/// when this run failed.
pub fn record(path: &Path, mut status: BackupStatus) -> anyhow::Result<BackupStatus> {
    if status.ok {
        status.last_success_at = Some(status.last_run_at);
    } else if status.last_success_at.is_none() {
        status.last_success_at = read(path).and_then(|prev| prev.last_success_at);
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(&status)?;
    crate::config::atomic_write(path, json.as_bytes())?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_keeps_last_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATUS_FILE);
        assert_eq!(read(&path), None);

        record(
            &path,
            BackupStatus {
                last_run_at: 100,
                ok: true,
                ..BackupStatus::default()
            },
        )
        .unwrap();
        let failed = record(
            &path,
            BackupStatus {
                last_run_at: 200,
                ok: false,
                error: Some("boom".into()),
                ..BackupStatus::default()
            },
        )
        .unwrap();
        assert_eq!(failed.last_success_at, Some(100));
        assert_eq!(read(&path).unwrap(), failed);
    }
}
