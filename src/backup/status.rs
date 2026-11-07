//! Last result per backup file, in `~/.config/bork/backup.json`. Cheap to
//! read for `bork backup status` and the TUI indicator; also holds the
//! fingerprint that lets an unchanged backup skip rewriting the file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATUS_FILE: &str = "backup.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveStatus {
    /// Unix seconds of the last attempt, successful or not.
    pub last_run_at: u64,
    /// Unix seconds of the last successful run. Survives later failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<u64>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Size of the backup file after the last successful run.
    #[serde(default)]
    pub size: u64,
    /// Project names in the last successful archive.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Container paths of those projects (informational; ownership is
    /// checked against the archive's own manifest).
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Skipped git checkouts with no remote at the last successful run
    /// (absolute paths): not in the backup and not on any remote.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_only_repos: Vec<PathBuf>,
    /// Content fingerprint of the last archive written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Whether the last successful run rewrote the file.
    #[serde(default)]
    pub changed: bool,
}

/// All backup files bork has written, keyed by absolute file path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusFile {
    #[serde(default)]
    pub archives: BTreeMap<PathBuf, ArchiveStatus>,
}

pub fn status_path() -> PathBuf {
    crate::global_config::global_config_dir().join(STATUS_FILE)
}

pub fn read(path: &Path) -> StatusFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Store `status` for `archive`. A failed run keeps the previous
/// success time, size, projects and fingerprint.
pub fn record(path: &Path, archive: &Path, status: ArchiveStatus) -> anyhow::Result<ArchiveStatus> {
    let mut file = read(path);
    let merged = if status.ok {
        ArchiveStatus {
            last_success_at: Some(status.last_run_at),
            ..status
        }
    } else {
        let prev = file.archives.get(archive).cloned().unwrap_or_default();
        ArchiveStatus {
            last_run_at: status.last_run_at,
            ok: false,
            error: status.error,
            ..prev
        }
    };
    file.archives.insert(archive.to_path_buf(), merged.clone());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(&file)?;
    crate::config::atomic_write(path, json.as_bytes())?;
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_keeps_last_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATUS_FILE);
        let archive = Path::new("/b/bork-backup.tar.gz");
        assert_eq!(read(&path), StatusFile::default());

        record(
            &path,
            archive,
            ArchiveStatus {
                last_run_at: 100,
                ok: true,
                size: 42,
                projects: vec!["bork".into()],
                fingerprint: Some("f".into()),
                ..ArchiveStatus::default()
            },
        )
        .unwrap();
        let failed = record(
            &path,
            archive,
            ArchiveStatus {
                last_run_at: 200,
                ok: false,
                error: Some("boom".into()),
                ..ArchiveStatus::default()
            },
        )
        .unwrap();

        assert_eq!(failed.last_success_at, Some(100));
        assert_eq!(failed.size, 42);
        assert_eq!(failed.fingerprint.as_deref(), Some("f"));
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert_eq!(read(&path).archives[archive], failed);
    }
}
