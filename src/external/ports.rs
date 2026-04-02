use std::collections::{HashMap, HashSet};
use std::process::Command;

/// Result of polling listening ports across all tmux sessions.
pub struct PortPollResult {
    /// Map from tmux session name to the TCP ports that session's processes are listening on.
    pub ports: HashMap<String, Vec<u16>>,
}

/// Detect listening TCP ports for each alive tmux session.
///
/// Strategy:
/// 1. `tmux list-panes -a` to get all pane PIDs grouped by session
/// 2. `lsof -iTCP -sTCP:LISTEN -P -n -F pn` to get all listening ports with PIDs
/// 3. `ps -eo pid,ppid` to build a process parent map
/// 4. For each listening port PID, walk the parent chain to match a tmux pane PID
pub fn poll_listening_ports(sessions: &HashSet<String>) -> HashMap<String, Vec<u16>> {
    if sessions.is_empty() {
        return HashMap::new();
    }

    let session_panes = list_all_pane_pids(sessions);
    if session_panes.is_empty() {
        return HashMap::new();
    }

    // Build reverse lookup: pane_pid -> session_name
    let mut pid_to_session: HashMap<u32, String> = HashMap::new();
    for (session, pids) in &session_panes {
        for &pid in pids {
            pid_to_session.insert(pid, session.clone());
        }
    }

    let listening = lsof_listening_ports();
    if listening.is_empty() {
        return HashMap::new();
    }

    let parent_map = build_parent_map();
    if parent_map.is_empty() {
        return HashMap::new();
    }

    let all_pane_pids: HashSet<u32> = pid_to_session.keys().copied().collect();

    let mut result: HashMap<String, Vec<u16>> = HashMap::new();
    for (pid, port) in &listening {
        if let Some(session) =
            find_ancestor_session(*pid, &all_pane_pids, &pid_to_session, &parent_map)
        {
            result.entry(session).or_default().push(*port);
        }
    }

    // Sort ports for consistent display
    for ports in result.values_mut() {
        ports.sort_unstable();
        ports.dedup();
    }

    result
}

/// Walk up the process tree from `pid` looking for any tmux pane PID.
fn find_ancestor_session(
    mut pid: u32,
    pane_pids: &HashSet<u32>,
    pid_to_session: &HashMap<u32, String>,
    parent_map: &HashMap<u32, u32>,
) -> Option<String> {
    let mut visited = HashSet::new();
    loop {
        if pane_pids.contains(&pid) {
            return pid_to_session.get(&pid).cloned();
        }
        if !visited.insert(pid) {
            return None; // cycle detection
        }
        match parent_map.get(&pid) {
            Some(&ppid) if ppid != 0 && ppid != pid => pid = ppid,
            _ => return None,
        }
    }
}

/// Get all pane PIDs across all tmux sessions (one tmux call).
/// Returns map: session_name -> [pane_pids]
fn list_all_pane_pids(sessions: &HashSet<String>) -> HashMap<String, Vec<u32>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{session_name} #{pane_pid}"])
        .output();

    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    let mut result: HashMap<String, Vec<u32>> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(2, ' ');
        let Some(session) = parts.next() else {
            continue;
        };
        let Some(pid_str) = parts.next() else {
            continue;
        };

        // Only include sessions we care about
        if !sessions.contains(session) {
            continue;
        }

        if let Ok(pid) = pid_str.parse::<u32>() {
            result.entry(session.to_string()).or_default().push(pid);
        }
    }

    result
}

/// Get all TCP ports in LISTEN state with their PIDs via lsof.
/// Uses -F (field output) for reliable parsing.
fn lsof_listening_ports() -> Vec<(u32, u16)> {
    let output = Command::new("lsof")
        .args(["-iTCP", "-sTCP:LISTEN", "-P", "-n", "-F", "pn"])
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    parse_lsof_field_output(&String::from_utf8_lossy(&output.stdout))
}

/// Parse lsof -F pn output.
/// Format:
///   p<pid>         (process ID line)
///   n<name>        (network name line, e.g. "*:3000" or "127.0.0.1:8080")
///
/// We pair each 'n' line with the most recent 'p' line.
fn parse_lsof_field_output(output: &str) -> Vec<(u32, u16)> {
    let mut results = Vec::new();
    let mut current_pid: Option<u32> = None;

    for line in output.lines() {
        if let Some(pid_str) = line.strip_prefix('p') {
            current_pid = pid_str.parse().ok();
        } else if let Some(name) = line.strip_prefix('n') {
            let Some(pid) = current_pid else {
                continue;
            };
            if let Some(port) = extract_port_from_name(name) {
                results.push((pid, port));
            }
        }
    }

    results
}

