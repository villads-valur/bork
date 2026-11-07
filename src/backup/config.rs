//! `[backup]` config. `enabled` is the per-project opt-in and is only read
//! from `<project>/.bork/config.toml`. `path` locates the single backup file,
//! so it is only read from the global `~/.config/bork/config.toml`. There is
//! no schedule setting: backups run daily (see `backup::is_due`) and on
//! `bork backup`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::toml_lite::{self, Table};

/// One config file's `[backup]` keys; `None` when unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupLayer {
    pub enabled: Option<bool>,
    pub path: Option<String>,
}

impl BackupLayer {
    pub fn from_table(table: &Table) -> Self {
        Self {
            enabled: table.get("backup.enabled").and_then(|v| v.as_bool()),
            path: table
                .get("backup.path")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
        }
    }

    pub fn from_file(path: &Path) -> Self {
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::from_table(&toml_lite::parse_with_warnings(&contents).0)
    }
}

/// Where the backup file is written (global config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSettings {
    /// Folder holding `bork-backup.tar.gz`, `~` expanded. `None` when unset.
    pub path: Option<PathBuf>,
}

impl BackupSettings {
    pub fn from_layer(global: &BackupLayer) -> Self {
        Self {
            path: global.path.as_deref().map(expand_tilde),
        }
    }
}

pub fn load_settings() -> BackupSettings {
    BackupSettings::from_layer(&BackupLayer::from_file(&crate::config::global_config_path()))
}

/// Whether the project opted in to backups (project file only, default off).
pub fn project_enabled(project_root: &Path) -> bool {
    BackupLayer::from_file(&crate::config::config_path(project_root))
        .enabled
        .unwrap_or(false)
}

pub fn expand_tilde(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    expand_tilde_with(path, home.as_deref())
}

fn expand_tilde_with(path: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return home.to_path_buf();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(contents: &str) -> BackupLayer {
        BackupLayer::from_table(&toml_lite::parse(contents))
    }

    #[test]
    fn parses_backup_section() {
        let p = layer(
            r#"
project_name = "x"

[backup]
enabled = true
path = "~/Backups"
"#,
        );
        assert_eq!(p.enabled, Some(true));
        assert_eq!(p.path.as_deref(), Some("~/Backups"));
    }

    #[test]
    fn dotted_keys_work_too() {
        let p = layer("backup.enabled = true\nbackup.path = \"/b\"\n");
        assert_eq!(p.enabled, Some(true));
        assert_eq!(p.path.as_deref(), Some("/b"));
    }

    #[test]
    fn empty_path_is_ignored() {
        assert_eq!(layer("[backup]\npath = \"\"\n").path, None);
    }

    #[test]
    fn settings_defaults() {
        let settings = BackupSettings::from_layer(&layer(""));
        assert_eq!(settings.path, None);
    }

    #[test]
    fn project_opt_in_is_project_file_only() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!project_enabled(dir.path()));
        std::fs::create_dir_all(dir.path().join(".bork")).unwrap();
        std::fs::write(
            dir.path().join(".bork/config.toml"),
            "[backup]\nenabled = true\n",
        )
        .unwrap();
        assert!(project_enabled(dir.path()));
    }

    #[test]
    fn expands_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(
            expand_tilde_with("~", Some(home)),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            expand_tilde_with("~/Backups", Some(home)),
            PathBuf::from("/home/me/Backups")
        );
        assert_eq!(expand_tilde_with("/abs", Some(home)), PathBuf::from("/abs"));
        assert_eq!(
            expand_tilde_with("~other/x", Some(home)),
            PathBuf::from("~other/x")
        );
        assert_eq!(expand_tilde_with("~/x", None), PathBuf::from("~/x"));
    }
}
