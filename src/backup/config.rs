//! `[backup]` config section, layered global → project like the rest of
//! `config.toml`, with one exception: `enabled` is only read from the project
//! file, so nothing is backed up until a project opts in.

use std::fs;
use std::path::{Path, PathBuf};

use crate::toml_lite::{self, Table};

pub const DEFAULT_INTERVAL_MINUTES: u64 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Destination {
    /// Plain mirror copy into a folder (e.g. iCloud Drive, Dropbox).
    #[default]
    Folder,
    /// A local git clone: commit when something changed, push if it has a remote.
    Git,
}

impl Destination {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "folder" | "dir" | "directory" => Some(Self::Folder),
            "git" => Some(Self::Git),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Git => "git",
        }
    }
}

/// One config layer; `None` means "inherit".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialBackupConfig {
    pub enabled: Option<bool>,
    pub destination: Option<Destination>,
    pub path: Option<String>,
    pub interval_minutes: Option<u64>,
    pub wip: Option<bool>,
}

impl PartialBackupConfig {
    pub fn from_table(table: &Table) -> Self {
        Self {
            enabled: table.get("backup.enabled").and_then(|v| v.as_bool()),
            destination: table
                .get("backup.destination")
                .and_then(|v| v.as_str())
                .and_then(Destination::parse),
            path: table
                .get("backup.path")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            interval_minutes: table
                .get("backup.interval_minutes")
                .and_then(|v| v.as_u64()),
            wip: table.get("backup.wip").and_then(|v| v.as_bool()),
        }
    }

    pub fn from_file(path: &Path) -> Self {
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::from_table(&toml_lite::parse_with_warnings(&contents).0)
    }
}

/// Resolved backup settings for one scope (a project, or the global files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupConfig {
    pub enabled: bool,
    pub destination: Destination,
    /// Destination root, `~` expanded. `None` when no layer sets a path.
    pub path: Option<PathBuf>,
    pub interval_minutes: u64,
    pub wip: bool,
}

impl BackupConfig {
    /// Merge the global and project layers. Only the project layer can turn
    /// backups on.
    pub fn resolve(global: &PartialBackupConfig, project: &PartialBackupConfig) -> Self {
        Self {
            enabled: project.enabled.unwrap_or(false),
            destination: project
                .destination
                .or(global.destination)
                .unwrap_or_default(),
            path: project
                .path
                .as_deref()
                .or(global.path.as_deref())
                .map(expand_tilde),
            interval_minutes: project
                .interval_minutes
                .or(global.interval_minutes)
                .unwrap_or(DEFAULT_INTERVAL_MINUTES),
            wip: project.wip.or(global.wip).unwrap_or(true),
        }
    }

    /// Settings for the global files (`projects.json`, global `config.toml`).
    /// They are backed up whenever the global layer has a path.
    pub fn resolve_global(global: &PartialBackupConfig) -> Self {
        let mut resolved = Self::resolve(global, &PartialBackupConfig::default());
        resolved.enabled = resolved.path.is_some();
        resolved
    }
}

pub fn load_global_layer() -> PartialBackupConfig {
    PartialBackupConfig::from_file(&crate::config::global_config_path())
}

pub fn load_project_layer(project_root: &Path) -> PartialBackupConfig {
    PartialBackupConfig::from_file(&crate::config::config_path(project_root))
}

/// Resolved backup config for a project (global + project layers).
pub fn load_for_project(project_root: &Path) -> BackupConfig {
    BackupConfig::resolve(&load_global_layer(), &load_project_layer(project_root))
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

    fn layer(contents: &str) -> PartialBackupConfig {
        PartialBackupConfig::from_table(&toml_lite::parse(contents))
    }

    #[test]
    fn parses_backup_section() {
        let p = layer(
            r#"
project_name = "x"

[backup]
enabled = true
destination = "git"
path = "~/Backups/bork"
interval_minutes = 30
wip = false
"#,
        );
        assert_eq!(p.enabled, Some(true));
        assert_eq!(p.destination, Some(Destination::Git));
        assert_eq!(p.path.as_deref(), Some("~/Backups/bork"));
        assert_eq!(p.interval_minutes, Some(30));
        assert_eq!(p.wip, Some(false));
    }

    #[test]
    fn dotted_keys_work_too() {
        let p = layer("backup.enabled = true\nbackup.destination = \"folder\"\n");
        assert_eq!(p.enabled, Some(true));
        assert_eq!(p.destination, Some(Destination::Folder));
    }

    #[test]
    fn unknown_destination_and_empty_path_are_ignored() {
        let p = layer("[backup]\ndestination = \"s3\"\npath = \"\"\n");
        assert_eq!(p.destination, None);
        assert_eq!(p.path, None);
    }

    #[test]
    fn disabled_by_default() {
        let resolved = BackupConfig::resolve(&layer(""), &layer(""));
        assert!(!resolved.enabled);
        assert_eq!(resolved.destination, Destination::Folder);
        assert_eq!(resolved.path, None);
        assert_eq!(resolved.interval_minutes, DEFAULT_INTERVAL_MINUTES);
        assert!(resolved.wip);
    }

    #[test]
    fn global_enabled_does_not_opt_projects_in() {
        let global = layer("[backup]\nenabled = true\npath = \"/b\"\n");
        let resolved = BackupConfig::resolve(&global, &layer(""));
        assert!(!resolved.enabled);
        assert_eq!(resolved.path, Some(PathBuf::from("/b")));
    }

    #[test]
    fn project_overrides_global() {
        let global = layer("[backup]\ndestination = \"git\"\npath = \"/personal\"\nwip = false\n");
        let project =
            layer("[backup]\nenabled = true\ndestination = \"folder\"\npath = \"/work\"\n");
        let resolved = BackupConfig::resolve(&global, &project);
        assert!(resolved.enabled);
        assert_eq!(resolved.destination, Destination::Folder);
        assert_eq!(resolved.path, Some(PathBuf::from("/work")));
        assert!(!resolved.wip);
    }

    #[test]
    fn global_scope_enabled_by_path() {
        assert!(!BackupConfig::resolve_global(&layer("")).enabled);
        assert!(BackupConfig::resolve_global(&layer("[backup]\npath = \"/b\"\n")).enabled);
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
