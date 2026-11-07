//! Build the `.tar.gz` backup file with the system `tar`: copy the selected
//! files into a private staging directory, archive it next to the
//! destination, then rename it into place so readers only ever see a complete
//! file.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

/// One file in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// Path inside the archive, e.g. `projects/bork/AGENTS.md`.
    pub path: String,
    /// File or symlink on disk.
    pub source: PathBuf,
    /// Copy a symlink's target content instead of the link itself.
    pub follow: bool,
}

/// Every entry's content, read once. The fingerprint and the archive are
/// both built from this snapshot, so a file edited mid-run can't leave the
/// archive and its recorded fingerprint out of step.
pub struct Contents(Vec<(String, Source)>);

/// Read every entry. Read errors other than "not found" fail; a source that
/// vanished is left out.
pub fn load(entries: &[ArchiveEntry]) -> anyhow::Result<Contents> {
    let mut contents = Vec::with_capacity(entries.len());
    for entry in entries {
        contents.push((entry.path.clone(), read_source(entry)?));
    }
    Ok(Contents(contents))
}

/// Content fingerprint of `manifest` plus every entry (path, mode, bytes or
/// link target). Equal fingerprints mean the archive would be identical, so
/// the write can be skipped.
pub fn fingerprint(manifest: &[u8], contents: &Contents) -> String {
    let mut hasher = DefaultHasher::new();
    manifest.hash(&mut hasher);
    for (path, source) in &contents.0 {
        path.hash(&mut hasher);
        match source {
            Source::Missing => 0u8.hash(&mut hasher),
            Source::Link(target) => {
                1u8.hash(&mut hasher);
                target.hash(&mut hasher);
            }
            Source::File(bytes, mode) => {
                2u8.hash(&mut hasher);
                mode.hash(&mut hasher);
                bytes.hash(&mut hasher);
            }
        }
    }
    format!("{:016x}", hasher.finish())
}

