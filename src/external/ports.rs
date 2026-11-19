use std::collections::{HashMap, HashSet};
use std::process::Command;

use crate::types::AgentKind;

/// Result of polling listening ports across all tmux sessions.
pub struct PortPollResult {
    /// Map from tmux session name to the TCP ports that session's processes are listening on.
    pub ports: HashMap<String, Vec<u16>>,
}

/// A row from `ps`: parent PID and executable name.
struct ProcessInfo {
    ppid: u32,
    name: String,
}

/// Detect listening TCP ports for each alive tmux session.
///
/// Strategy:
/// 1. `tmux list-panes -a` to get all pane PIDs grouped by session
/// 2. `lsof -iTCP -sTCP:LISTEN -P -n -F pn` to get listening ports with PIDs
/// 3. `ps -eo pid,ppid,comm` to build a process table (parents and names)
/// 4. Drop listeners owned by an agent process (their own IDE/MCP bridges)
/// 5. For each remaining listener, walk the parent chain to match a tmux pane PID
pub fn poll_listening_ports(sessions: &HashSet<String>) -> HashMap<String, Vec<u16>> {
    if sessions.is_empty() {
        return HashMap::new();
    }

    let session_panes = list_all_pane_pids(sessions);
    if session_panes.is_empty() {
        return HashMap::new();
    }

    let listening = lsof_listening_ports();
    if listening.is_empty() {
        return HashMap::new();
    }

    let processes = build_process_table();
    if processes.is_empty() {
        return HashMap::new();
    }

    match_ports_to_sessions(&session_panes, &listening, &processes)
}

fn match_ports_to_sessions(
    session_panes: &HashMap<String, Vec<u32>>,
    listening: &[(u32, u16)],
    processes: &HashMap<u32, ProcessInfo>,
) -> HashMap<String, Vec<u16>> {
    let mut pid_to_session: HashMap<u32, String> = HashMap::new();
    for (session, pids) in session_panes {
        for &pid in pids {
            pid_to_session.insert(pid, session.clone());
        }
    }
    let mut result: HashMap<String, Vec<u16>> = HashMap::new();
    for &(pid, port) in listening {
        if processes
            .get(&pid)
            .is_some_and(|info| is_agent_process(&info.name))
        {
            continue;
        }
        if let Some(session) = find_ancestor_session(pid, &pid_to_session, processes) {
            result.entry(session).or_default().push(port);
        }
    }

    // Sort ports for consistent display
    for ports in result.values_mut() {
        ports.sort_unstable();
        ports.dedup();
    }

    result
}

/// Whether a `ps` command name belongs to a coding agent. Agents open their own
/// localhost listeners (Claude's IDE bridge, Codex's app server), which aren't
/// dev servers. Uses `ps` rather than lsof's command field because Claude
/// rewrites its process title to its version (e.g. `2.1.328`), which is what
/// lsof and tmux report. Dev servers an agent starts are child processes with
/// their own names, so they still count.
fn is_agent_process(command: &str) -> bool {
    let name = command.rsplit('/').next().unwrap_or(command);
    AgentKind::ALL.iter().any(|kind| kind.command() == name)
}

