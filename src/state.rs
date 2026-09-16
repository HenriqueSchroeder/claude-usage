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
    pub fn session_pct(&self) -> f32 {
        self.quota.as_ref().map_or(0.0, |q| q.session.percent)
    }

    pub fn weekly_pct(&self) -> f32 {
        self.quota.as_ref().map_or(0.0, |q| q.weekly.percent)
    }

    /// Time until the nearest upcoming window reset, if any.
    pub fn reset_in(&self, now: OffsetDateTime) -> Option<Duration> {
        let q = self.quota.as_ref()?;
        [q.session.resets_at, q.weekly.resets_at]
            .into_iter()
            .flatten()
            .map(|t| t - now)
            .filter(|d| d.is_positive())
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn empty_snapshot_reports_zero_not_panic() {
        let s = Snapshot::default();
        assert_eq!(s.session_pct(), 0.0);
        assert_eq!(s.weekly_pct(), 0.0);
        assert!(s.reset_in(datetime!(2026-09-15 14:00 UTC)).is_none());
    }

    #[test]
    fn reset_in_uses_the_nearest_upcoming_window() {
        let mut q = crate::quota::Quota::default();
        q.session.resets_at = Some(datetime!(2026-09-15 15:40 UTC));
        q.weekly.resets_at = Some(datetime!(2026-09-16 07:00 UTC));
        let s = Snapshot {
            quota: Some(q),
            ..Default::default()
        };
        let d = s.reset_in(datetime!(2026-09-15 14:40 UTC)).unwrap();
        assert_eq!(d.whole_minutes(), 60);
    }

    #[test]
    fn already_past_reset_is_not_negative() {
        let mut q = crate::quota::Quota::default();
        q.session.resets_at = Some(datetime!(2026-09-15 10:00 UTC));
        let s = Snapshot {
            quota: Some(q),
            ..Default::default()
        };
        assert_eq!(s.reset_in(datetime!(2026-09-15 14:00 UTC)), None);
    }
}
