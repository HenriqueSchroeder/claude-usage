//! The last good quota reading, kept on disk.
//!
//! `last_quota` lives in memory, so a daemon that restarts while the endpoint
//! is unreachable paints an empty bar over a reading it held seconds earlier.
//! That is the one failure the in-memory fallback cannot cover, because the
//! process that remembers is the process that died.

use crate::quota::{BreakdownRow, Quota, Window};
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::OffsetDateTime;

/// A reading older than the longest quota window describes a world that has
/// since rolled over at least once. Past that age the numbers are not stale
/// truth any more, just noise, so the cache is dropped and the bar admits it
/// has nothing rather than invent confidence.
const MAX_AGE_SECONDS: i64 = 7 * 24 * 3600;

/// The on-disk shape, deliberately not `Quota` itself.
///
/// Timestamps are whole seconds: `events` truncates to seconds and tolerates
/// a 60-second margin, and the bar shows minutes, so sub-second precision
/// buys nothing — while serializing `OffsetDateTime` directly would pull
/// `time`'s `serde-well-known` (and with it `formatting`) into a binary built
/// for size. Keeping the format separate from `Quota` also means changing the
/// in-memory struct cannot silently invalidate a file already on disk.
#[derive(Serialize, Deserialize)]
struct Stored {
    fetched_at: i64,
    /// `null` when there was no five-hour window to store.
    #[serde(default)]
    session: Option<StoredWindow>,
    weekly: StoredWindow,
    #[serde(default)]
    breakdown: Vec<StoredRow>,
}

#[derive(Serialize, Deserialize)]
struct StoredWindow {
    percent: f32,
    #[serde(default)]
    resets_at: Option<i64>,
    #[serde(default)]
    severity: String,
}

#[derive(Serialize, Deserialize)]
struct StoredRow {
    display_name: String,
    percent: f32,
}

impl From<&Window> for StoredWindow {
    fn from(w: &Window) -> Self {
        StoredWindow {
            percent: w.percent,
            resets_at: w.resets_at.map(OffsetDateTime::unix_timestamp),
            severity: w.severity.as_str().to_string(),
        }
    }
}

impl From<&StoredWindow> for Window {
    fn from(w: &StoredWindow) -> Self {
        Window {
            percent: w.percent,
            resets_at: w
                .resets_at
                .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok()),
            // An unknown or absent name reads back as Normal, the same way a
            // response with no severity does.
            severity: w.severity.as_str().into(),
        }
    }
}

/// Writes `bytes` to `path` through a sibling temp file and a rename.
///
/// A crash partway through a plain write leaves a truncated file that the
/// next read has to discard. Rename is atomic, so a reader sees either the
/// old contents or the new ones, never half of either.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);

    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            std::fs::remove_file(&tmp).ok();
            Err(e)
        }
    }
}

pub fn save(path: &Path, quota: &Quota, fetched_at: OffsetDateTime) {
    let stored = Stored {
        fetched_at: fetched_at.unix_timestamp(),
        session: quota.session.as_ref().map(Into::into),
        weekly: (&quota.weekly).into(),
        breakdown: quota
            .breakdown
            .iter()
            .map(|r| StoredRow {
                display_name: r.display_name.clone(),
                percent: r.percent,
            })
            .collect(),
    };
    if let Ok(json) = serde_json::to_vec(&stored) {
        atomic_write(path, &json).ok();
    }
}

