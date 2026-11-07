//! Status decisions for the TUI backup worker. Pure functions over the
//! status record in `~/.config/bork/backup.json`, so the worker thread in
//! `main.rs` only does the I/O.
//!
//! Backups run once a day: the worker checks shortly after startup and then
//! hourly whether one is due ([`super::is_due`]), and `B` in the TUI forces
//! a run. The TUI warns when the last run failed, or when the last success
//! is older than [`STALE_AFTER_SECS`] (TUI closed for days, a run stuck on
//! the backup lock); the age is checked at render time.

use std::time::Duration;

use super::status::ArchiveStatus;

/// How long after startup the first check runs.
pub const STARTUP_DELAY: Duration = Duration::from_secs(15);
/// How often the worker checks whether a backup is due.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// The TUI warns once the last success is this old.
pub const STALE_AFTER_SECS: u64 = 48 * 60 * 60;

/// What one worker check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// No `[backup] path` configured; nothing ran.
    NoPath,
    /// No registered project has backups enabled; nothing ran.
    NoProjects,
    /// Not due yet; nothing ran.
    Skipped,
    /// `backup::run` ran, or reading a project's config before it failed.
    /// `Ok(None)` when no project has backups enabled, `Ok(Some(written))`
    /// on success (`written` is false when unchanged).
    Ran(Result<Option<bool>, String>),
}

/// One worker message to the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub indicator: BackupIndicator,
    /// The check forced by "back up now"; `None` for scheduled checks and
    /// the startup snapshot.
    pub manual: Option<Check>,
    /// The startup snapshot of the recorded status: it sets the baseline
    /// and never re-reports a failure the user has already seen.
    pub baseline: bool,
}

/// Whether any registered project has backups enabled. An unreadable
/// config is an error, like it is for `backup::run`.
pub fn any_enabled(
    projects: impl IntoIterator<Item = anyhow::Result<bool>>,
) -> Result<bool, String> {
    let mut any = false;
    for enabled in projects {
        any |= enabled.map_err(|err| format!("{err:#}"))?;
    }
    Ok(any)
}

/// The check result when a backup can't run at all (no path, no opted-in
/// project, unreadable config), checked before the due test so an old
/// failure doesn't outlive backups being turned off. `None` when it can run.
pub fn blocked(path_configured: bool, enabled: Result<bool, String>) -> Option<Check> {
    if !path_configured {
        return Some(Check::NoPath);
    }
    match enabled {
        Ok(true) => None,
        Ok(false) => Some(Check::NoProjects),
        Err(err) => Some(Check::Ran(Err(err))),
    }
}

/// The report sent at startup, before the first check: the recorded status,
/// unless backups can't run. Only a failure already in `backup.json` is a
/// silent baseline; a config or registry error found now is reported like
/// any check's.
pub fn startup_report(blocked: Option<Check>, record: Option<&ArchiveStatus>) -> Report {
    let baseline = !matches!(blocked, Some(Check::Ran(Err(_))));
    let indicator = match &blocked {
        Some(check) => indicator(check, record),
        None => from_record(record),
    };
    Report {
        indicator,
        manual: None,
        baseline,
    }
}

/// Why backups are off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffReason {
    NoPath,
    NoProjects,
}

/// Backup state as the TUI shows it. Only `Failed` and an old `Ok` render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BackupIndicator {
    /// Nothing known yet (before the first report).
    #[default]
    Unknown,
    Off(OffReason),
    /// The last run succeeded (or none has run yet).
    Ok {
        last_success_at: Option<u64>,
    },
    /// The last run failed.
    Failed {
        error: String,
        last_success_at: Option<u64>,
    },
}

/// Indicator after `check`, given the status record read after it ran.
pub fn indicator(check: &Check, record: Option<&ArchiveStatus>) -> BackupIndicator {
    match check {
        Check::NoPath => BackupIndicator::Off(OffReason::NoPath),
        Check::NoProjects | Check::Ran(Ok(None)) => BackupIndicator::Off(OffReason::NoProjects),
        Check::Ran(Err(err)) => BackupIndicator::Failed {
            error: err.clone(),
            last_success_at: record.and_then(|r| r.last_success_at),
        },
        Check::Ran(Ok(Some(_))) | Check::Skipped => from_record(record),
    }
}

/// Indicator from the status record alone (also the startup snapshot).
pub fn from_record(record: Option<&ArchiveStatus>) -> BackupIndicator {
    match record {
        Some(record) if !record.ok => BackupIndicator::Failed {
            error: record
                .error
                .clone()
                .unwrap_or_else(|| "unknown error".to_string()),
            last_success_at: record.last_success_at,
        },
        _ => BackupIndicator::Ok {
            last_success_at: record.and_then(|r| r.last_success_at),
        },
    }
}

