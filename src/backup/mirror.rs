//! Mirror a list of entries into one destination directory: write what
//! changed, remove what no longer exists, never touch anything outside it.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Where an entry's content comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file or symlink on disk (symlinks are recreated, not followed).
    Path(PathBuf),
    /// Generated content, e.g. the manifest.
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the destination directory.
    pub rel: PathBuf,
    pub source: Source,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorReport {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub unchanged: usize,
}

impl MirrorReport {
    pub fn changed(&self) -> bool {
        !self.written.is_empty() || !self.removed.is_empty()
    }
}

/// Make `dest` contain exactly `entries`. Sources that vanished mid-run are
/// skipped (and their stale copy removed) rather than failing the backup.
pub fn mirror(entries: &[Entry], dest: &Path) -> anyhow::Result<MirrorReport> {
    fs::create_dir_all(dest)?;
    let mut report = MirrorReport::default();
    let mut keep: HashSet<PathBuf> = HashSet::new();

    for entry in entries {
        let target = dest.join(&entry.rel);
        let wrote = match &entry.source {
            Source::Bytes(bytes) => sync_bytes(bytes, None, &target)?,
            Source::Path(src) => {
                let Ok(meta) = fs::symlink_metadata(src) else {
                    continue;
                };
                if meta.file_type().is_symlink() {
                    sync_symlink(&fs::read_link(src)?, &target)?
                } else if meta.is_file() {
                    let Ok(bytes) = fs::read(src) else {
                        continue;
                    };
                    sync_bytes(&bytes, Some(meta.permissions().mode()), &target)?
                } else {
                    continue;
                }
            }
        };
        keep.insert(entry.rel.clone());
        if wrote {
            report.written.push(entry.rel.clone());
        } else {
            report.unchanged += 1;
        }
    }

    remove_stale(dest, Path::new(""), &keep, &mut report.removed)?;
    report.removed.sort();
    Ok(report)
}

/// Write `bytes` to `target` if it differs. Returns whether it wrote.
fn sync_bytes(bytes: &[u8], mode: Option<u32>, target: &Path) -> anyhow::Result<bool> {
    let mode = mode.map(|m| m & 0o777);
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.is_file() {
            let same_mode = mode.is_none_or(|m| meta.permissions().mode() & 0o777 == m);
            if same_mode && fs::read(target).is_ok_and(|existing| existing == bytes) {
                return Ok(false);
            }
        } else {
            remove_any(target)?;
        }
    }
    if let Some(parent) = target.parent() {
        ensure_dir(parent)?;
    }
    crate::config::atomic_write(target, bytes)?;
    if let Some(mode) = mode {
        fs::set_permissions(target, fs::Permissions::from_mode(mode))?;
    }
    Ok(true)
}

fn sync_symlink(link: &Path, target: &Path) -> anyhow::Result<bool> {
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() && fs::read_link(target).is_ok_and(|t| t == link) {
            return Ok(false);
        }
        remove_any(target)?;
    }
    if let Some(parent) = target.parent() {
        ensure_dir(parent)?;
    }
    std::os::unix::fs::symlink(link, target)?;
    Ok(true)
}

/// `create_dir_all`, replacing a file or symlink that sits where a directory
/// now has to be.
fn ensure_dir(dir: &Path) -> anyhow::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(dir) {
        if meta.is_dir() {
            return Ok(());
        }
        fs::remove_file(dir)?;
    } else if let Some(parent) = dir.parent() {
        ensure_dir(parent)?;
    }
    fs::create_dir_all(dir)?;
    Ok(())
}

