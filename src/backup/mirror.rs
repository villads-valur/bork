//! Mirror a list of files into one destination directory: write what
//! changed, remove what no longer exists, never touch anything outside it.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context};

/// Marker file at the top of every backup directory, holding the source path.
/// Bork only writes into an empty directory or one whose marker names the same
/// source, and stale-file cleanup never enters another backup's directory.
pub const MARKER: &str = ".bork-backup-source";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the destination directory.
    pub rel: PathBuf,
    /// File or symlink on disk. Symlinks are recreated as links unless
    /// `follow` is set, in which case the target's content is copied.
    pub source: PathBuf,
    pub follow: bool,
}

/// Make `dest` a backup directory owned by `source`: create it when missing,
/// accept it when its marker names `source`, and refuse anything else (a
/// symlink, a file, a non-empty unmarked directory, another source's backup).
pub fn claim(dest: &Path, source: &Path) -> anyhow::Result<()> {
    let owner = source.display().to_string();
    let marker = dest.join(MARKER);
    match fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => match fs::read_to_string(&marker) {
            Ok(existing) if existing.trim() == owner => return Ok(()),
            Ok(existing) => bail!(
                "{} holds the backup of {}, not {owner}",
                dest.display(),
                existing.trim()
            ),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                if fs::read_dir(dest)?.next().is_some() {
                    bail!(
                        "{} is not empty and is not a bork backup; pick another path or empty it",
                        dest.display()
                    );
                }
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", marker.display())),
        },
        Ok(_) => bail!(
            "{} is a file or symlink, not a backup directory",
            dest.display()
        ),
        Err(err) if err.kind() == ErrorKind::NotFound => fs::create_dir_all(dest)?,
        Err(err) => return Err(err).with_context(|| format!("reading {}", dest.display())),
    }
    crate::config::atomic_write(&marker, format!("{owner}\n").as_bytes())
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
/// `retain` returns true, the marker, and nested backups. Sources that
/// vanished mid-run are skipped (and their stale copy removed unless
/// retained) rather than failing the backup. Call [`claim`] first.
pub fn mirror(
    entries: &[Entry],
    dest: &Path,
    retain: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<MirrorReport> {
    fs::create_dir_all(dest)?;
    let mut report = MirrorReport::default();
    let mut keep: HashSet<PathBuf> = HashSet::new();

    for entry in entries {
        let Some(wrote) = sync_path(&entry.source, entry.follow, dest, &entry.rel)? else {
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

/// Copy one file or symlink to `dest/rel` if it differs. `Ok(None)` when
/// the source is gone or not a file; otherwise whether it wrote. Read errors
/// other than "not found" fail, so an unreadable source never looks deleted.
pub fn sync_path(
    src: &Path,
    follow: bool,
    dest: &Path,
    rel: &Path,
) -> anyhow::Result<Option<bool>> {
    let meta = if follow {
        fs::metadata(src)
    } else {
        fs::symlink_metadata(src)
    };
    let meta = match meta {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", src.display())),
    };
    if !meta.file_type().is_symlink() && !meta.is_file() {
        return Ok(None);
    }
    let target = prepare_parent(dest, rel)?;
    if meta.file_type().is_symlink() {
        let link = fs::read_link(src).with_context(|| format!("reading {}", src.display()))?;
        return sync_symlink(&link, &target).map(Some);
    }
    let bytes = match fs::read(src) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", src.display())),
    };
    sync_bytes(&bytes, meta.permissions().mode() & 0o777, &target).map(Some)
}

/// Make every directory between `dest` and `dest/rel` a real directory,
/// replacing files or symlinks in the way, and return `dest/rel`. Checking
/// each component from `dest` down means a symlink left in the backup can
/// never redirect a write outside it.
fn prepare_parent(dest: &Path, rel: &Path) -> anyhow::Result<PathBuf> {
    let mut dir = dest.to_path_buf();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                bail!("invalid backup path {}", rel.display());
            };
            dir.push(name);
            match fs::symlink_metadata(&dir) {
                Ok(meta) if meta.is_dir() => continue,
                Ok(_) => fs::remove_file(&dir)?,
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            fs::create_dir(&dir)?;
        }
    }
    let Some(Component::Normal(name)) = rel.components().next_back() else {
        bail!("invalid backup path {}", rel.display());
    };
    Ok(dir.join(name))
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
    std::os::unix::fs::symlink(link, target)?;
    Ok(true)
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
        if rel.as_os_str().is_empty() && child_rel == Path::new(MARKER) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            // Another backup nested in this one is not ours to clean up.
            if fs::symlink_metadata(path.join(MARKER)).is_ok() {
                continue;
            }
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
            follow: false,
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
    fn claim_only_accepts_own_or_empty_dirs() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("demo");
        let me = Path::new("/projects/demo");

        claim(&dest, me).unwrap();
        assert_eq!(
            fs::read_to_string(dest.join(MARKER)).unwrap(),
            "/projects/demo\n"
        );
        claim(&dest, me).unwrap();
        let err = claim(&dest, Path::new("/elsewhere/demo")).unwrap_err();
        assert!(err
            .to_string()
            .contains("holds the backup of /projects/demo"));

        let live = root.path().join("live");
        write(&live.join("main/code.rs"), "code");
        assert!(claim(&live, me).is_err());

        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        assert!(claim(&link, me).is_err());
        assert!(!outside.path().join(MARKER).exists());

        let empty = root.path().join("empty");
        fs::create_dir(&empty).unwrap();
        claim(&empty, me).unwrap();
    }

    #[test]
    fn cleanup_keeps_marker_and_nested_backups() {
        let dest = tempfile::tempdir().unwrap();
        write(&dest.path().join(MARKER), "/p\n");
        write(&dest.path().join("child/.bork-backup-source"), "/c\n");
        write(&dest.path().join("child/AGENTS.md"), "child");
        write(&dest.path().join("stale.md"), "x");
        let report = mirror(&[], dest.path(), NO_RETAIN).unwrap();
        assert_eq!(report.removed, vec![PathBuf::from("stale.md")]);
        assert!(dest.path().join(MARKER).exists());
        assert!(dest.path().join("child/AGENTS.md").exists());
    }

    #[test]
    fn follow_copies_symlink_content() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(&src.path().join("real.json"), "{}");
        std::os::unix::fs::symlink(src.path().join("real.json"), src.path().join("state.json"))
            .unwrap();
        let mut e = entry(src.path(), "state.json");
        e.follow = true;
        mirror(&[e], dest.path(), NO_RETAIN).unwrap();
        let meta = fs::symlink_metadata(dest.path().join("state.json")).unwrap();
        assert!(meta.is_file());
        assert_eq!(
            fs::read_to_string(dest.path().join("state.json")).unwrap(),
            "{}"
        );
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
    fn stale_symlink_in_backup_never_redirects_writes() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("sub/note.md"), "outside");
        std::os::unix::fs::symlink(outside.path(), dest.path().join("plans")).unwrap();
        write(&src.path().join("plans/sub/note.md"), "backup");

        let report = mirror(
            &[entry(src.path(), "plans/sub/note.md")],
            dest.path(),
            NO_RETAIN,
        )
        .unwrap();

        assert_eq!(report.written, vec![PathBuf::from("plans/sub/note.md")]);
        assert_eq!(
            fs::read_to_string(outside.path().join("sub/note.md")).unwrap(),
            "outside"
        );
        assert!(fs::symlink_metadata(dest.path().join("plans"))
            .unwrap()
            .is_dir());
        assert_eq!(
            fs::read_to_string(dest.path().join("plans/sub/note.md")).unwrap(),
            "backup"
        );
    }

    #[test]
    fn unreadable_source_fails_instead_of_deleting_the_backup() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let file = src.path().join("private.md");
        write(&file, "v1");
        let entries = vec![entry(src.path(), "private.md")];
        mirror(&entries, dest.path(), NO_RETAIN).unwrap();

        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            return; // Running as root: permissions don't apply.
        }
        assert!(mirror(&entries, dest.path(), NO_RETAIN).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            fs::read_to_string(dest.path().join("private.md")).unwrap(),
            "v1"
        );
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
