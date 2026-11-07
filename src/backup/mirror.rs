//! Mirror a list of files into one destination directory: write what
//! changed, remove what no longer exists, never touch anything outside it.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the destination directory.
    pub rel: PathBuf,
    /// File or symlink on disk (symlinks are recreated, not followed).
    pub source: PathBuf,
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

/// Make `dest` contain exactly `entries`, except stale files for which
/// `retain` returns true. Sources that vanished mid-run are skipped (and their
/// stale copy removed unless retained) rather than failing the backup.
pub fn mirror(
    entries: &[Entry],
    dest: &Path,
    retain: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<MirrorReport> {
    fs::create_dir_all(dest)?;
    let mut report = MirrorReport::default();
    let mut keep: HashSet<PathBuf> = HashSet::new();

    for entry in entries {
        let Some(wrote) = sync_path(&entry.source, &dest.join(&entry.rel))? else {
            continue;
        };
        keep.insert(entry.rel.clone());
        if wrote {
            report.written.push(entry.rel.clone());
        } else {
            report.unchanged += 1;
        }
    }

    remove_stale(dest, Path::new(""), &keep, retain, &mut report.removed)?;
    report.removed.sort();
    Ok(report)
}

/// Copy one file or symlink to `target` if it differs. `Ok(None)` when the
/// source is missing or not a file; otherwise whether it wrote.
pub fn sync_path(src: &Path, target: &Path) -> anyhow::Result<Option<bool>> {
    let Ok(meta) = fs::symlink_metadata(src) else {
        return Ok(None);
    };
    if meta.file_type().is_symlink() {
        return sync_symlink(&fs::read_link(src)?, target).map(Some);
    }
    if !meta.is_file() {
        return Ok(None);
    }
    let Ok(bytes) = fs::read(src) else {
        return Ok(None);
    };
    sync_bytes(&bytes, meta.permissions().mode() & 0o777, target).map(Some)
}

/// Write `bytes` to `target` (atomically) if content or mode differ.
fn sync_bytes(bytes: &[u8], mode: u32, target: &Path) -> anyhow::Result<bool> {
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.is_file() {
            let same_mode = meta.permissions().mode() & 0o777 == mode;
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
    fs::set_permissions(target, fs::Permissions::from_mode(mode))?;
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

/// Delete files under `dest/rel` that are neither in `keep` nor retained,
/// then empty directories.
fn remove_stale(
    dest: &Path,
    rel: &Path,
    keep: &HashSet<PathBuf>,
    retain: &dyn Fn(&Path) -> bool,
    removed: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dest.join(rel))? {
        let entry = entry?;
        let child_rel = rel.join(entry.file_name());
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            remove_stale(dest, &child_rel, keep, retain, removed)?;
            if fs::read_dir(&path)?.next().is_none() {
                fs::remove_dir(&path)?;
            }
        } else if !keep.contains(&child_rel) && !retain(&child_rel) {
            fs::remove_file(&path)?;
            removed.push(child_rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn entry(src: &Path, rel: &str) -> Entry {
        Entry {
            rel: PathBuf::from(rel),
            source: src.join(rel),
        }
    }

    const NO_RETAIN: &dyn Fn(&Path) -> bool = &|_| false;

    #[test]
    fn copies_updates_and_removes() {
        let src = tempfile::tempdir().unwrap();
        let dest_root = tempfile::tempdir().unwrap();
        let dest = dest_root.path().join("proj");
        let sibling = dest_root.path().join("other/keep.txt");
        write(&sibling, "x");
        write(&src.path().join("AGENTS.md"), "v1");
        write(&src.path().join("plans/a/p.md"), "plan");
        let entries = vec![
            entry(src.path(), "AGENTS.md"),
            entry(src.path(), "plans/a/p.md"),
        ];

        let first = mirror(&entries, &dest, NO_RETAIN).unwrap();
        assert_eq!(first.written.len(), 2);
        assert_eq!(
            fs::read_to_string(dest.join("plans/a/p.md")).unwrap(),
            "plan"
        );

        let second = mirror(&entries, &dest, NO_RETAIN).unwrap();
        assert!(!second.changed());
        assert_eq!(second.unchanged, 2);

        write(&src.path().join("AGENTS.md"), "v2");
        let third = mirror(&entries[..1], &dest, NO_RETAIN).unwrap();
        assert_eq!(third.written, vec![PathBuf::from("AGENTS.md")]);
        assert_eq!(third.removed, vec![PathBuf::from("plans/a/p.md")]);
        assert_eq!(fs::read_to_string(dest.join("AGENTS.md")).unwrap(), "v2");
        assert!(!dest.join("plans").exists(), "empty dirs are pruned");
        assert!(sibling.exists(), "never touches outside dest");
    }

    #[test]
    fn retained_stale_files_survive() {
        let dest = tempfile::tempdir().unwrap();
        write(&dest.path().join("_issues/p-1/planning.md"), "kept");
        write(&dest.path().join("old.md"), "gone");
        let report = mirror(&[], dest.path(), &|rel| rel.starts_with("_issues")).unwrap();
        assert_eq!(report.removed, vec![PathBuf::from("old.md")]);
        assert!(dest.path().join("_issues/p-1/planning.md").exists());
    }

    #[test]
    fn missing_source_is_skipped_and_its_copy_removed() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(&dest.path().join("gone.md"), "old");
        let report = mirror(&[entry(src.path(), "gone.md")], dest.path(), NO_RETAIN).unwrap();
        assert_eq!(report.removed, vec![PathBuf::from("gone.md")]);
    }

    #[test]
    fn keeps_mode_and_symlinks() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let script = src.path().join("hook.sh");
        write(&script, "#!/bin/sh");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("hook.sh", src.path().join("link")).unwrap();
        let entries = vec![entry(src.path(), "hook.sh"), entry(src.path(), "link")];
        mirror(&entries, dest.path(), NO_RETAIN).unwrap();
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
        let report = mirror(&entries, dest.path(), NO_RETAIN).unwrap();
        assert_eq!(report.written, vec![PathBuf::from("hook.sh")]);
    }

    #[test]
    fn replaces_type_changes() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(&dest.path().join("x/inner/f"), "dir before");
        write(&dest.path().join("y"), "file before");
        write(&src.path().join("x"), "now a file");
        write(&src.path().join("y/z"), "under a dir");
        let entries = vec![entry(src.path(), "x"), entry(src.path(), "y/z")];
        mirror(&entries, dest.path(), NO_RETAIN).unwrap();
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