/// Extract port number from lsof name field.
/// Examples: "*:3000", "127.0.0.1:8080", "[::1]:5173", "localhost:4000"
fn extract_port_from_name(name: &str) -> Option<u16> {
    let port_str = name.rsplit(':').next()?;
    port_str.parse().ok()
}

/// Build a PID -> parent PID map from `ps -eo pid,ppid`.
fn build_parent_map() -> HashMap<u32, u32> {
    let output = Command::new("ps").args(["-eo", "pid,ppid"]).output();

    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines().skip(1) {
        let mut parts = line.split_whitespace();
        let Some(pid_str) = parts.next() else {
            continue;
        };
        let Some(ppid_str) = parts.next() else {
            continue;
        };
        if let (Ok(pid), Ok(ppid)) = (pid_str.parse::<u32>(), ppid_str.parse::<u32>()) {
            map.insert(pid, ppid);
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsof_basic() {
        let output = "p14201\nn*:3000\np14205\nn127.0.0.1:5173\n";
        let result = parse_lsof_field_output(output);
        assert_eq!(result, vec![(14201, 3000), (14205, 5173)]);
    }

    #[test]
    fn parse_lsof_ipv6() {
        let output = "p1234\nn[::1]:8080\n";
        let result = parse_lsof_field_output(output);
        assert_eq!(result, vec![(1234, 8080)]);
    }

    #[test]
    fn parse_lsof_multiple_ports_same_pid() {
        let output = "p100\nn*:3000\nn*:3001\np200\nn*:8080\n";
        let result = parse_lsof_field_output(output);
        assert_eq!(result, vec![(100, 3000), (100, 3001), (200, 8080)]);
    }

    #[test]
    fn parse_lsof_empty() {
        assert!(parse_lsof_field_output("").is_empty());
    }

    #[test]
    fn parse_lsof_name_before_pid_is_skipped() {
        let output = "n*:3000\np100\nn*:8080\n";
        let result = parse_lsof_field_output(output);
        assert_eq!(result, vec![(100, 8080)]);
    }

    #[test]
    fn extract_port_wildcard() {
        assert_eq!(extract_port_from_name("*:3000"), Some(3000));
    }

    #[test]
    fn extract_port_ipv4() {
        assert_eq!(extract_port_from_name("127.0.0.1:8080"), Some(8080));
    }

    #[test]
    fn extract_port_ipv6() {
        assert_eq!(extract_port_from_name("[::1]:5173"), Some(5173));
    }

    #[test]
    fn extract_port_invalid() {
        assert_eq!(extract_port_from_name("no-port-here"), None);
    }

    #[test]
    fn find_ancestor_direct_match() {
        let pane_pids: HashSet<u32> = [100].into();
        let pid_to_session: HashMap<u32, String> = [(100, "my-session".to_string())].into();
        let parent_map: HashMap<u32, u32> = [(100, 1)].into();

        let result = find_ancestor_session(100, &pane_pids, &pid_to_session, &parent_map);
        assert_eq!(result, Some("my-session".to_string()));
    }

    #[test]
    fn find_ancestor_two_levels_up() {
        let pane_pids: HashSet<u32> = [100].into();
        let pid_to_session: HashMap<u32, String> = [(100, "my-session".to_string())].into();
        let parent_map: HashMap<u32, u32> = [(300, 200), (200, 100), (100, 1)].into();

        let result = find_ancestor_session(300, &pane_pids, &pid_to_session, &parent_map);
        assert_eq!(result, Some("my-session".to_string()));
    }

    #[test]
    fn find_ancestor_no_match() {
        let pane_pids: HashSet<u32> = [100].into();
        let pid_to_session: HashMap<u32, String> = [(100, "my-session".to_string())].into();
        let parent_map: HashMap<u32, u32> = [(300, 200), (200, 1)].into();

        let result = find_ancestor_session(300, &pane_pids, &pid_to_session, &parent_map);
        assert_eq!(result, None);
    }

    #[test]
    fn find_ancestor_cycle_protection() {
        let pane_pids: HashSet<u32> = [100].into();
        let pid_to_session: HashMap<u32, String> = [(100, "my-session".to_string())].into();
        // Cycle: 200 -> 300 -> 200
        let parent_map: HashMap<u32, u32> = [(200, 300), (300, 200)].into();

        let result = find_ancestor_session(200, &pane_pids, &pid_to_session, &parent_map);
        assert_eq!(result, None);
    }
}