/// The failure to surface (status bar + errors.log) when moving from `prev`
/// to `next`: only a new failure or a changed error message, so a backup
/// that keeps failing doesn't re-report every hour.
pub fn failure_to_report<'a>(prev: &BackupIndicator, next: &'a BackupIndicator) -> Option<&'a str> {
    let BackupIndicator::Failed { error: new, .. } = next else {
        return None;
    };
    match prev {
        BackupIndicator::Failed { error: old, .. } if old == new => None,
        _ => Some(new),
    }
}

/// Short warning text for the header, `None` when nothing should show.
pub fn warning_text(indicator: &BackupIndicator, now: u64) -> Option<String> {
    let stale_age = |at: &Option<u64>| {
        at.map(|at| now.saturating_sub(at))
            .filter(|age| *age > STALE_AFTER_SECS)
            .map(format_age)
    };
    match indicator {
        BackupIndicator::Failed {
            last_success_at, ..
        } => Some(match (last_success_at, stale_age(last_success_at)) {
            (None, _) => "backup failed, none yet".to_string(),
            (Some(_), Some(age)) => format!("backup failed, none in {age}"),
            (Some(_), None) => "backup failed".to_string(),
        }),
        BackupIndicator::Ok { last_success_at } => {
            stale_age(last_success_at).map(|age| format!("no backup in {age}"))
        }
        _ => None,
    }
}

/// Status bar message after a manual "back up now", and whether it's an error.
pub fn manual_message(check: &Check) -> (String, bool) {
    match check {
        Check::NoPath => (
            "Backups are off: set `path` under [backup] in ~/.config/bork/config.toml".to_string(),
            true,
        ),
        Check::NoProjects | Check::Ran(Ok(None)) => (
            "Backups are off: set `enabled = true` under [backup] in a project's .bork/config.toml"
                .to_string(),
            true,
        ),
        Check::Ran(Ok(Some(true))) => ("Backup written".to_string(), false),
        Check::Ran(Ok(Some(false))) | Check::Skipped => {
            ("Backup up to date (nothing changed)".to_string(), false)
        }
        Check::Ran(Err(err)) => (format!("Backup failed: {err}"), true),
    }
}

