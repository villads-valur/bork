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

use super::archive::{STAGING_PREFIX, TMP_INFIX};
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

/// What a worker check does: nothing when backups can't run (`blocked`),
/// otherwise `run` when forced by "back up now" or when `due` says so.
pub fn decide(
    blocked: Option<Check>,
    manual: bool,
    due: impl FnOnce() -> bool,
    run: impl FnOnce() -> Result<Option<bool>, String>,
) -> Check {
    if let Some(check) = blocked {
        return check;
    }
    if manual || due() {
        Check::Ran(run())
    } else {
        Check::Skipped
    }
}

/// Whether a scheduled check should run: due by [`super::is_due`], or the
/// last run failed (e.g. a CLI `bork backup` hours after a good one), so a
/// failure is retried hourly instead of waiting out the 24h.
pub fn retry_or_due(record: Option<&ArchiveStatus>, due: bool) -> bool {
    due || record.is_some_and(|r| !r.ok)
}

/// Error shown when the worker thread is gone and `B` can't reach it.
pub const WORKER_STOPPED: &str = "Automatic backups stopped (worker crashed); restart bork";

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
        BackupIndicator::Failed { error: old, .. } if same_error(old, new) => None,
        _ => Some(new),
    }
}

/// Error messages compared with the generated parts of temp and staging
/// names masked ([`TMP_INFIX`] + pid, [`STAGING_PREFIX`] + pid-nanos, from
/// `archive.rs`), which would otherwise make the same failure look new
/// after a restart or every run. Other numbers, like `project-2` in a path,
/// still count.
fn same_error(a: &str, b: &str) -> bool {
    fn mask(s: &str) -> String {
        let staging_nanos = format!("{STAGING_PREFIX}#-");
        let chars: Vec<char> = s.chars().collect();
        let digits_from = |start: usize| {
            chars[start..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count()
        };
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < chars.len() {
            let run = digits_from(i);
            // `<TMP_INFIX><pid>`, and the staging dir's
            // `<STAGING_PREFIX><pid>-<nanos>` (both runs), but not a backup file named after a host that
            // starts with a digit.
            let staging_pid = out.ends_with(STAGING_PREFIX)
                && chars.get(i + run) == Some(&'-')
                && digits_from(i + run + 1) > 0;
            let generated =
                out.ends_with(TMP_INFIX) || out.ends_with(&staging_nanos) || staging_pid;
            if run > 0 && generated {
                out.push('#');
                i += run;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }
    mask(a) == mask(b)
}

/// Short warning text for the header, `None` when nothing should show.
pub fn warning_text(indicator: &BackupIndicator, now: u64) -> Option<String> {
    let stale_age = |at: &Option<u64>| {
        at.map(|at| now.saturating_sub(at))
            .filter(|age| *age > STALE_AFTER_SECS)
            .map(format_days)
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
        Check::Ran(Ok(Some(false))) => ("Backup up to date (nothing changed)".to_string(), false),
        // A manual check always runs (see `decide`); kept for exhaustiveness.
        Check::Skipped => ("Backup not due".to_string(), false),
        Check::Ran(Err(err)) => (format!("Backup failed: {err}"), true),
    }
}

/// Age in whole days (`2d`); only used past [`STALE_AFTER_SECS`].
fn format_days(secs: u64) -> String {
    format!("{}d", secs / (24 * 60 * 60))
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
    fn formats_days() {
        assert_eq!(format_days(50 * HOUR), "2d");
        assert_eq!(format_days(72 * HOUR), "3d");
    }

    #[test]
    fn a_failed_last_run_is_retried_before_the_24h_mark() {
        assert!(retry_or_due(
            Some(&failed(Some(NOW - HOUR), "no dir")),
            false
        ));
        assert!(!retry_or_due(Some(&ok_at(NOW - HOUR)), false));
        assert!(!retry_or_due(None, false));
        assert!(retry_or_due(Some(&ok_at(NOW - 25 * HOUR)), true));
    }

    #[test]
    fn decide_checks_blocked_then_manual_then_due() {
        let ran = || Ok(Some(true));
        let never_due = || false;
        let never_run = || -> Result<Option<bool>, String> { panic!("must not run") };

        // Blocked wins, even over `B`.
        assert_eq!(
            decide(Some(Check::NoPath), true, || true, never_run),
            Check::NoPath
        );
        assert_eq!(
            decide(Some(Check::NoProjects), true, || true, never_run),
            Check::NoProjects
        );
        // `B` runs even when not due.
        assert_eq!(
            decide(None, true, never_due, ran),
            Check::Ran(Ok(Some(true)))
        );
        // Scheduled: run only when due.
        assert_eq!(
            decide(None, false, || true, ran),
            Check::Ran(Ok(Some(true)))
        );
        assert_eq!(decide(None, false, never_due, never_run), Check::Skipped);
        assert_eq!(
            decide(None, false, || true, || Err("tar failed".into())),
            Check::Ran(Err("tar failed".into()))
        );
    }

    #[test]
    fn errors_differing_only_in_pids_are_the_same_failure() {
        let a = failure("tar: /b/.backup.tar.gz.tmp.4123: Permission denied", None);
        let b = failure("tar: /b/.backup.tar.gz.tmp.98: Permission denied", None);
        assert_eq!(failure_to_report(&a, &b), None);
        let other = failure("tar: /b/.backup.tar.gz.tmp.98: No space left", None);
        assert!(failure_to_report(&a, &other).is_some());
        let staging_a = failure("staging /tmp/bork-backup-41-1700000001/x: denied", None);
        let staging_b = failure("staging /tmp/bork-backup-7-1700009999/x: denied", None);
        assert_eq!(failure_to_report(&staging_a, &staging_b), None);
        // A backup file named after a host keeps its digits.
        let host_a = failure("writing /b/bork-backup-2024mac.tar.gz: denied", None);
        let host_b = failure("writing /b/bork-backup-2025mac.tar.gz: denied", None);
        assert!(failure_to_report(&host_a, &host_b).is_some());
    }

    #[test]
    fn numbers_in_project_paths_still_make_a_new_failure() {
        let one = failure("reading /work/project-1/.bork/config.toml: denied", None);
        let two = failure("reading /work/project-2/.bork/config.toml: denied", None);
        assert_eq!(
            failure_to_report(&one, &two),
            Some("reading /work/project-2/.bork/config.toml: denied")
        );
    }
}