/// Walk up the process tree from `pid` looking for any tmux pane PID.
fn find_ancestor_session(
    mut pid: u32,
    pid_to_session: &HashMap<u32, String>,
    processes: &HashMap<u32, ProcessInfo>,
) -> Option<String> {
    let mut visited = HashSet::new();
    loop {
        if let Some(session) = pid_to_session.get(&pid) {
            return Some(session.clone());
        }
        if !visited.insert(pid) {
            return None; // cycle detection
        }
        match processes.get(&pid).map(|info| info.ppid) {
            Some(ppid) if ppid != 0 && ppid != pid => pid = ppid,
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
///   f<fd>          (file descriptor line, ignored)
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

/// Build a PID -> (parent PID, command) table from `ps -eo pid,ppid,comm`.
fn build_process_table() -> HashMap<u32, ProcessInfo> {
    let output = Command::new("ps").args(["-eo", "pid,ppid,comm"]).output();

    let Ok(output) = output else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    parse_ps_output(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `ps -eo pid,ppid,comm` output (with header). `comm` is the last
/// column and may contain spaces (macOS app bundle paths).
fn parse_ps_output(output: &str) -> HashMap<u32, ProcessInfo> {
    let mut map = HashMap::new();
    for line in output.lines().skip(1) {
        let mut parts = line.split_whitespace();
        let (Some(pid_str), Some(ppid_str)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid_str.parse::<u32>(), ppid_str.parse::<u32>()) else {
            continue;
        };
        let name = parts.collect::<Vec<_>>().join(" ");
        map.insert(pid, ProcessInfo { ppid, name });
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
    fn parse_lsof_skips_fd_lines() {
        let output = "p100\nf39\nn127.0.0.1:59346\nf40\nn[::1]:59346\n";
        assert_eq!(
            parse_lsof_field_output(output),
            vec![(100, 59346), (100, 59346)]
        );
    }

    fn session(pane_pid: u32) -> HashMap<String, Vec<u32>> {
        [("bork-bork-1".to_string(), vec![pane_pid])].into()
    }

    #[test]
    fn agent_listeners_are_ignored_but_their_dev_servers_count() {
        // Pane shell 10 runs claude (100) and codex (300). Claude's own IDE
        // bridge and Codex's app server don't count; the node dev server
        // claude started and a python server in the shell do.
        let processes = parse_ps_output(
            "  PID  PPID COMM\n\
             10 1 -zsh\n\
             100 10 claude\n\
             200 100 node\n\
             300 10 /Users/me/.codex/bin/codex\n\
             400 10 python3\n",
        );
        let listening = [(100, 56412), (200, 3000), (300, 50536), (400, 8000)];
        let result = match_ports_to_sessions(&session(10), &listening, &processes);
        assert_eq!(result["bork-bork-1"], vec![3000, 8000]);
    }

    #[test]
    fn session_with_only_agent_listeners_has_no_ports() {
        let processes = parse_ps_output("PID PPID COMM\n10 1 -zsh\n100 10 claude\n");
        let result = match_ports_to_sessions(&session(10), &[(100, 56412)], &processes);
        assert!(result.is_empty());
    }

    #[test]
    fn agent_names_match_every_supported_agent() {
        for name in ["claude", "codex", "opencode", "pi", "cursor-agent"] {
            assert!(is_agent_process(name), "{name}");
        }
        assert!(is_agent_process("/opt/homebrew/bin/claude"));
        assert!(!is_agent_process("node"));
        assert!(!is_agent_process("claude-dev-server"));
    }

    #[test]
    fn parse_ps_keeps_command_with_spaces() {
        let processes = parse_ps_output(
            "PID PPID COMM\n4558 1 /Applications/Okta Verify.app/Contents/MacOS/Okta Verify\n",
        );
        assert_eq!(processes[&4558].ppid, 1);
        assert_eq!(
            processes[&4558].name,
            "/Applications/Okta Verify.app/Contents/MacOS/Okta Verify"
        );
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

    fn pane_100() -> HashMap<u32, String> {
        [(100, "my-session".to_string())].into()
    }

    fn tree(parents: &[(u32, u32)]) -> HashMap<u32, ProcessInfo> {
        parents
            .iter()
            .map(|&(pid, ppid)| {
                let info = ProcessInfo {
                    ppid,
                    name: "sh".to_string(),
                };
                (pid, info)
            })
            .collect()
    }

    #[test]
    fn find_ancestor_direct_match() {
        let result = find_ancestor_session(100, &pane_100(), &tree(&[(100, 1)]));
        assert_eq!(result, Some("my-session".to_string()));
    }

    #[test]
    fn find_ancestor_two_levels_up() {
        let processes = tree(&[(300, 200), (200, 100), (100, 1)]);
        let result = find_ancestor_session(300, &pane_100(), &processes);
        assert_eq!(result, Some("my-session".to_string()));
    }

    #[test]
    fn find_ancestor_no_match() {
        let processes = tree(&[(300, 200), (200, 1)]);
        assert_eq!(find_ancestor_session(300, &pane_100(), &processes), None);
    }

    #[test]
    fn find_ancestor_cycle_protection() {
        // Cycle: 200 -> 300 -> 200
        let processes = tree(&[(200, 300), (300, 200)]);
        assert_eq!(find_ancestor_session(200, &pane_100(), &processes), None);
    }
}
