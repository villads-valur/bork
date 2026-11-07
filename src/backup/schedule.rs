//! Status decisions for the TUI backup worker. Pure functions over the
//! status record in `~/.config/bork/backup.json`, so the worker thread in
//! `main.rs` only does the I/O.
//!
//! Backups run once a day: the worker checks shortly after startup and then
//! hourly whether one is due ([`super::is_due`]), and `B` in the TUI forces
//! a run.

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
    /// Not due yet; nothing ran.
    Skipped,
    /// `backup::run` ran. `Ok(None)` when no project has backups enabled,
    /// `Ok(Some(written))` on success (`written` is false when unchanged).
    Ran(Result<Option<bool>, String>),
}

/// Why backups are off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffReason {
    NoPath,
    NoProjects,
}

/// Backup state as the TUI shows it. Only `Failed` and `Stale` render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BackupIndicator {
    /// Nothing known yet (before the first report).
    #[default]
    Unknown,
    Off(OffReason),
    Healthy,
    /// The last run failed.
    Failed(String),
    /// No success in more than [`STALE_AFTER_SECS`]; holds the last success.
    Stale(u64),
}

/// Indicator after `check`, given the status record read after it ran.
pub fn indicator(check: &Check, record: Option<&ArchiveStatus>, now: u64) -> BackupIndicator {
    match check {
        Check::NoPath => BackupIndicator::Off(OffReason::NoPath),
        Check::Ran(Ok(None)) => BackupIndicator::Off(OffReason::NoProjects),
        Check::Ran(Err(err)) => BackupIndicator::Failed(err.clone()),
        Check::Ran(Ok(Some(_))) | Check::Skipped => from_record(record, now),
    }
}

/// Indicator from the status record alone (also used at startup, before
/// the first check).
pub fn from_record(record: Option<&ArchiveStatus>, now: u64) -> BackupIndicator {
    let Some(record) = record else {
        return BackupIndicator::Healthy;
    };
    if !record.ok {
        let error = record
            .error
            .clone()
            .unwrap_or_else(|| "unknown error".to_string());
        return BackupIndicator::Failed(error);
    }
    match record.last_success_at {
        Some(at) if now.saturating_sub(at) > STALE_AFTER_SECS => BackupIndicator::Stale(at),
        _ => BackupIndicator::Healthy,
    }
}

/// The failure to surface (status bar + errors.log) when moving from `prev`
/// to `next`: only a new failure or a changed error message, so a backup
/// that keeps failing doesn't re-report every hour.
pub fn failure_to_report<'a>(prev: &BackupIndicator, next: &'a BackupIndicator) -> Option<&'a str> {
    match (prev, next) {
        (BackupIndicator::Failed(old), BackupIndicator::Failed(new)) if old == new => None,
        (_, BackupIndicator::Failed(new)) => Some(new),
        _ => None,
    }
}

/// Short warning text for the header, `None` when nothing should show.
pub fn warning_text(indicator: &BackupIndicator, now: u64) -> Option<String> {
    match indicator {
        BackupIndicator::Failed(_) => Some("backup failed".to_string()),
        BackupIndicator::Stale(at) => Some(format!(
            "no backup in {}",
            format_age(now.saturating_sub(*at))
        )),
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
        Check::Ran(Ok(None)) => (
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

    #[test]
    fn off_without_path_or_projects() {
        assert_eq!(
            indicator(&Check::NoPath, Some(&ok_at(NOW)), NOW),
            BackupIndicator::Off(OffReason::NoPath)
        );
        assert_eq!(
            indicator(&Check::Ran(Ok(None)), None, NOW),
            BackupIndicator::Off(OffReason::NoProjects)
        );
    }

    #[test]
    fn run_error_is_a_failure() {
        let ind = indicator(&Check::Ran(Err("tar failed".into())), None, NOW);
        assert_eq!(ind, BackupIndicator::Failed("tar failed".into()));
    }

    #[test]
    fn success_is_healthy() {
        let ind = indicator(&Check::Ran(Ok(Some(true))), Some(&ok_at(NOW)), NOW);
        assert_eq!(ind, BackupIndicator::Healthy);
    }

    #[test]
    fn skipped_reads_the_record() {
        // A failed `bork backup` from the CLI shows up on the next check.
        assert_eq!(
            indicator(
                &Check::Skipped,
                Some(&failed(Some(NOW - HOUR), "no dir")),
                NOW
            ),
            BackupIndicator::Failed("no dir".into())
        );
        assert_eq!(
            indicator(&Check::Skipped, Some(&ok_at(NOW - HOUR)), NOW),
            BackupIndicator::Healthy
        );
        assert_eq!(
            indicator(&Check::Skipped, None, NOW),
            BackupIndicator::Healthy
        );
    }

    #[test]
    fn stale_after_two_days() {
        assert_eq!(
            from_record(Some(&ok_at(NOW - 47 * HOUR)), NOW),
            BackupIndicator::Healthy
        );
        assert_eq!(
            from_record(Some(&ok_at(NOW - 49 * HOUR)), NOW),
            BackupIndicator::Stale(NOW - 49 * HOUR)
        );
    }

    #[test]
    fn failure_wins_over_staleness() {
        assert_eq!(
            from_record(Some(&failed(Some(NOW - 72 * HOUR), "gone")), NOW),
            BackupIndicator::Failed("gone".into())
        );
    }

    #[test]
    fn reports_only_new_failures() {
        let healthy = BackupIndicator::Healthy;
        let a = BackupIndicator::Failed("a".into());
        let b = BackupIndicator::Failed("b".into());
        assert_eq!(failure_to_report(&healthy, &a), Some("a"));
        assert_eq!(failure_to_report(&BackupIndicator::Unknown, &a), Some("a"));
        assert_eq!(failure_to_report(&a, &a), None);
        assert_eq!(failure_to_report(&a, &b), Some("b"));
        assert_eq!(failure_to_report(&a, &healthy), None);
    }

    #[test]
    fn warning_text_only_for_warnings() {
        assert_eq!(warning_text(&BackupIndicator::Healthy, NOW), None);
        assert_eq!(
            warning_text(&BackupIndicator::Off(OffReason::NoPath), NOW),
            None
        );
        assert_eq!(warning_text(&BackupIndicator::Unknown, NOW), None);
        assert_eq!(
            warning_text(&BackupIndicator::Failed("x".into()), NOW).as_deref(),
            Some("backup failed")
        );
        assert_eq!(
            warning_text(&BackupIndicator::Stale(NOW - 72 * HOUR), NOW).as_deref(),
            Some("no backup in 3d")
        );
    }

    #[test]
    fn manual_messages() {
        assert!(manual_message(&Check::NoPath).1);
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
