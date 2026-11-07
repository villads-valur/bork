//! Build the `.tar.gz` backup file with the system `tar`: copy the selected
//! files into a private staging directory, archive it next to the
//! destination, then rename it into place so readers only ever see a complete
//! file.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
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

/// Content fingerprint of `manifest` plus every entry (path, mode, bytes or
/// link target). Equal fingerprints mean the archive would be identical, so
/// the write can be skipped. Read errors other than "not found" fail.
pub fn fingerprint(manifest: &[u8], entries: &[ArchiveEntry]) -> anyhow::Result<String> {
    let mut hasher = DefaultHasher::new();
    manifest.hash(&mut hasher);
    for entry in entries {
        entry.path.hash(&mut hasher);
        match read_source(entry)? {
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
    Ok(format!("{:016x}", hasher.finish()))
}

/// Write `manifest.json` plus `entries` to `dest` as a gzipped tarball,
/// atomically. Returns the archive size in bytes.
pub fn write(dest: &Path, manifest: &[u8], entries: &[ArchiveEntry]) -> anyhow::Result<u64> {
    let staging = Staging::new()?;
    stage_file(&staging.0, super::manifest::MANIFEST_FILE, manifest, 0o644)?;
    for entry in entries {
        match read_source(entry)? {
            Source::Missing => {}
            Source::Link(target) => {
                let path = staged_path(&staging.0, &entry.path)?;
                std::os::unix::fs::symlink(target, path)?;
            }
            Source::File(bytes, mode) => stage_file(&staging.0, &entry.path, &bytes, mode)?,
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

enum Source {
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
fn staged_path(staging: &Path, path: &str) -> anyhow::Result<PathBuf> {
    let rel = Path::new(path);
    let plain = rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain || path.is_empty() {
        bail!("invalid archive path {path:?}");
    }
    let full = staging.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(full)
}

fn stage_file(staging: &Path, path: &str, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    let full = staged_path(staging, path)?;
    fs::write(&full, bytes)?;
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
        let dir = std::env::temp_dir().join(format!("bork-backup-{}-{nanos}", std::process::id()));
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

        let size = write(&dest, b"{}", &entries).unwrap();

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
        write(&dest, b"{}", &[e]).unwrap();
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
        let base = fingerprint(b"m", &entries).unwrap();
        assert_eq!(base, fingerprint(b"m", &entries).unwrap());
        assert_ne!(base, fingerprint(b"other", &entries).unwrap());

        write_file(&file, "v2");
        let changed = fingerprint(b"m", &entries).unwrap();
        assert_ne!(base, changed);

        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(changed, fingerprint(b"m", &entries).unwrap());
    }

    #[test]
    fn unreadable_source_fails() {
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("private.md");
        write_file(&file, "secret");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read(&file).is_ok(); // true when running as root
        let entries = vec![entry(src.path(), "private.md", "projects/p/private.md")];
        let result = fingerprint(b"m", &entries);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        if !readable {
            assert!(result.is_err());
        }
    }

    #[test]
    fn rejects_escaping_archive_paths() {
        let staging = tempfile::tempdir().unwrap();
        for bad in ["", "../x", "/abs", "a/../../x"] {
            assert!(staged_path(staging.path(), bad).is_err(), "{bad:?}");
        }
    }
}
