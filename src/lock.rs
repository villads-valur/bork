use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Held for the lifetime of the process; dropping it releases the flock.
static LOCK_FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn signal_received() -> bool {
    SIGNAL_RECEIVED.load(Ordering::Relaxed)
}

/// SIGINT is already captured by crossterm in raw mode.
pub fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, signal_handler as libc::sighandler_t);
        libc::signal(libc::SIGHUP, signal_handler as libc::sighandler_t);
    }
}

extern "C" fn signal_handler(_sig: libc::c_int) {
    SIGNAL_RECEIVED.store(true, Ordering::Relaxed);
}

fn lock_path(project_root: &Path) -> PathBuf {
    project_root.join(".bork").join("bork.pid")
}

/// Acquire an exclusive file lock (flock) on the PID file.
///
/// Unlike the previous check-then-write approach, flock is atomic and
/// race-free. The OS releases the lock automatically when the process exits
/// (including crashes and battery death), so stale locks are impossible.
pub fn acquire_lock(project_root: &Path) -> anyhow::Result<()> {
    let path = lock_path(project_root);
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir)?;

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;

    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let existing_pid = read_pid_from_file(&mut file).unwrap_or(0);
            anyhow::bail!(
                "Another bork instance is already running (PID {existing_pid}). \
                 If this is stale, remove {}",
                path.display()
            );
        }
        return Err(err.into());
    }

    // We hold the lock. Write our PID (truncate first in case old content was longer).
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()?;

    // Keep the File alive so the flock is held for the process lifetime.
    if let Ok(mut guard) = LOCK_FILE.lock() {
        *guard = Some(file);
    }

    Ok(())
}

/// Release the lock by dropping the held file descriptor.
pub fn release_lock(project_root: &Path) {
    if let Ok(mut guard) = LOCK_FILE.lock() {
        if guard.is_some() {
            // Drop the File, which closes the fd and releases the flock.
            *guard = None;
            // Remove the PID file for tidiness (not required for correctness).
            let _ = fs::remove_file(lock_path(project_root));
        }
    }
}

fn read_pid_from_file(file: &mut File) -> Option<u32> {
    file.seek(std::io::SeekFrom::Start(0)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse::<u32>().ok()
}
