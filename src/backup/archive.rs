//! Build the `.tar.gz` backup file with the system `tar`: copy the selected
//! files into a private staging directory, archive it next to the
//! destination, then rename it into place so readers only ever see a complete
//! file.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

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
    /// Content already read by the caller (bytes, mode), used instead of
    /// reading `source` again. For files whose parsed content shaped the
    /// rest of the archive, so the archive holds exactly what was parsed.
    pub captured: Option<(Vec<u8>, u32)>,
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
///
/// The staging dir, temp file and `tar` child are registered as in flight
/// while this runs, so a quitting TUI can wait for it ([`in_flight`]) or
/// stop it and clean up ([`abort_in_flight`]).
pub fn write(dest: &Path, manifest: &[u8], contents: &Contents) -> anyhow::Result<u64> {
    let dir = dest
        .parent()
        .context("backup file has no parent directory")?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid backup file name")?;
    let tmp = dir.join(format!(".{name}{TMP_INFIX}{}", std::process::id()));

    let staging = Staging::new()?;
    let _registered = Registered::new(&staging.0, &tmp)?;
    stage_file(&staging.0, super::manifest::MANIFEST_FILE, manifest, 0o644)?;
    for (path, source) in &contents.0 {
        if aborted() {
            bail!(ABORTED);
        }
        match source {
            Source::Missing => {}
            Source::Link(target) => {
                let staged = staged_path(&staging.0, path)?;
                std::os::unix::fs::symlink(target, staged)?;
            }
            Source::File(bytes, mode) => stage_file(&staging.0, path, bytes, *mode)?,
        }
    }

    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let result = (|| {
        let mut top: Vec<String> = fs::read_dir(&staging.0)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        top.sort();
        let mut command = Command::new("tar");
        command
            // Keep macOS from adding `._*` AppleDouble files for xattrs.
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&tmp)
            .arg("-C")
            .arg(&staging.0)
            .args(&top)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Its own group, so an abort also stops a `gzip` it spawns.
            .process_group(0);
        let child = spawn_registered(&staging.0, &mut command)?;
        let output = child.wait_with_output().context("running tar")?;
        if aborted() {
            bail!(ABORTED);
        }
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

/// Error of a write stopped by [`abort_in_flight`].
const ABORTED: &str = "backup stopped because bork quit";

/// A `write` in progress: what to clean up if it has to be stopped.
struct InFlight {
    staging: PathBuf,
    tmp: PathBuf,
    tar: Option<u32>,
}

/// Every `write` in progress in this process (one at a time under the
/// backup lock; tests write concurrently).
static IN_FLIGHT: Mutex<Vec<InFlight>> = Mutex::new(Vec::new());
/// Set by [`stop_new_writes`]: no new `write` may start.
static STOPPING: AtomicBool = AtomicBool::new(false);
/// Set by [`abort_in_flight`]: running writes stop at the next step.
static ABORT: AtomicBool = AtomicBool::new(false);

fn in_flight_list() -> std::sync::MutexGuard<'static, Vec<InFlight>> {
    IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Registers a write in [`IN_FLIGHT`] until dropped.
struct Registered(PathBuf);

impl Registered {
    fn new(staging: &Path, tmp: &Path) -> anyhow::Result<Self> {
        let mut list = in_flight_list();
        if STOPPING.load(Ordering::SeqCst) {
            bail!(ABORTED);
        }
        list.push(InFlight {
            staging: staging.to_path_buf(),
            tmp: tmp.to_path_buf(),
            tar: None,
        });
        Ok(Self(staging.to_path_buf()))
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        in_flight_list().retain(|w| w.staging != self.0);
    }
}

/// Spawn `tar` and record its pid under the registry lock, so an abort
/// either sees the pid or stops the spawn.
fn spawn_registered(staging: &Path, command: &mut Command) -> anyhow::Result<Child> {
    let mut list = in_flight_list();
    if ABORT.load(Ordering::SeqCst) {
        bail!(ABORTED);
    }
    let child = command.spawn().context("running tar")?;
    if let Some(write) = list.iter_mut().find(|w| w.staging == staging) {
        write.tar = Some(child.id());
    }
    Ok(child)
}

/// Whether the current write was stopped by [`abort_in_flight`]; such a
/// failure isn't recorded, since bork is quitting, not the backup broken.
pub fn aborted() -> bool {
    ABORT.load(Ordering::SeqCst)
}

/// Whether a backup file is being written right now.
pub fn in_flight() -> bool {
    !in_flight_list().is_empty()
}

/// Refuse new writes from now on (bork is quitting). Writes already
/// running carry on.
pub fn stop_new_writes() {
    let _list = in_flight_list();
    STOPPING.store(true, Ordering::SeqCst);
}

/// Stop every running write: kill its `tar` and remove its temp file and
/// staging dir. Implies [`stop_new_writes`]. The writer thread sees the
/// abort and fails without recording it.
pub fn abort_in_flight() {
    let list = in_flight_list();
    STOPPING.store(true, Ordering::SeqCst);
    ABORT.store(true, Ordering::SeqCst);
    for write in list.iter() {
        if let Some(pid) = write.tar.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: plain syscall; the group is the tar child's own
            // (`process_group(0)`), still unreaped while it's registered.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        // Removing the temp file races a rename only if tar just finished:
        // either the backup is in place or the rename fails. Both are fine.
        let _ = fs::remove_file(&write.tmp);
        let _ = fs::remove_dir_all(&write.staging);
    }
}

/// Remove temp files and staging dirs that crashed or killed bork
/// processes left behind: `.bork-backup-*{TMP_INFIX}<pid>` files in
/// `backup_dir` and `{STAGING_PREFIX}<pid>-<digits>` dirs in `temp_dir`,
/// when `<pid>` is no longer running. Anything else, symlinks, entries of
/// other users and those of live processes (including this one) are left
/// alone. Best effort: errors are ignored. Call it under the backup lock.
pub fn remove_stale(backup_dir: Option<&Path>, temp_dir: &Path) {
    let stale = |dir: &Path, owner_pid: fn(&str) -> Option<u32>, want_dir: bool| {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let pid = owner_pid(entry.file_name().to_str()?)?;
                let meta = fs::symlink_metadata(entry.path()).ok()?;
                let kind_ok = if want_dir {
                    meta.file_type().is_dir()
                } else {
                    meta.file_type().is_file()
                };
                // SAFETY: geteuid has no preconditions.
                let mine = meta.uid() == unsafe { libc::geteuid() };
                (kind_ok && mine && !pid_alive(pid)).then(|| entry.path())
            })
            .collect::<Vec<_>>()
    };
    if let Some(dir) = backup_dir {
        for file in stale(dir, tmp_file_pid, false) {
            let _ = fs::remove_file(file);
        }
    }
    for dir in stale(temp_dir, staging_dir_pid, true) {
        let _ = fs::remove_dir_all(dir);
    }
}

