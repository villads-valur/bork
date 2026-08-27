use std::thread;
use std::time::{Duration, Instant};
#[cfg(not(test))]
use std::{collections::HashSet, process::Command};
#[cfg(all(not(test), target_os = "linux"))]
use std::{fs, path::Path};

/// Env var exported into every agent pane at launch and inherited by the
/// commands it starts. Linux uses it as an additional cleanup fallback.
pub const BORK_SESSION_VAR: &str = "BORK_SESSION";

#[cfg(not(test))]
const TERM_GRACE: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(1);

/// Resolve the POSIX session IDs that own a tmux session's pane processes.
/// Job-control process groups can change, but foreground jobs, background
/// jobs, and reparented children retain this session ID unless they explicitly
/// call `setsid`.
pub fn process_session_ids(pids: &[i32]) -> Vec<i32> {
    let mut sessions: Vec<i32> = pids
        .iter()
        .filter_map(|pid| process_session_id(*pid))
        .collect();
    sessions.sort_unstable();
    sessions.dedup();
    sessions
}

/// Kill processes that outlived their tmux panes.
///
/// The pane's POSIX session IDs cover normal agent descendants plus detached
/// `nohup` jobs after they reparent to PID 1. On Linux, the inherited
/// `BORK_SESSION` environment marker additionally finds descendants that
/// escaped with `setsid`. Other platforms use the POSIX session IDs only.
///
/// Stubbed under test so ordinary unit tests never signal real processes.
#[cfg(not(test))]
pub fn kill_session_survivors(_session_name: &str, process_sessions: &[i32]) {
    let self_pid = std::process::id() as i32;
    let survivors: HashSet<i32> = current_user_pids()
        .into_iter()
        .filter(|pid| *pid > 1 && *pid != self_pid)
        .filter(|pid| process_session_id(*pid).is_some_and(|sid| process_sessions.contains(&sid)))
        .collect();
    #[cfg(target_os = "linux")]
    let survivors: HashSet<i32> = survivors
        .into_iter()
        .chain(environment_marker_pids(_session_name, self_pid))
        .collect();

    terminate_pids(&survivors.into_iter().collect::<Vec<_>>(), TERM_GRACE);
}

#[cfg(test)]
pub fn kill_session_survivors(_session_name: &str, _process_sessions: &[i32]) {}

/// SIGTERM each PID, wait up to `grace`, then SIGKILL survivors.
fn terminate_pids(pids: &[i32], grace: Duration) {
    let mut alive: Vec<i32> = pids
        .iter()
        .copied()
        .filter(|&pid| process_alive(pid))
        .collect();
    if alive.is_empty() {
        return;
    }

    for &pid in &alive {
        send_signal(pid, libc::SIGTERM);
    }
    wait_for_exit(&mut alive, grace);

    for &pid in &alive {
        send_signal(pid, libc::SIGKILL);
    }
    wait_for_exit(&mut alive, KILL_GRACE);
}

fn wait_for_exit(alive: &mut Vec<i32>, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        alive.retain(|&pid| process_alive(pid));
        if alive.is_empty() || Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Returns true if a process with this PID currently exists.
fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn process_session_id(pid: i32) -> Option<i32> {
    let sid = unsafe { libc::getsid(pid) };
    (sid >= 0).then_some(sid)
}

#[cfg(not(test))]
fn current_user_pids() -> Vec<i32> {
    Command::new("ps")
        .args(["-x", "-o", "pid="])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(all(not(test), target_os = "linux"))]
fn environment_marker_pids(session_name: &str, self_pid: i32) -> Vec<i32> {
    let marker = format!("{BORK_SESSION_VAR}={session_name}");
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            if pid <= 1 || pid == self_pid {
                return None;
            }
            process_env_has_marker(&entry.path(), &marker).then_some(pid)
        })
        .collect()
}

#[cfg(all(not(test), target_os = "linux"))]
fn process_env_has_marker(proc_dir: &Path, marker: &str) -> bool {
    fs::read(proc_dir.join("environ"))
        .ok()
        .is_some_and(|environment| env_has_marker(&environment, marker))
}

#[cfg(any(test, target_os = "linux"))]
fn env_has_marker(environment: &[u8], marker: &str) -> bool {
    environment
        .split(|byte| *byte == 0)
        .any(|variable| variable == marker.as_bytes())
}

fn send_signal(pid: i32, signal: i32) {
    unsafe { libc::kill(pid, signal) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_session_ids_deduplicate_and_skip_missing_processes() {
        let own_pid = std::process::id() as i32;
        let sessions = process_session_ids(&[own_pid, own_pid, i32::MAX]);
        assert_eq!(sessions, vec![process_session_id(own_pid).unwrap()]);
    }

    #[test]
    fn finds_exact_env_marker() {
        let environment = b"HOME=/x\0BORK_SESSION=bork-bork-1\0PATH=/y\0";
        assert!(env_has_marker(environment, "BORK_SESSION=bork-bork-1"));
        assert!(!env_has_marker(environment, "BORK_SESSION=bork-bork-14"));
    }

    #[test]
    fn terminate_pids_ignores_already_dead_pids() {
        terminate_pids(&[i32::MAX], Duration::from_secs(2));
    }
}