/// Write `manifest.json` plus `contents` to `dest` as a gzipped tarball,
/// atomically. Returns the archive size in bytes.
pub fn write(dest: &Path, manifest: &[u8], contents: &Contents) -> anyhow::Result<u64> {
    let staging = Staging::new()?;
    stage_file(&staging.0, super::manifest::MANIFEST_FILE, manifest, 0o644)?;
    for (path, source) in &contents.0 {
        match source {
            Source::Missing => {}
            Source::Link(target) => {
                let staged = staged_path(&staging.0, path)?;
                std::os::unix::fs::symlink(target, staged)?;
            }
            Source::File(bytes, mode) => stage_file(&staging.0, path, bytes, *mode)?,
        }
    }

    let dir = dest
        .parent()
        .context("backup file has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid backup file name")?;
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));

    let result = (|| {
        let mut top: Vec<String> = fs::read_dir(&staging.0)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        top.sort();
        let output = Command::new("tar")
            // Keep macOS from adding `._*` AppleDouble files for xattrs.
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&tmp)
            .arg("-C")
            .arg(&staging.0)
            .args(&top)
            .output()
            .context("running tar")?;
        if !output.status.success() {
            bail!(
                "tar failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, dest)?;
        Ok(fs::metadata(dest)?.len())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Read `manifest.json` from an existing backup archive.
pub fn read_manifest(file: &Path) -> anyhow::Result<super::manifest::Manifest> {
    let output = Command::new("tar")
        .arg("-xzOf")
        .arg(file)
        .arg(super::manifest::MANIFEST_FILE)
        .output()
        .context("running tar")?;
    if !output.status.success() {
        bail!(
            "can't read the manifest of {}: {}",
            file.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parsing the manifest of {}", file.display()))
}

pub(crate) enum Source {
    Missing,
    Link(PathBuf),
    File(Vec<u8>, u32),
}

fn read_source(entry: &ArchiveEntry) -> anyhow::Result<Source> {
    let src = &entry.source;
    let context = || format!("reading {}", src.display());
    let meta = if entry.follow {
        fs::metadata(src)
    } else {
        fs::symlink_metadata(src)
    };
    let meta = match meta {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Source::Missing),
        Err(err) => return Err(err).with_context(context),
    };
    if meta.file_type().is_symlink() {
        return Ok(Source::Link(fs::read_link(src).with_context(context)?));
    }
    if !meta.is_file() {
        return Ok(Source::Missing);
    }
    match fs::read(src) {
        Ok(bytes) => Ok(Source::File(bytes, meta.permissions().mode() & 0o777)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Source::Missing),
        Err(err) => Err(err).with_context(context),
    }
}

/// `staging/<path>` with its parent directories created. Rejects paths that
/// could leave the staging directory.
///
/// Never follows or reuses an existing entry: every parent must be a real
/// directory created for this archive, and the target must not exist yet.
/// Two archive paths that the staging filesystem treats as the same entry
/// (case or Unicode-normalization variants) therefore fail the backup
/// instead of overwriting each other or writing through a staged symlink.
fn staged_path(staging: &Path, path: &str) -> anyhow::Result<PathBuf> {
    let rel = Path::new(path);
    let plain = rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain || path.is_empty() {
        bail!("invalid archive path {path:?}");
    }
    let mut dir = staging.to_path_buf();
    if let Some(parent) = rel.parent() {
        for part in parent.components() {
            dir.push(part);
            match fs::symlink_metadata(&dir) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => bail!("archive path {path:?} collides with another entry"),
                Err(err) if err.kind() == ErrorKind::NotFound => fs::create_dir(&dir)?,
                Err(err) => return Err(err.into()),
            }
        }
    }
    let full = staging.join(rel);
    if fs::symlink_metadata(&full).is_ok() {
        bail!("archive path {path:?} collides with another entry");
    }
    Ok(full)
}

fn stage_file(staging: &Path, path: &str, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    let full = staged_path(staging, path)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&full)
        .with_context(|| format!("staging {path}"))?;
    file.write_all(bytes)?;
    fs::set_permissions(&full, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Private temp directory, removed on drop. Also holds an extracted archive
/// during `bork import`.
pub(super) struct Staging(pub(super) PathBuf);

impl Staging {
    pub(super) fn new() -> anyhow::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        // The counter keeps two dirs made in the same instant apart.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bork-backup-{}-{nanos}-{n}", std::process::id()));
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(Self(dir))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn entry(src: &Path, rel: &str, path: &str) -> ArchiveEntry {
        ArchiveEntry {
            path: path.to_string(),
            source: src.join(rel),
            follow: false,
        }
    }

    fn list(archive: &Path) -> Vec<String> {
        let out = Command::new("tar")
            .arg("-tzf")
            .arg(archive)
            .output()
            .unwrap();
        assert!(out.status.success());
        let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.ends_with('/'))
            .map(String::from)
            .collect();
        names.sort();
        names
    }

    fn extract(archive: &Path, path: &str) -> String {
        let out = Command::new("tar")
            .arg("-xzOf")
            .arg(archive)
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn writes_a_readable_archive() {
        let src = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let dest = dest_dir.path().join("sub/bork-backup.tar.gz");
        write_file(&src.path().join("AGENTS.md"), "agents");
        write_file(&src.path().join("plans/a.md"), "plan");
        std::os::unix::fs::symlink("AGENTS.md", src.path().join("CLAUDE.md")).unwrap();
        let entries = vec![
            entry(src.path(), "AGENTS.md", "projects/p/AGENTS.md"),
            entry(src.path(), "CLAUDE.md", "projects/p/CLAUDE.md"),
            entry(src.path(), "plans/a.md", "projects/p/plans/a.md"),
            entry(src.path(), "gone.md", "projects/p/gone.md"),
        ];

        let size = write(&dest, b"{}", &load(&entries).unwrap()).unwrap();

        assert_eq!(size, fs::metadata(&dest).unwrap().len());
        assert_eq!(
            list(&dest),
            vec![
                "manifest.json",
                "projects/p/AGENTS.md",
                "projects/p/CLAUDE.md",
                "projects/p/plans/a.md",
            ]
        );
        assert_eq!(extract(&dest, "projects/p/plans/a.md"), "plan");
        assert_eq!(extract(&dest, "manifest.json"), "{}");
        let leftovers: Vec<_> = fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers,
            vec![std::ffi::OsString::from("bork-backup.tar.gz")]
        );
    }

    #[test]
    fn follow_archives_symlink_content() {
        let src = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let dest = dest_dir.path().join("b.tar.gz");
        write_file(&src.path().join("real.json"), "{\"issues\":[]}");
        std::os::unix::fs::symlink(src.path().join("real.json"), src.path().join("state.json"))
            .unwrap();
        let mut e = entry(src.path(), "state.json", "projects/p/.bork/state.json");
        e.follow = true;
        write(&dest, b"{}", &load(&[e]).unwrap()).unwrap();
        assert_eq!(
            extract(&dest, "projects/p/.bork/state.json"),
            "{\"issues\":[]}"
        );
    }

    #[test]
    fn fingerprint_tracks_content_mode_and_manifest() {
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("a.md");
        write_file(&file, "v1");
        let entries = vec![entry(src.path(), "a.md", "projects/p/a.md")];
        let fp = |manifest: &[u8]| fingerprint(manifest, &load(&entries).unwrap());
        let base = fp(b"m");
        assert_eq!(base, fp(b"m"));
        assert_ne!(base, fp(b"other"));

        write_file(&file, "v2");
        let changed = fp(b"m");
        assert_ne!(base, changed);

        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(changed, fp(b"m"));
    }

    #[test]
    fn unreadable_source_fails() {
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("private.md");
        write_file(&file, "secret");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read(&file).is_ok(); // true when running as root
        let entries = vec![entry(src.path(), "private.md", "projects/p/private.md")];
        let result = load(&entries);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        if !readable {
            assert!(result.is_err());
        }
    }

    #[test]
    fn colliding_paths_fail_instead_of_writing_through_symlinks() {
        let staging = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_file(&outside.path().join("demo-1/planning.md"), "outside");
        std::os::unix::fs::symlink(outside.path(), staging.path().join("_ISSUES")).unwrap();
        // Probe before writing anything: on a case-insensitive disk `_issues`
        // resolves to the `_ISSUES` symlink.
        let case_insensitive = fs::symlink_metadata(staging.path().join("_issues")).is_ok();

        // The symlink's own spelling must never be followed.
        assert!(stage_file(staging.path(), "_ISSUES/demo-1/planning.md", b"live", 0o644).is_err());
        // A case variant collides on case-insensitive disks and is a separate,
        // real directory on case-sensitive ones.
        let variant = stage_file(staging.path(), "_issues/demo-1/planning.md", b"live", 0o644);
        if case_insensitive {
            assert!(variant.is_err());
        } else {
            variant.unwrap();
            assert!(fs::symlink_metadata(staging.path().join("_issues"))
                .unwrap()
                .is_dir());
        }
        assert_eq!(
            fs::read_to_string(outside.path().join("demo-1/planning.md")).unwrap(),
            "outside"
        );

        stage_file(staging.path(), "a/b.md", b"1", 0o644).unwrap();
        assert!(stage_file(staging.path(), "a/b.md", b"2", 0o644).is_err());
        assert!(stage_file(staging.path(), "a/b.md/c", b"3", 0o644).is_err());
    }

    #[test]
    fn rejects_escaping_archive_paths() {
        let staging = tempfile::tempdir().unwrap();
        for bad in ["", "../x", "/abs", "a/../../x"] {
            assert!(staged_path(staging.path(), bad).is_err(), "{bad:?}");
        }
    }
}