/// The pid in a temp file name `.bork-backup-<rest>{TMP_INFIX}<pid>`.
fn tmp_file_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('.')?.strip_prefix(STAGING_PREFIX)?;
    let (file, pid) = rest.rsplit_once(TMP_INFIX)?;
    if file.is_empty() {
        return None;
    }
    digits(pid)
}

/// The pid in a staging dir name `{STAGING_PREFIX}<pid>-<digits>`.
fn staging_dir_pid(name: &str) -> Option<u32> {
    let (pid, suffix) = name.strip_prefix(STAGING_PREFIX)?.split_once('-')?;
    digits(suffix)?;
    digits(pid)
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Whether `pid` may still be running. Only "no such process" counts as
/// dead; this process and anything unparseable count as alive.
fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
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
    if let Some((bytes, mode)) = &entry.captured {
        return Ok(Source::File(bytes.clone(), *mode));
    }
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

/// The in-progress backup file is `.<file name>{TMP_INFIX}<pid>`.
pub const TMP_INFIX: &str = ".tmp.";
/// The staging dir is `{STAGING_PREFIX}<pid>-<nanos><n>` under the temp dir
/// (nanos zero-padded, `n` a per-process counter).
pub const STAGING_PREFIX: &str = "bork-backup-";

impl Staging {
    pub(super) fn new() -> anyhow::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        // The counter keeps two dirs made in the same instant apart. It joins
        // the nanos digit run, which `schedule::same_error` masks.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "{STAGING_PREFIX}{}-{nanos:09}{n}",
            std::process::id()
        ));
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
pub(crate) mod tests {
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
            captured: None,
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
    fn captured_content_is_archived_without_rereading() {
        let src = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let dest = dest_dir.path().join("b.tar.gz");
        write_file(&src.path().join("state.json"), "on disk now");
        let mut e = entry(src.path(), "state.json", "projects/p/.bork/state.json");
        e.captured = Some((b"what was parsed".to_vec(), 0o644));
        write(&dest, b"{}", &load(&[e]).unwrap()).unwrap();
        assert_eq!(
            extract(&dest, "projects/p/.bork/state.json"),
            "what was parsed"
        );
    }

