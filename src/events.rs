use crate::config::ThresholdConfig;
use crate::state::{Health, Snapshot};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Session,
    Weekly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ThresholdCrossed { metric: Metric, level: u8 },
    WindowReset { metric: Metric },
    Unauthorized,
}

/// A `resets_at` only counts as a fresh window when it advances by more than
/// this many seconds.
///
/// The API returns `resets_at` with microsecond precision that changes on
/// every call even when the underlying reset instant hasn't moved — two live
/// calls 20 seconds apart have been observed returning the same reset
/// instant with different fractional seconds (`.400922` vs `.693440`).
/// Comparing whole-second-truncated timestamps with a bare `>` treats that
/// jitter as a reset the moment its fractional part crosses a second
/// boundary. The margin has to clear that noise (observed well under a
/// second) with room to spare, while staying far below the smallest real
/// advance, which is the 5-hour session window (18 000 seconds). 60 seconds
/// clears the noise by roughly two orders of magnitude and stays 300 times
/// smaller than the smallest genuine reset.
const RESET_MARGIN_SECONDS: i64 = 60;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    armed_session: Vec<u8>,
    #[serde(default)]
    armed_weekly: Vec<u8>,
    #[serde(default)]
    last_session_reset: Option<i64>,
    #[serde(default)]
    last_weekly_reset: Option<i64>,
    #[serde(default)]
    seen: bool,
    #[serde(default)]
    notified_unauthorized: bool,
}

pub struct Detector {
    cfg: ThresholdConfig,
    state: Persisted,
}

impl Detector {
    pub fn new(cfg: ThresholdConfig) -> Self {
        Detector {
            cfg,
            state: Persisted::default(),
        }
    }

