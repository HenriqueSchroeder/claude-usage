use crate::quota::Quota;
use crate::session::SessionInfo;
use time::{Duration, OffsetDateTime};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Health {
    #[default]
    Ok,
    Stale {
        seconds: u64,
    },
    Unauthorized,
    NoCredentials,
}

/// The single source of truth for everything on screen.
///
/// Renderers read from this and keep no copy of their own; that is what
/// stops the bar text and the menu from showing different numbers.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub quota: Option<Quota>,
    pub session: Option<SessionInfo>,
    pub health: Health,
    /// `None` when the session used a model with no entry in the price table,
    /// which is not the same thing as a session that cost nothing.
    pub cost_usd: Option<f64>,
}

impl Snapshot {
    /// `None` when there is no five-hour window to show, which callers must
    /// leave out rather than draw as 0%.
    ///
    /// Both percentages come back rounded to the whole number the panel
    /// shows, so a rule or a notification level is judged against the same
    /// figure the user reads: "95%" on the bar and `above = 95` not matching
    /// was the raw 94.6 being compared behind the label's back.
    pub fn session_pct(&self) -> Option<f32> {
        Some(self.quota.as_ref()?.session.as_ref()?.percent.round())
    }

    pub fn weekly_pct(&self) -> f32 {
        self.quota
            .as_ref()
            .map_or(0.0, |q| q.weekly.percent.round())
    }

    /// Time until the 5-hour window clears, if the endpoint said when.
    pub fn session_reset_in(&self, now: OffsetDateTime) -> Option<Duration> {
        until(self.quota.as_ref()?.session.as_ref()?.resets_at, now)
    }

    /// Time until the 7-day window clears, if the endpoint said when.
    pub fn weekly_reset_in(&self, now: OffsetDateTime) -> Option<Duration> {
        until(self.quota.as_ref()?.weekly.resets_at, now)
    }

    /// Time until the *nearest* upcoming window reset, whichever that is.
    ///
    /// In practice that is nearly always the 5-hour one, which is why
    /// `{session_reset}` and `{weekly_reset}` exist: a template that says
    /// "weekly" beside this number is saying something false.
    pub fn reset_in(&self, now: OffsetDateTime) -> Option<Duration> {
        [self.session_reset_in(now), self.weekly_reset_in(now)]
            .into_iter()
            .flatten()
            .min()
    }
}

fn until(at: Option<OffsetDateTime>, now: OffsetDateTime) -> Option<Duration> {
    let remaining = at? - now;
    remaining.is_positive().then_some(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn empty_snapshot_reports_zero_not_panic() {
        let s = Snapshot::default();
        assert_eq!(s.session_pct(), None);
        assert_eq!(s.weekly_pct(), 0.0);
        assert!(s.reset_in(datetime!(2026-09-15 14:00 UTC)).is_none());
    }

    #[test]
    fn reset_in_uses_the_nearest_upcoming_window() {
        let mut q = crate::quota::Quota::default();
        q.session.get_or_insert_default().resets_at = Some(datetime!(2026-09-15 15:40 UTC));
        q.weekly.resets_at = Some(datetime!(2026-09-16 07:00 UTC));
        let s = Snapshot {
            quota: Some(q),
            ..Default::default()
        };
        let d = s.reset_in(datetime!(2026-09-15 14:40 UTC)).unwrap();
        assert_eq!(d.whole_minutes(), 60);
    }

    #[test]
    fn each_window_reports_its_own_reset() {
        let mut q = crate::quota::Quota::default();
        q.session.get_or_insert_default().resets_at = Some(datetime!(2026-09-15 15:40 UTC));
        q.weekly.resets_at = Some(datetime!(2026-09-19 07:00 UTC));
        let s = Snapshot {
            quota: Some(q),
            ..Default::default()
        };
        let now = datetime!(2026-09-15 14:40 UTC);

        // The nearest is the 5-hour one, as it almost always is...
        assert_eq!(s.reset_in(now).unwrap().whole_minutes(), 60);
        assert_eq!(s.session_reset_in(now).unwrap().whole_minutes(), 60);
        // ...but the weekly window is nearly four days out, and a template
        // saying "weekly" must be able to say so truthfully.
        assert_eq!(s.weekly_reset_in(now).unwrap().whole_hours(), 88);
    }

    #[test]
    fn a_window_with_no_reset_time_reports_none_not_the_other_window() {
        let mut q = crate::quota::Quota::default();
        q.session.get_or_insert_default().resets_at = Some(datetime!(2026-09-15 15:40 UTC));
        let s = Snapshot {
            quota: Some(q),
            ..Default::default()
        };
        assert!(s.weekly_reset_in(datetime!(2026-09-15 14:40 UTC)).is_none());
    }

    #[test]
    fn already_past_reset_is_not_negative() {
        let mut q = crate::quota::Quota::default();
        q.session.get_or_insert_default().resets_at = Some(datetime!(2026-09-15 10:00 UTC));
        let s = Snapshot {
            quota: Some(q),
            ..Default::default()
        };
        assert_eq!(s.reset_in(datetime!(2026-09-15 14:00 UTC)), None);
    }
}