    #[test]
    fn rejects_escaping_archive_paths() {
        let staging = tempfile::tempdir().unwrap();
        for bad in ["", "../x", "/abs", "a/../../x"] {
            assert!(staged_path(staging.path(), bad).is_err(), "{bad:?}");
        }
    }

    /// A pid that just exited (its child was reaped), so nothing runs as it.
    pub(crate) fn dead_pid() -> u32 {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn parses_owner_pids_from_generated_names_only() {
        assert_eq!(tmp_file_pid(".bork-backup-mac.tar.gz.tmp.42"), Some(42));
        assert_eq!(tmp_file_pid(".bork-backup-mac--p.tar.gz.tmp.7"), Some(7));
        assert_eq!(tmp_file_pid("bork-backup-mac.tar.gz.tmp.42"), None);
        assert_eq!(tmp_file_pid(".notes.tar.gz.tmp.42"), None);
        assert_eq!(tmp_file_pid(".bork-backup-mac.tar.gz.tmp.4x"), None);
        assert_eq!(tmp_file_pid(".bork-backup-.tmp.42"), None);
        assert_eq!(staging_dir_pid("bork-backup-42-0000012340"), Some(42));
        assert_eq!(staging_dir_pid("bork-backup-42-"), None);
        assert_eq!(staging_dir_pid("bork-backup-mac.tar.gz"), None);
        assert_eq!(staging_dir_pid("bork-backup-42-12-3"), None);
    }

    #[test]
    fn removes_only_stale_temp_files_and_staging_dirs() {
        let backup = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let dead = dead_pid();
        let me = std::process::id();
        let b = backup.path();
        let t = tmp.path();

        let stale_file = b.join(format!(".bork-backup-mac.tar.gz.tmp.{dead}"));
        let live_file = b.join(format!(".bork-backup-mac.tar.gz.tmp.{me}"));
        let other_file = b.join(format!(".notes.tar.gz.tmp.{dead}"));
        let backup_file = b.join("bork-backup-mac.tar.gz");
        let file_dir = b.join(format!(".bork-backup-x.tar.gz.tmp.{dead}"));
        let file_link = b.join(format!(".bork-backup-y.tar.gz.tmp.{dead}"));
        for file in [&stale_file, &live_file, &other_file, &backup_file] {
            write_file(file, "x");
        }
        fs::create_dir(&file_dir).unwrap();
        write_file(&elsewhere.path().join("keep"), "x");
        std::os::unix::fs::symlink(elsewhere.path().join("keep"), &file_link).unwrap();

        let stale_dir = t.join(format!("bork-backup-{dead}-0000012340"));
        let live_dir = t.join(format!("bork-backup-{me}-0000012340"));
        let odd_dir = t.join(format!("bork-backup-{dead}-abc"));
        let dir_file = t.join(format!("bork-backup-{dead}-1"));
        let dir_link = t.join(format!("bork-backup-{dead}-2"));
        write_file(&stale_dir.join("projects/p/AGENTS.md"), "x");
        write_file(&live_dir.join("manifest.json"), "x");
        fs::create_dir(&odd_dir).unwrap();
        write_file(&dir_file, "x");
        std::os::unix::fs::symlink(elsewhere.path(), &dir_link).unwrap();

        remove_stale(Some(b), t);

        assert!(!stale_file.exists());
        assert!(!stale_dir.exists());
        for kept in [
            &live_file,
            &other_file,
            &backup_file,
            &file_dir,
            &live_dir,
            &odd_dir,
            &dir_file,
        ] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        assert!(fs::symlink_metadata(&file_link).is_ok());
        assert!(fs::symlink_metadata(&dir_link).is_ok());
        assert!(elsewhere.path().join("keep").exists());
    }

    #[test]
    fn missing_backup_folder_still_cleans_the_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = tmp.path().join(format!("bork-backup-{}-1", dead_pid()));
        fs::create_dir(&stale).unwrap();

        remove_stale(Some(&tmp.path().join("missing")), tmp.path());
        assert!(!stale.exists());
    }
}