/// Compact age: `5m`, `3h`, `2d`.
pub fn format_age(secs: u64) -> String {
    if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h", secs / (60 * 60))
    } else {
        format!("{}d", secs / (24 * 60 * 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60;
    const NOW: u64 = 1_000_000;

    fn ok_at(at: u64) -> ArchiveStatus {
        ArchiveStatus {
            last_run_at: at,
            last_success_at: Some(at),
            ok: true,
            ..ArchiveStatus::default()
        }
    }

    fn failed(last_success_at: Option<u64>, error: &str) -> ArchiveStatus {
        ArchiveStatus {
            last_run_at: NOW,
            last_success_at,
            ok: false,
            error: Some(error.to_string()),
            ..ArchiveStatus::default()
        }
    }

    fn healthy(last_success_at: Option<u64>) -> BackupIndicator {
        BackupIndicator::Ok { last_success_at }
    }

    fn failure(error: &str, last_success_at: Option<u64>) -> BackupIndicator {
        BackupIndicator::Failed {
            error: error.to_string(),
            last_success_at,
        }
    }

    #[test]
    fn off_without_path_or_projects() {
        assert_eq!(
            indicator(&Check::NoPath, Some(&ok_at(NOW))),
            BackupIndicator::Off(OffReason::NoPath)
        );
        assert_eq!(
            indicator(&Check::Ran(Ok(None)), None),
            BackupIndicator::Off(OffReason::NoProjects)
        );
    }

    #[test]
    fn blocked_before_the_due_check() {
        assert_eq!(blocked(false, Ok(true)), Some(Check::NoPath));
        assert_eq!(blocked(true, Ok(false)), Some(Check::NoProjects));
        assert_eq!(
            blocked(true, Err("reading config".into())),
            Some(Check::Ran(Err("reading config".into())))
        );
        assert_eq!(blocked(true, Ok(true)), None);
    }

    #[test]
    fn startup_reports_new_config_errors_but_not_recorded_failures() {
        let record = failed(Some(NOW - HOUR), "tar failed");

        let recorded = startup_report(None, Some(&record));
        assert!(recorded.baseline);
        assert_eq!(recorded.indicator, failure("tar failed", Some(NOW - HOUR)));

        let config = startup_report(Some(Check::Ran(Err("bad registry".into()))), Some(&record));
        assert!(!config.baseline);
        assert_eq!(config.indicator, failure("bad registry", Some(NOW - HOUR)));

        let off = startup_report(Some(Check::NoProjects), Some(&record));
        assert_eq!(off.indicator, BackupIndicator::Off(OffReason::NoProjects));
    }

    #[test]
    fn opting_out_clears_an_old_failure() {
        let record = failed(Some(NOW - HOUR), "tar failed");
        assert_eq!(
            indicator(&Check::NoProjects, Some(&record)),
            BackupIndicator::Off(OffReason::NoProjects)
        );
    }

    #[test]
    fn any_enabled_fails_on_unreadable_config() {
        assert_eq!(any_enabled([Ok(false), Ok(true)]), Ok(true));
        assert_eq!(any_enabled([Ok(false)]), Ok(false));
        assert_eq!(any_enabled(Vec::new()), Ok(false));
        assert_eq!(
            any_enabled([Ok(true), Err(anyhow::anyhow!("denied"))]),
            Err("denied".to_string())
        );
    }

    #[test]
    fn run_error_is_a_failure_with_the_last_success() {
        let record = failed(Some(NOW - HOUR), "tar failed");
        assert_eq!(
            indicator(&Check::Ran(Err("tar failed".into())), Some(&record)),
            failure("tar failed", Some(NOW - HOUR))
        );
        assert_eq!(
            indicator(&Check::Ran(Err("tar failed".into())), None),
            failure("tar failed", None)
        );
    }

    #[test]
    fn success_is_healthy() {
        assert_eq!(
            indicator(&Check::Ran(Ok(Some(true))), Some(&ok_at(NOW))),
            healthy(Some(NOW))
        );
    }

    #[test]
    fn skipped_reads_the_record() {
        // A failed `bork backup` from the CLI shows up on the next check.
        assert_eq!(
            indicator(&Check::Skipped, Some(&failed(Some(NOW - HOUR), "no dir"))),
            failure("no dir", Some(NOW - HOUR))
        );
        assert_eq!(
            indicator(&Check::Skipped, Some(&ok_at(NOW - HOUR))),
            healthy(Some(NOW - HOUR))
        );
        assert_eq!(indicator(&Check::Skipped, None), healthy(None));
    }

    #[test]
    fn warns_when_the_last_success_is_over_two_days_old() {
        // E.g. the TUI was closed for days, or the due run waits on the lock.
        let old = from_record(Some(&ok_at(NOW - 72 * HOUR)));
        assert_eq!(old, healthy(Some(NOW - 72 * HOUR)));
        assert_eq!(warning_text(&old, NOW).as_deref(), Some("no backup in 3d"));
        assert_eq!(warning_text(&healthy(Some(NOW - 47 * HOUR)), NOW), None);
        // Never backed up: the first run is about to happen, nothing to say.
        assert_eq!(warning_text(&healthy(None), NOW), None);
    }

    #[test]
    fn reports_only_new_failures() {
        let healthy = healthy(Some(1));
        let a = failure("a", Some(1));
        let b = failure("b", Some(1));
        assert_eq!(failure_to_report(&healthy, &a), Some("a"));
        assert_eq!(failure_to_report(&BackupIndicator::Unknown, &a), Some("a"));
        assert_eq!(failure_to_report(&a, &a), None);
        assert_eq!(failure_to_report(&a, &failure("a", None)), None);
        assert_eq!(failure_to_report(&a, &b), Some("b"));
        assert_eq!(failure_to_report(&a, &healthy), None);
    }

    #[test]
    fn warning_text_for_failures() {
        assert_eq!(warning_text(&healthy(Some(NOW)), NOW), None);
        assert_eq!(
            warning_text(&BackupIndicator::Off(OffReason::NoPath), NOW),
            None
        );
        assert_eq!(warning_text(&BackupIndicator::Unknown, NOW), None);
        assert_eq!(
            warning_text(&failure("x", Some(NOW - 47 * HOUR)), NOW).as_deref(),
            Some("backup failed")
        );
        assert_eq!(
            warning_text(&failure("x", Some(NOW - 72 * HOUR)), NOW).as_deref(),
            Some("backup failed, none in 3d")
        );
        assert_eq!(
            warning_text(&failure("x", None), NOW).as_deref(),
            Some("backup failed, none yet")
        );
    }

    #[test]
    fn manual_messages() {
        assert!(manual_message(&Check::NoPath).1);
        assert!(manual_message(&Check::NoProjects).1);
        assert!(manual_message(&Check::Ran(Ok(None))).1);
        assert_eq!(
            manual_message(&Check::Ran(Ok(Some(true)))),
            ("Backup written".to_string(), false)
        );
        assert!(!manual_message(&Check::Ran(Ok(Some(false)))).1);
        assert_eq!(
            manual_message(&Check::Ran(Err("boom".into()))),
            ("Backup failed: boom".to_string(), true)
        );
    }

    #[test]
    fn formats_ages() {
        assert_eq!(format_age(59), "0m");
        assert_eq!(format_age(5 * 60), "5m");
        assert_eq!(format_age(3 * HOUR), "3h");
        assert_eq!(format_age(50 * HOUR), "2d");
    }
}