/// The cached reading and the moment it was taken, or `None` when there is
/// nothing usable on disk.
///
/// A cache dated in the future is discarded along with an expired one: a
/// clock that jumped backwards, or a state directory copied from another
/// machine, would otherwise produce a negative age that becomes an enormous
/// number the moment it is rendered as unsigned.
pub fn load(path: &Path, now: OffsetDateTime) -> Option<(Quota, OffsetDateTime)> {
    let bytes = std::fs::read(path).ok()?;
    let stored: Stored = serde_json::from_slice(&bytes).ok()?;
    let fetched_at = OffsetDateTime::from_unix_timestamp(stored.fetched_at).ok()?;

    let age = (now - fetched_at).whole_seconds();
    if !(0..=MAX_AGE_SECONDS).contains(&age) {
        return None;
    }

    let quota = Quota {
        session: stored.session.as_ref().map(Into::into),
        weekly: (&stored.weekly).into(),
        breakdown: stored
            .breakdown
            .iter()
            .map(|r| BreakdownRow {
                display_name: r.display_name.clone(),
                percent: r.percent,
            })
            .collect(),
    };
    Some((quota, fetched_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::Severity;
    use std::path::PathBuf;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-18 12:00 UTC);

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cu-persist-{}-{}", name, std::process::id()));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample() -> Quota {
        Quota {
            session: Some(Window {
                percent: 42.5,
                resets_at: Some(datetime!(2026-09-18 15:30 UTC)),
                severity: Severity::Warning,
            }),
            weekly: Window {
                percent: 83.0,
                resets_at: Some(datetime!(2026-09-22 07:00 UTC)),
                severity: Severity::Critical,
            },
            breakdown: vec![BreakdownRow {
                display_name: "Claude Code".into(),
                percent: 61.0,
            }],
        }
    }

    #[test]
    fn a_saved_reading_comes_back_intact() {
        let dir = scratch("roundtrip");
        let path = dir.join("quota.json");
        let taken = NOW - time::Duration::minutes(3);

        save(&path, &sample(), taken);
        let (q, at) = load(&path, NOW).expect("just written");

        assert_eq!(at, taken);
        let session = q.session.as_ref().expect("the session window survives");
        assert_eq!(session.percent, 42.5);
        assert_eq!(q.weekly.percent, 83.0);
        assert_eq!(session.resets_at, Some(datetime!(2026-09-18 15:30 UTC)));
        assert_eq!(q.weekly.resets_at, Some(datetime!(2026-09-22 07:00 UTC)));
        // Severity drives the icon colour; a reading that came back Normal
        // would quietly turn a red ring green across a restart.
        assert_eq!(session.severity, Severity::Warning);
        assert_eq!(q.weekly.severity, Severity::Critical);
        assert_eq!(q.breakdown.len(), 1);
        assert_eq!(q.breakdown[0].display_name, "Claude Code");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn no_session_window_stays_none_across_a_restart() {
        // Coming back as a 0% window would put a session number on the bar
        // of a plan that has no such limit.
        let dir = scratch("no-session");
        let path = dir.join("quota.json");
        let q = Quota {
            session: None,
            ..sample()
        };

        save(&path, &q, NOW);
        let (back, _) = load(&path, NOW).expect("just written");
        assert!(back.session.is_none());
        assert_eq!(back.weekly.percent, 83.0);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_reading_older_than_the_longest_window_is_dropped() {
        let dir = scratch("stale");
        let path = dir.join("quota.json");

        // Eight days: both windows have rolled over since, so these numbers
        // describe nothing that still exists.
        save(&path, &sample(), NOW - time::Duration::days(8));
        assert!(load(&path, NOW).is_none());

        // One day is still worth showing — stale, and `health` says so.
        save(&path, &sample(), NOW - time::Duration::days(1));
        assert!(load(&path, NOW).is_some());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_reading_from_the_future_is_dropped() {
        let dir = scratch("future");
        let path = dir.join("quota.json");

        // A clock stepped backwards, or a state directory copied off another
        // machine. Age would be negative, and a negative age rendered as
        // unsigned is billions of seconds of apparent staleness.
        save(&path, &sample(), NOW + time::Duration::hours(2));
        assert!(load(&path, NOW).is_none());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn nothing_readable_is_never_a_crash() {
        let dir = scratch("junk");

        assert!(load(&dir.join("absent.json"), NOW).is_none());

        let corrupt = dir.join("corrupt.json");
        std::fs::write(&corrupt, b"{\"fetched_at\": tru").unwrap();
        assert!(load(&corrupt, NOW).is_none());

        // Valid JSON, wrong shape: the windows the daemon needs are absent.
        let wrong = dir.join("wrong.json");
        std::fs::write(&wrong, b"{\"fetched_at\":0}").unwrap();
        assert!(load(&wrong, NOW).is_none());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn atomic_write_creates_the_directory_and_leaves_no_temp_behind() {
        let dir = scratch("atomic");
        let path = dir.join("nested/deeper/state.json");

        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(!path.with_file_name("state.json.tmp").exists());

        // Overwriting is the common case — the daemon rewrites both files
        // every cycle — and must not leave the old contents behind.
        atomic_write(&path, b"bye").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"bye");

        std::fs::remove_dir_all(dir).ok();
    }
}
