use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::external::process;
use crate::types::Issue;

/// How long the launch command gets to show up in the pane. Covers slow
/// shell rc files; setup time doesn't count once the command has started.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// What the launch command reports back right before it execs the agent.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Report {
    Started {
        session_id: Option<String>,
        setup_ran: bool,
        replaced_lost_session: bool,
    },
    Failed {
        error: String,
    },
}

/// Files shared between `launch_session` and the `bork issue launch` command
/// it types into the pane, in `.bork/agent-status/launch-<id>/`:
///   - `request.json`: the issue to launch, written by the caller
///   - `pid`: written by the command on start
///   - `result.json`: the command's `Report`, written before it execs
///
/// This is the caller's end, which owns the dir and removes it on drop.
pub(super) struct Handoff {
    dir: PathBuf,
}

/// The launch command's end of a `Handoff`. Leaves the dir alone so the
/// caller can still read a failure report after the command exits.
pub(super) struct Claim {
    dir: PathBuf,
}

impl Handoff {
    pub(super) fn dir(status_dir: &Path, issue_id: &str) -> PathBuf {
        status_dir.join(format!("launch-{}", issue_id.to_lowercase()))
    }

    /// Caller side: stage `issue` for the launch command. Clears anything a
    /// previous launch of the same issue left behind.
    pub(super) fn prepare(status_dir: &Path, issue: &Issue) -> Result<Self, AppError> {
        let dir = Self::dir(status_dir, &issue.id);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        let handoff = Self { dir };
        let request = serde_json::to_string(issue)
            .map_err(|e| AppError::Launch(format!("failed to stage launch: {e}")))?;
        write_atomic(&handoff.dir.join("request.json"), &request)?;
        Ok(handoff)
    }

    /// Caller side: wait for the launch command's report. Fails when the
    /// command never starts, exits without reporting (e.g. Ctrl-C during
    /// setup), or the pane goes away.
    pub(super) fn wait(&self, pane_alive: impl Fn() -> bool) -> Result<Report, AppError> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(report) = self.read_report()? {
                return Ok(report);
            }
            match self.read_pid() {
                Some(pid) if !process::process_alive(pid) => {
                    // It may have reported right before exiting.
                    return match self.read_report()? {
                        Some(report) => Ok(report),
                        None => Err(AppError::Launch(
                            "launch command exited before the agent started".to_string(),
                        )),
                    };
                }
                Some(_) => {}
                None if Instant::now() >= deadline => {
                    return Err(AppError::Launch(
                        "launch command did not start in the agent pane".to_string(),
                    ));
                }
                None => {}
            }
            if !pane_alive() {
                return Err(AppError::Launch(
                    "agent pane ended before the agent started".to_string(),
                ));
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    fn read_report(&self) -> Result<Option<Report>, AppError> {
        match fs::read_to_string(self.dir.join("result.json")) {
            Ok(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|_| AppError::Launch("invalid launch report".to_string())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn read_pid(&self) -> Option<i32> {
        fs::read_to_string(self.dir.join("pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Claim {
    /// Claim a staged launch. `None` when the command was run by hand, with
    /// nobody waiting on a report. The rename makes the claim one-shot, so a
    /// re-run in the pane can't take a request the caller is about to remove.
    pub(super) fn take(status_dir: &Path, issue_id: &str) -> Option<(Self, Issue)> {
        let dir = Handoff::dir(status_dir, issue_id);
        let claimed = dir.join("request.claimed");
        fs::rename(dir.join("request.json"), &claimed).ok()?;
        let request = fs::read_to_string(claimed).ok()?;
        let issue: Issue = serde_json::from_str(&request).ok()?;
        // Without the pid the caller times out, so a failed write fails the
        // launch either way.
        write_atomic(&dir.join("pid"), &std::process::id().to_string()).ok()?;
        Some((Self { dir }, issue))
    }

    /// Publish the report the caller is waiting on.
    pub(super) fn report(&self, report: &Report) -> Result<(), AppError> {
        let json = serde_json::to_string(report)
            .map_err(|e| AppError::Launch(format!("failed to report launch: {e}")))?;
        write_atomic(&self.dir.join("result.json"), &json)
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Write via a temp file + rename so a reader never sees a partial file.
fn write_atomic(path: &Path, contents: &str) -> Result<(), AppError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::types::{AgentKind, Column};

    use super::*;

    fn issue() -> Issue {
        Issue::new("BORK-7", "Launch me", Column::Todo, AgentKind::Codex)
    }

    fn started(session_id: Option<&str>) -> Report {
        Report::Started {
            session_id: session_id.map(str::to_string),
            setup_ran: true,
            replaced_lost_session: false,
        }
    }

    #[test]
    fn claimed_request_round_trips_the_issue() {
        let status = tempfile::TempDir::new().unwrap();
        let _caller = Handoff::prepare(status.path(), &issue()).unwrap();

        let (_command, claimed) = Claim::take(status.path(), "bork-7").unwrap();
        assert_eq!(claimed, issue());
    }

    #[test]
    fn a_staged_launch_can_only_be_claimed_once() {
        let status = tempfile::TempDir::new().unwrap();
        let _caller = Handoff::prepare(status.path(), &issue()).unwrap();

        let _first = Claim::take(status.path(), "bork-7").unwrap();
        assert!(Claim::take(status.path(), "bork-7").is_none());
    }

    #[test]
    fn claim_without_a_staged_launch_is_none() {
        let status = tempfile::TempDir::new().unwrap();
        assert!(Claim::take(status.path(), "bork-7").is_none());
    }

    #[test]
    fn wait_returns_the_report() {
        let status = tempfile::TempDir::new().unwrap();
        let caller = Handoff::prepare(status.path(), &issue()).unwrap();
        let (command, _) = Claim::take(status.path(), "bork-7").unwrap();

        command.report(&started(Some("ses_1"))).unwrap();
        assert_eq!(caller.wait(|| true).unwrap(), started(Some("ses_1")));
    }

    #[test]
    fn wait_returns_a_failure_report() {
        let status = tempfile::TempDir::new().unwrap();
        let caller = Handoff::prepare(status.path(), &issue()).unwrap();
        let (command, _) = Claim::take(status.path(), "bork-7").unwrap();

        let failed = Report::Failed {
            error: "setup error: script exited with status 7".into(),
        };
        command.report(&failed).unwrap();
        assert_eq!(caller.wait(|| true).unwrap(), failed);
    }

    #[test]
    fn wait_fails_when_the_command_exits_without_reporting() {
        let status = tempfile::TempDir::new().unwrap();
        let caller = Handoff::prepare(status.path(), &issue()).unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        fs::write(caller.dir.join("pid"), child.id().to_string()).unwrap();

        let err = caller.wait(|| true).unwrap_err().to_string();
        assert!(err.contains("exited before the agent started"), "{err}");
    }

    #[test]
    fn wait_fails_when_the_pane_ends() {
        let status = tempfile::TempDir::new().unwrap();
        let caller = Handoff::prepare(status.path(), &issue()).unwrap();

        assert!(caller.wait(|| false).is_err());
    }

    #[test]
    fn prepare_clears_an_earlier_report() {
        let status = tempfile::TempDir::new().unwrap();
        let _first = Handoff::prepare(status.path(), &issue()).unwrap();
        let (command, _) = Claim::take(status.path(), "bork-7").unwrap();
        command.report(&started(None)).unwrap();

        let retry = Handoff::prepare(status.path(), &issue()).unwrap();
        assert!(retry.read_report().unwrap().is_none());
    }
}