fn remove_any(path: &Path) -> anyhow::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Delete files under `dest/rel` not in `keep`, then empty directories.
fn remove_stale(
    dest: &Path,
    rel: &Path,
    keep: &HashSet<PathBuf>,
    removed: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dest.join(rel))? {
        let entry = entry?;
        let child_rel = rel.join(entry.file_name());
        let path = entry.path();
        let file_type = entry.file_type()?;
        // Defensive: a destination root must never lose a repo's metadata.
        if entry.file_name() == ".git" {
            continue;
        }
        if file_type.is_dir() {
            remove_stale(dest, &child_rel, keep, removed)?;
            if fs::read_dir(&path)?.next().is_none() {
                fs::remove_dir(&path)?;
            }
        } else if !keep.contains(&child_rel) {
            fs::remove_file(&path)?;
            removed.push(child_rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rel: &str, source: Source) -> Entry {
        Entry {
            rel: PathBuf::from(rel),
            source,
        }
    }

    #[test]
    fn copies_updates_and_removes() {
        let src = tempfile::tempdir().unwrap();
        let dest_root = tempfile::tempdir().unwrap();
        let dest = dest_root.path().join("proj");
        let sibling = dest_root.path().join("other/keep.txt");
        fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        fs::write(&sibling, "x").unwrap();

        fs::create_dir_all(src.path().join("plans/a")).unwrap();
        fs::write(src.path().join("AGENTS.md"), "v1").unwrap();
        fs::write(src.path().join("plans/a/p.md"), "plan").unwrap();
        let entries = vec![
            entry("AGENTS.md", Source::Path(src.path().join("AGENTS.md"))),
            entry(
                "plans/a/p.md",
                Source::Path(src.path().join("plans/a/p.md")),
            ),
            entry("manifest.json", Source::Bytes(b"{}".to_vec())),
        ];

        let first = mirror(&entries, &dest).unwrap();
        assert_eq!(first.written.len(), 3);
        assert_eq!(
            fs::read_to_string(dest.join("plans/a/p.md")).unwrap(),
            "plan"
        );

        let second = mirror(&entries, &dest).unwrap();
        assert!(!second.changed());
        assert_eq!(second.unchanged, 3);

        fs::write(src.path().join("AGENTS.md"), "v2").unwrap();
        let third = mirror(&entries[..1], &dest).unwrap();
        assert_eq!(third.written, vec![PathBuf::from("AGENTS.md")]);
        assert_eq!(
            third.removed,
            vec![
                PathBuf::from("manifest.json"),
                PathBuf::from("plans/a/p.md")
            ]
        );
        assert_eq!(fs::read_to_string(dest.join("AGENTS.md")).unwrap(), "v2");
        assert!(!dest.join("plans").exists(), "empty dirs are pruned");
        assert!(sibling.exists(), "never touches outside dest");
    }

    #[test]
    fn missing_source_is_skipped_and_its_copy_removed() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        fs::write(dest.path().join("gone.md"), "old").unwrap();
        let entries = vec![entry("gone.md", Source::Path(src.path().join("gone.md")))];
        let report = mirror(&entries, dest.path()).unwrap();
        assert_eq!(report.removed, vec![PathBuf::from("gone.md")]);
    }

    #[test]
    fn keeps_mode_and_symlinks() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let script = src.path().join("hook.sh");
        fs::write(&script, "#!/bin/sh").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("hook.sh", src.path().join("link")).unwrap();
        let entries = vec![
            entry("hook.sh", Source::Path(script.clone())),
            entry("link", Source::Path(src.path().join("link"))),
        ];
        mirror(&entries, dest.path()).unwrap();
        let mode = fs::metadata(dest.path().join("hook.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            fs::read_link(dest.path().join("link")).unwrap(),
            PathBuf::from("hook.sh")
        );

        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        let report = mirror(&entries, dest.path()).unwrap();
        assert_eq!(report.written, vec![PathBuf::from("hook.sh")]);
    }

    #[test]
    fn replaces_type_changes() {
        let dest = tempfile::tempdir().unwrap();
        fs::create_dir_all(dest.path().join("x/inner")).unwrap();
        fs::write(dest.path().join("y"), "file").unwrap();
        let entries = vec![
            entry("x", Source::Bytes(b"now a file".to_vec())),
            entry("y/z", Source::Bytes(b"under a dir".to_vec())),
        ];
        mirror(&entries, dest.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dest.path().join("x")).unwrap(),
            "now a file"
        );
        assert_eq!(
            fs::read_to_string(dest.path().join("y/z")).unwrap(),
            "under a dir"
        );
    }
}
