use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(false);

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

/// Returns true if process exists (EPERM counts as alive).
fn is_process_alive(pid: u32) -> bool {
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn acquire_lock(project_root: &Path) -> anyhow::Result<()> {
    let path = lock_path(project_root);
    let our_pid = std::process::id();

    if let Some(pid) = read_lock_pid(&path) {
        if pid != our_pid && is_process_alive(pid) {
            anyhow::bail!(
                "Another bork instance is already running (PID {}). \
                 If this is stale, remove {}",
                pid,
                path.display()
            );
        }
    }

    let dir = path.parent().unwrap();
    fs::create_dir_all(dir)?;
    fs::write(&path, format!("{}\n", our_pid))?;

    Ok(())
}

/// Remove the lock file only if it contains our PID.
pub fn release_lock(project_root: &Path) {
    let path = lock_path(project_root);

    if read_lock_pid(&path) == Some(std::process::id()) {
        let _ = fs::remove_file(&path);
    }
}

fn read_lock_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse::<u32>().ok()
}
