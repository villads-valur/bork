use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::error::AppError;

use super::shell_escape_single_quotes;

pub(super) struct SetupRun {
    dir: PathBuf,
}

impl SetupRun {
    pub(super) fn prepare(status_dir: &Path, prefix: &str) -> Result<Self, AppError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        fs::create_dir_all(status_dir)?;
        let dir = loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let dir = status_dir.join(format!("setup-{}-{id}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let run = Self { dir };
        fs::write(run.dir.join("script"), format!("{prefix}\n"))?;
        Ok(run)
    }

    pub(super) fn command(&self) -> String {
        let script = shell_escape_single_quotes(&self.dir.join("script").to_string_lossy());
        let result = shell_escape_single_quotes(&self.dir.join("result").to_string_lossy());
        // Source a staged script so even a syntax error reaches the EXIT trap.
        // Publish atomically: the reader must never see an empty exit code.
        let trap = format!("printf '%s\\n' \"$?\" > '{result}.tmp'; mv '{result}.tmp' '{result}'");
        format!(
            "(trap '{}' EXIT; trap 'exit 130' INT; trap 'exit 143' TERM; trap 'exit 129' HUP; . '{script}')",
            shell_escape_single_quotes(&trap)
        )
    }

    pub(super) fn wait(&self, pane_alive: impl Fn() -> bool) -> Result<(), AppError> {
        loop {
            match fs::read_to_string(self.dir.join("result")) {
                Ok(result) => {
                    let code: i32 = result
                        .trim()
                        .parse()
                        .map_err(|_| AppError::Setup("invalid setup exit status".to_string()))?;
                    return if code == 0 {
                        Ok(())
                    } else {
                        Err(AppError::Setup(format!("script exited with status {code}")))
                    };
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            if !pane_alive() {
                return Err(AppError::Setup(
                    "agent pane ended before setup completed".to_string(),
                ));
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for SetupRun {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::Instant;

    use super::*;

    fn run_setup(prefix: &str) -> (SetupRun, bool) {
        run_setup_with_shell(prefix, "sh")
    }

    fn run_setup_with_shell(prefix: &str, shell: &str) -> (SetupRun, bool) {
        let run = SetupRun::prepare(&std::env::temp_dir(), prefix).expect("stage setup");
        let agent_started = run.dir.join("agent-started");
        let status = Command::new(shell)
            .arg("-c")
            .arg(format!(
                "{} && touch '{}'",
                run.command(),
                shell_escape_single_quotes(&agent_started.to_string_lossy())
            ))
            .status()
            .expect("run setup shell");
        let started = agent_started.exists();
        assert_eq!(status.success(), started);
        assert!(
            run.dir.join("result").exists(),
            "setup must acknowledge its exit"
        );
        (run, started)
    }

    #[test]
    fn success_acknowledged_before_agent_start() {
        let (run, started) = run_setup("(printf 'setup output\\n')");
        assert!(started);
        assert!(run.wait(|| true).is_ok());
    }

    #[test]
    fn failure_prevents_launch_and_is_not_acknowledged_as_success() {
        let (run, started) = run_setup("(exit 7)");
        assert!(!started);
        assert!(run
            .wait(|| true)
            .unwrap_err()
            .to_string()
            .contains("status 7"));
    }

    #[test]
    fn syntax_error_reports_failure_instead_of_waiting_forever() {
        let (run, started) = run_setup("(if");
        assert!(!started);
        assert!(run.wait(|| false).is_err());
    }

    #[test]
    fn acknowledgements_work_in_zsh() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        for (prefix, success) in [("(true)", true), ("(exit 7)", false), ("(if", false)] {
            let (run, started) = run_setup_with_shell(prefix, "/bin/zsh");
            assert_eq!(started, success);
            assert_eq!(run.wait(|| false).is_ok(), success);
        }
    }

    #[test]
    fn receipt_paths_with_spaces_and_quotes_are_shell_escaped() {
        let parent = SetupRun::prepare(&std::env::temp_dir(), "(true)").unwrap();
        let run = SetupRun::prepare(&parent.dir.join("status dir's"), "(true)").unwrap();
        let status = Command::new("sh")
            .args(["-c", &run.command()])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(run.wait(|| false).is_ok());
    }

    #[test]
    fn stopped_pane_does_not_claim_success() {
        let run = SetupRun::prepare(&std::env::temp_dir(), "(true)").unwrap();
        assert!(run.wait(|| false).is_err());
    }

    #[test]
    fn interrupted_setup_reports_failure() {
        for shell in ["/bin/sh", "/bin/zsh"] {
            if !Path::new(shell).exists() {
                continue;
            }
            let run = SetupRun::prepare(&std::env::temp_dir(), "").unwrap();
            let ready = run.dir.join("ready");
            fs::write(
                run.dir.join("script"),
                format!(
                    "(touch '{}'; sleep 5)\n",
                    shell_escape_single_quotes(&ready.to_string_lossy())
                ),
            )
            .unwrap();
            let mut child = Command::new(shell)
                .args(["-c", &run.command()])
                .process_group(0)
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ready.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let signal = unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGINT) };
            let status = child.wait().unwrap();
            assert!(ready.exists());
            assert_eq!(signal, 0);
            assert!(!status.success());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !run.dir.join("result").exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                run.dir.join("result").exists(),
                "{shell} must report cancellation"
            );
            assert!(run.wait(|| false).is_err());
        }
    }

    #[test]
    fn retry_cannot_reuse_an_earlier_success_receipt() {
        let (first, _) = run_setup("(true)");
        let (retry, started) = run_setup("(false)");
        assert_ne!(first.dir, retry.dir);
        assert!(first.wait(|| false).is_ok());
        assert!(!started);
        assert!(retry.wait(|| false).is_err());
        let dir = retry.dir.clone();
        drop(retry);
        assert!(!dir.exists());
    }
}