    pub fn load(path: &Path, cfg: ThresholdConfig) -> Self {
        let mut detector = Detector::new(cfg);
        if let Some(state) = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            detector.state = state;
        }
        detector
    }

    /// Writes atomically, because a truncated state file is worse than none.
    ///
    /// A crash partway through a plain write leaves a file `load` cannot
    /// deserialise; it falls back to a default with `seen: false` and treats
    /// the next poll as a first run — which is silent by design. The
    /// threshold crossing after a bad shutdown would simply never be
    /// announced.
    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_vec(&self.state) {
            crate::persist::atomic_write(path, &json).ok();
        }
    }

    /// Compares this snapshot against the previous one and reports events.
    ///
    /// The first call after start reports no threshold crossings and no
    /// window resets: without a previous state, every threshold already
    /// crossed would fire at once and every resets_at would look like a
    /// rollover, so the daemon would notify everything on every boot.
    /// Unauthorized is the one exception: it is deduped across polls like
    /// the rest, but still reported the moment it is seen, first cycle
    /// included, since a user cannot fix an expired token nobody mentioned.
    pub fn diff(&mut self, snap: &Snapshot) -> Vec<Event> {
        let mut events = Vec::new();

        if snap.health == Health::Unauthorized {
            if !self.state.notified_unauthorized {
                events.push(Event::Unauthorized);
                self.state.notified_unauthorized = true;
            }
        } else {
            self.state.notified_unauthorized = false;
        }

        let first_run = !self.state.seen;
        self.state.seen = true;

        for metric in [Metric::Session, Metric::Weekly] {
            let (pct, resets_at) = match (metric, snap.quota.as_ref()) {
                (_, None) => continue,
                // No five-hour window: nothing to cross and nothing to reset.
                // What the last window left behind is forgotten, so the next
                // one is a first sighting rather than an "advance" that would
                // announce a reset hours late, at the first prompt of the
                // next session.
                (Metric::Session, Some(q)) => match &q.session {
                    Some(w) => (w.percent, w.resets_at),
                    None => {
                        self.state.last_session_reset = None;
                        self.state.armed_session.clear();
                        continue;
                    }
                },
                (Metric::Weekly, Some(q)) => (q.weekly.percent, q.weekly.resets_at),
            };

            let ts = resets_at.map(|t| t.unix_timestamp());
            let last = match metric {
                Metric::Session => &mut self.state.last_session_reset,
                Metric::Weekly => &mut self.state.last_weekly_reset,
            };
            // A resets_at further in the future than the one we last saw means
            // the window rolled over. More reliable than watching the
            // percentage drop, which has other causes — but the timestamp
            // itself carries sub-second server noise, so the advance has to
            // clear RESET_MARGIN_SECONDS before it counts as real.
            if let (Some(new), Some(old)) = (ts, *last) {
                if new > old + RESET_MARGIN_SECONDS && !first_run {
                    events.push(Event::WindowReset { metric });
                }
            }
            if ts.is_some() {
                *last = ts;
            }

            let armed = match metric {
                Metric::Session => &mut self.state.armed_session,
                Metric::Weekly => &mut self.state.armed_weekly,
            };

            // Rearm anything the metric has fallen clear of.
            armed.retain(|lvl| pct >= *lvl as f32 - self.cfg.hysteresis);

            let newly = self
                .cfg
                .levels
                .iter()
                .copied()
                .filter(|lvl| pct >= *lvl as f32 && !armed.contains(lvl))
                .max();

            if let Some(level) = newly {
                for lvl in self.cfg.levels.iter().copied().filter(|l| pct >= *l as f32) {
                    if !armed.contains(&lvl) {
                        armed.push(lvl);
                    }
                }
                if !first_run {
                    events.push(Event::ThresholdCrossed { metric, level });
                }
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThresholdConfig;
    use crate::quota::Quota;
    use crate::state::{Health, Snapshot};
    use time::macros::datetime;

    fn cfg() -> ThresholdConfig {
        ThresholdConfig {
            levels: vec![75, 90, 95],
            hysteresis: 2.0,
            notify: true,
        }
    }

    fn snap(weekly: f32) -> Snapshot {
        let mut q = Quota::default();
        q.weekly.percent = weekly;
        q.weekly.resets_at = Some(datetime!(2026-09-16 07:00 UTC));
        Snapshot {
            quota: Some(q),
            ..Default::default()
        }
    }

    fn snap_session(session: f32) -> Snapshot {
        let mut q = Quota::default();
        q.session.get_or_insert_default().percent = session;
        q.session.get_or_insert_default().resets_at = Some(datetime!(2026-09-16 07:00 UTC));
        Snapshot {
            quota: Some(q),
            ..Default::default()
        }
    }

    fn snap_with_health(health: Health) -> Snapshot {
        Snapshot {
            health,
            ..snap(50.0)
        }
    }

    #[test]
    fn first_cycle_is_silent() {
        let mut d = Detector::new(cfg());
        assert!(d.diff(&snap(98.0)).is_empty());
    }

    #[test]
    fn crossing_a_level_upward_fires_once() {
        let mut d = Detector::new(cfg());
        d.diff(&snap(70.0));
        let evs = d.diff(&snap(80.0));
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], Event::ThresholdCrossed { level: 75, .. }));
    }

    #[test]
    fn oscillating_inside_hysteresis_does_not_refire() {
        let mut d = Detector::new(cfg());
        d.diff(&snap(70.0));
        d.diff(&snap(90.1));
        assert!(d.diff(&snap(89.8)).is_empty());
        assert!(d.diff(&snap(90.2)).is_empty());
    }

    #[test]
    fn falling_below_hysteresis_rearms_the_level() {
        let mut d = Detector::new(cfg());
        d.diff(&snap(70.0));
        d.diff(&snap(76.0));
        d.diff(&snap(72.0)); // below 75 - 2
        let evs = d.diff(&snap(76.0));
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn crossing_several_levels_at_once_fires_only_the_highest() {
        let mut d = Detector::new(cfg());
        d.diff(&snap(10.0));
        let evs = d.diff(&snap(96.0));
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], Event::ThresholdCrossed { level: 95, .. }));
    }

    #[test]
    fn a_later_resets_at_is_a_window_reset() {
        let mut d = Detector::new(cfg());
        d.diff(&snap(98.0));
        let mut next = snap(3.0);
        next.quota.as_mut().unwrap().weekly.resets_at = Some(datetime!(2026-09-23 07:00 UTC));
        let evs = d.diff(&next);
        assert!(evs.iter().any(|e| matches!(e, Event::WindowReset { .. })));
    }

    #[test]
    fn the_same_resets_at_is_not_a_reset() {
        let mut d = Detector::new(cfg());
        d.diff(&snap(50.0));
        assert!(!d
            .diff(&snap(51.0))
            .iter()
            .any(|e| matches!(e, Event::WindowReset { .. })));
    }

    // The API returns `resets_at` with microsecond precision that changes on
    // every call even when the underlying reset instant hasn't moved. A
    // one-second advance is exactly that noise crossing a second boundary —
    // a genuine reset is five hours (session) or seven days (weekly) away.
    #[test]
    fn a_one_second_jitter_is_not_a_window_reset_weekly() {
        let mut d = Detector::new(cfg());
        let mut base = snap(50.0);
        base.quota.as_mut().unwrap().weekly.resets_at = Some(datetime!(2026-09-16 07:00:00 UTC));
        d.diff(&base);

        let mut jittered = snap(51.0);
        jittered.quota.as_mut().unwrap().weekly.resets_at =
            Some(datetime!(2026-09-16 07:00:01 UTC));
        assert!(!d
            .diff(&jittered)
            .iter()
            .any(|e| matches!(e, Event::WindowReset { .. })));

        let mut real_reset = snap(3.0);
        real_reset.quota.as_mut().unwrap().weekly.resets_at =
            Some(datetime!(2026-09-16 12:00:01 UTC));
        assert!(d
            .diff(&real_reset)
            .iter()
            .any(|e| matches!(e, Event::WindowReset { .. })));
    }

    #[test]
    fn a_session_after_an_idle_gap_is_not_announced_as_a_reset() {
        let mut d = Detector::new(cfg());
        let mut before = snap_session(40.0);
        before
            .quota
            .as_mut()
            .unwrap()
            .session
            .get_or_insert_default()
            .resets_at = Some(datetime!(2026-09-16 07:00 UTC));
        d.diff(&before);

        let mut idle = snap_session(0.0);
        idle.quota.as_mut().unwrap().session = None;
        assert!(d.diff(&idle).is_empty());

        let mut next = snap_session(2.0);
        next.quota
            .as_mut()
            .unwrap()
            .session
            .get_or_insert_default()
            .resets_at = Some(datetime!(2026-09-16 15:00 UTC));
        assert!(!d
            .diff(&next)
            .iter()
            .any(|e| matches!(e, Event::WindowReset { .. })));
    }

    #[test]
    fn a_one_second_jitter_is_not_a_window_reset_session() {
        let mut d = Detector::new(cfg());
        let mut base = snap_session(50.0);
        base.quota
            .as_mut()
            .unwrap()
            .session
            .get_or_insert_default()
            .resets_at = Some(datetime!(2026-09-16 07:00:00 UTC));
        d.diff(&base);

        let mut jittered = snap_session(51.0);
        jittered
            .quota
            .as_mut()
            .unwrap()
            .session
            .get_or_insert_default()
            .resets_at = Some(datetime!(2026-09-16 07:00:01 UTC));
        assert!(!d
            .diff(&jittered)
            .iter()
            .any(|e| matches!(e, Event::WindowReset { .. })));

        let mut real_reset = snap_session(3.0);
        real_reset
            .quota
            .as_mut()
            .unwrap()
            .session
            .get_or_insert_default()
            .resets_at = Some(datetime!(2026-09-16 12:00:01 UTC));
        assert!(d
            .diff(&real_reset)
            .iter()
            .any(|e| matches!(e, Event::WindowReset { .. })));
    }

    #[test]
    fn state_survives_a_restart() {
        let mut p = std::env::temp_dir();
        p.push(format!("cu-state-{}.json", std::process::id()));
        std::fs::remove_file(&p).ok();

        let mut d = Detector::new(cfg());
        d.diff(&snap(70.0));
        d.diff(&snap(80.0)); // fires 75
        d.save(&p);

        let mut reloaded = Detector::load(&p, cfg());
        assert!(reloaded.diff(&snap(81.0)).is_empty());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let mut p = std::env::temp_dir();
        p.push(format!("cu-atomic-{}.json", std::process::id()));
        let tmp = p.with_extension("json.tmp");
        std::fs::remove_file(&p).ok();
        std::fs::remove_file(&tmp).ok();

        let mut d = Detector::new(cfg());
        d.diff(&snap(80.0));
        d.save(&p);
        d.save(&p); // the second write must overwrite, not trip on the tmp

        assert!(p.exists(), "state file should exist");
        assert!(!tmp.exists(), "temporary file should be renamed away");
        // And it is still readable, which is the point of the rename.
        assert!(Detector::load(&p, cfg()).diff(&snap(81.0)).is_empty());

        std::fs::remove_file(p).ok();
    }

    #[test]
    fn unauthorized_fires_once_then_rearms_after_recovery() {
        let mut d = Detector::new(cfg());
        let bad = snap_with_health(Health::Unauthorized);
        let ok = snap_with_health(Health::Ok);

        // Fires on the very first poll: unlike thresholds, a startup with an
        // already-expired token must say so immediately.
        assert!(d
            .diff(&bad)
            .iter()
            .any(|e| matches!(e, Event::Unauthorized)));
        // Still unauthorized on the next two polls: must not refire.
        assert!(!d
            .diff(&bad)
            .iter()
            .any(|e| matches!(e, Event::Unauthorized)));
        assert!(!d
            .diff(&bad)
            .iter()
            .any(|e| matches!(e, Event::Unauthorized)));

        // Recovers, then expires again: must notify the new episode.
        d.diff(&ok);
        assert!(d
            .diff(&bad)
            .iter()
            .any(|e| matches!(e, Event::Unauthorized)));
    }

    #[test]
    fn crossing_a_level_on_session_leaves_weekly_armed_set_untouched() {
        let mut d = Detector::new(cfg());
        d.diff(&snap_session(70.0));
        let evs = d.diff(&snap_session(80.0));
        assert_eq!(evs.len(), 1);
        assert!(matches!(
            evs[0],
            Event::ThresholdCrossed {
                metric: Metric::Session,
                level: 75
            }
        ));

        // Weekly's armed set is a separate field: an unrelated fresh weekly
        // reading still fires its own crossing instead of coming back armed.
        let weekly_evs = d.diff(&snap(80.0));
        assert_eq!(weekly_evs.len(), 1);
        assert!(matches!(
            weekly_evs[0],
            Event::ThresholdCrossed {
                metric: Metric::Weekly,
                level: 75
            }
        ));
    }
}
