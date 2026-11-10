mod config;
mod creds;
mod diag;
mod events;
mod icon;
mod notify;
mod persist;
mod pricing;
mod quota;
mod render;
mod session;
mod state;
mod tray;

use ksni::blocking::TrayMethods;
use state::{Health, Snapshot};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;

const BACKOFF_CAP_SECONDS: u64 = 900;
/// Floor for `poll_seconds`: a config saying 0 must not turn the loop into
/// a request flood.
const MIN_POLL_SECONDS: u64 = 10;
/// How long a threshold-crossing pulse animates.
///
/// `[bar].rules[]` has no `duration` field of its own — only `effect` — so
/// this is a fixed, short nudge rather than a new config knob nobody asked
/// for. `[reset].duration` is the one animation length that is configurable,
/// because that is the key the owner actually asked for.
const THRESHOLD_PULSE_SECONDS: u64 = 2;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// How often to re-stat `.credentials.json` while backing off. Only while
/// backing off — a healthy loop sleeps through its whole interval.
const CREDS_CHECK_INTERVAL: Duration = Duration::from_secs(15);

fn backoff_seconds(failures: u32, base: u64) -> u64 {
    base.saturating_mul(1u64 << failures.min(10))
        .min(BACKOFF_CAP_SECONDS)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

struct Daemon {
    cfg: config::Config,
    agent: ureq::Agent,
    creds_path: PathBuf,
    state_path: PathBuf,
    projects_dir: PathBuf,
    tailer: Option<session::Tailer>,
    detector: events::Detector,
    notifier: Option<notify::Notifier>,
    /// Where the last reading is cached between runs. Sibling of
    /// `state_path`, and written for the same reason: what the daemon knows
    /// should not die with the process that learned it.
    cache_path: PathBuf,
    /// Last successful reading, kept so a failed poll shows stale truth
    /// instead of a fresh-looking zero.
    last_quota: Option<quota::Quota>,
    /// When every window was last fresh. Absolute rather than an `Instant`
    /// because it is restored from disk at startup, and a monotonic clock
    /// means nothing across a reboot or a suspend.
    last_ok: Option<OffsetDateTime>,
    /// Seconds the server asked us to wait, from the last `Retry-After`.
    retry_after: Option<u64>,
    /// Shared with the tray, so `Atualizar agora` can cut a wait short.
    wakeup: Arc<tray::Wakeup>,
    notes: diag::Notes,
    failures: u32,
}

/// The result of reconciling one fetch against the last known reading.
///
/// `carried_over` names the window (`"five_hour"` or `"seven_day"`) that
/// this response didn't carry and had to be filled in from `last_quota`, or
/// `None` when both windows in this response were fresh.
struct Merged {
    quota: quota::Quota,
    carried_over: Option<&'static str>,
}

impl Daemon {
    fn read_quota(&mut self, snap: &mut Snapshot, now: OffsetDateTime) {
        let token = match creds::read_access_token(&self.creds_path) {
            Ok(t) => t,
            Err(e) => {
                snap.health = Health::NoCredentials;
                self.notes.say(match e {
                    creds::CredsError::Missing => "credentials file not found".into(),
                    creds::CredsError::Unreadable(m) => format!("credentials unreadable: {m}"),
                    creds::CredsError::Malformed(m) => format!("credentials malformed: {m}"),
                });
                return;
            }
        };

        match quota::fetch(&self.agent, &token) {
            Ok(parsed) => match self.merge_quota(parsed) {
                Some(merged) => {
                    match merged.carried_over {
                        None => {
                            snap.health = Health::Ok;
                            self.notes.clear();
                            self.last_ok = Some(now);
                        }
                        Some(window) => {
                            // Only part of this reading is fresh; saying
                            // `Ok` here would be exactly the lie the
                            // original bug told, just for one window
                            // instead of both. `last_ok` is deliberately
                            // left untouched: it now means "the last time
                            // every window was fresh," so the age shown
                            // keeps growing across consecutive partial
                            // polls instead of resetting to near-zero.
                            snap.health = Health::Stale {
                                seconds: self.age_seconds(now),
                            };
                            self.notes.say(format!(
                                "usage response missing the {window} window; \
                                 keeping the last reading for it"
                            ));
                        }
                    }
                    self.failures = 0;
                    self.retry_after = None;
                    // Dated by `last_ok`, not by `now`: a partial merge is
                    // only as fresh as the reading it borrowed from, and
                    // stamping it with the current time would let a run of
                    // partial polls keep a week-old window looking new
                    // across a restart.
                    let fetched_at = self.last_ok.unwrap_or(now);
                    persist::save(&self.cache_path, &merged.quota, fetched_at);
                    self.last_quota = Some(merged.quota);
                }
                None => self.note_failure(
                    snap,
                    quota::FetchError::Parse(
                        "a window is missing from the response and there is no prior \
                         reading to fall back on"
                            .into(),
                    ),
                    now,
                ),
            },
            Err(e) => self.note_failure(snap, e, now),
        }
    }

    /// Fills in whichever window this response didn't carry, from the last
    /// good reading — the per-window twin of how a fully failed poll
    /// already leaves `last_quota` untouched rather than zero it.
    /// `parse_usage` only ever returns `Ok` when at least one of the two
    /// windows is `Some`, so at most one side ever needs a fallback here.
    ///
    /// `None` means a window is missing and there is nothing yet to fall
    /// back on — most likely the very first successful poll dropping a
    /// window — so the caller treats the whole poll like a failed one
    /// rather than show a metric that has never actually been read.
    fn merge_quota(&self, parsed: quota::ParsedUsage) -> Option<Merged> {
        let prior = self.last_quota.as_ref();
        let mut carried_over = None;

        let session = match parsed.session {
            Some(w) => w,
            None => {
                carried_over = Some("five_hour");
                prior?.session.clone()
            }
        };
        let weekly = match parsed.weekly {
            Some(w) => w,
            None => {
                carried_over = Some("seven_day");
                prior?.weekly.clone()
            }
        };

        Some(Merged {
            quota: quota::Quota {
                session,
                weekly,
                breakdown: parsed.breakdown,
            },
            carried_over,
        })
    }

    /// Records a failed poll. Counting happens here, once, for every variant:
    /// an expired refresh token used to be exempt, which meant one 401 per
    /// `poll_seconds`, forever, from an identifiable User-Agent against an
    /// endpoint that is not public. Backing off is the whole point.
    /// How long ago every window was last fresh, in seconds.
    ///
    /// Saturates at zero instead of wrapping: `last_ok` can sit in the future
    /// if the system clock steps backwards, and a negative age cast to `u64`
    /// would render as several billion seconds of staleness.
    fn age_seconds(&self, now: OffsetDateTime) -> u64 {
        self.last_ok
            .map_or(0, |t| (now - t).whole_seconds().max(0) as u64)
    }

    fn note_failure(&mut self, snap: &mut Snapshot, e: quota::FetchError, now: OffsetDateTime) {
        self.failures = self.failures.saturating_add(1);
        let stale = Health::Stale {
            seconds: self.age_seconds(now),
        };
        // Only a 429 carries an interval; anything else leaves the pacing to
        // the exponential backoff, so a stale one must not outlive its cause.
        self.retry_after = None;
        match e {
            quota::FetchError::Unauthorized => {
                snap.health = Health::Unauthorized;
                self.notes
                    .say("token rejected by the usage endpoint".into());
            }
            quota::FetchError::RateLimited { retry_after } => {
                self.retry_after = retry_after;
                snap.health = stale;
                self.notes.say(match retry_after {
                    Some(s) => format!("rate limited; the server asked for {s}s"),
                    None => "rate limited".into(),
                });
            }
            quota::FetchError::Network(m) => {
                snap.health = stale;
                self.notes.say(format!("network: {m}"));
            }
            quota::FetchError::Parse(m) => {
                snap.health = stale;
                self.notes.say(format!("unreadable response: {m}"));
            }
        }
    }

    fn announce(&mut self, event: &events::Event) {
        let wanted = match event {
            events::Event::ThresholdCrossed { .. } => self.cfg.threshold.notify,
            events::Event::WindowReset { .. } => self.cfg.reset.notify,
            events::Event::Unauthorized => true,
        };
        if !wanted {
            return;
        }
        let (summary, body, critical) = notify::message_for(event);
        if let Some(n) = self.notifier.as_mut() {
            n.send(&summary, &body, critical);
        }
    }

    /// Points the tailer at whatever transcript is newest right now.
    ///
    /// At login the newest one is usually yesterday's, so a tailer chosen once
    /// at startup would report a finished session as the current one forever.
    /// A `readdir` per poll is cheap; re-reading a file from offset zero is
    /// not, so an unchanged path keeps its existing tailer. `None` means no
    /// transcript is readable at all, which is no reason to forget the one
    /// already being followed.
    fn refresh_tailer(&mut self) {
        let Some(path) = session::discover_active(&self.projects_dir) else {
            return;
        };
        if self.tailer.as_ref().map(session::Tailer::path) != Some(path.as_path()) {
            self.tailer = Some(session::Tailer::new(path));
        }
    }

    /// One poll: read the quota, fold in the active session, notify on what
    /// changed, and render the bar. Never fails; every problem it meets ends
    /// up in `Snapshot::health`. The events returned are what just happened
    /// this cycle — the caller uses them to decide whether to animate.
    fn cycle(&mut self, now: OffsetDateTime) -> (Snapshot, render::BarOutput, Vec<events::Event>) {
        let mut snap = Snapshot::default();
        self.read_quota(&mut snap, now);
        // The last good reading outlives a failed poll; `health` is what says
        // how much to trust it.
        snap.quota = self.last_quota.clone();

        self.refresh_tailer();
        if let Some(t) = self.tailer.as_mut() {
            let info = t.poll().clone();
            // The newest transcript is not the same thing as a live session.
            // A finished one leaves the snapshot empty, and the menu's whole
            // session section disappears with it.
            if info.is_active(now) {
                snap.cost_usd = pricing::cost_usd(&info.by_model);
                snap.session = Some(info);
            }
        }

        let events = self.detector.diff(&snap);
        for event in &events {
            self.announce(event);
        }
        self.detector.save(&self.state_path);

        let bar = render::render(&self.cfg.bar, &snap, now);
        (snap, bar, events)
    }

    /// How long to animate the icon after this cycle's events, or 0 for none.
    ///
    /// Both mechanisms are event-driven on purpose: a rule that stays
    /// matched, or a `[reset].effect` left at "pulse", must not replay the
    /// animation on every single cycle — only the crossing/reset moment
    /// itself does. A window reset wins if both fire in the same cycle
    /// (rare): it has an explicit, user-configured duration, while the
    /// threshold pulse below is a fixed nudge with no duration of its own.
    fn pulse_seconds(&self, events: &[events::Event], bar: &render::BarOutput) -> u64 {
        let reset_fired = events
            .iter()
            .any(|e| matches!(e, events::Event::WindowReset { .. }));
        if reset_fired {
            if let Some(secs) = self.cfg.reset.pulse_duration() {
                return secs;
            }
        }

        let threshold_fired = events
            .iter()
            .any(|e| matches!(e, events::Event::ThresholdCrossed { .. }));
        if threshold_fired && bar.effect == render::Effect::Pulse {
            return THRESHOLD_PULSE_SECONDS;
        }

        0
    }

    fn wait_seconds(&self) -> u64 {
        let base = self.cfg.poll_seconds.max(MIN_POLL_SECONDS);
        let backoff = match self.failures {
            0 => base,
            n => backoff_seconds(n - 1, base),
        };
        // A 429 is the server naming its own interval; the exponential
        // backoff is only a guess at one. The longer of the two wins, so we
        // never crowd an endpoint that asked for room, and never cut a
        // backoff short either.
        backoff.max(self.retry_after.unwrap_or(0))
    }

    /// Sleeps up to `d`, reporting whether a menu click is what ended it.
    ///
    /// The line is printed unconditionally rather than through `Notes`: that
    /// deduplicates, which is right for a boundary failing on a timer and
    /// wrong for an action a person just took. Two clicks are two events,
    /// and a click that produces no visible change — because the poll it
    /// forced then failed — is exactly when the journal needs to say that
    /// the click itself arrived.
    fn nap(&self, d: Duration) -> bool {
        let woken = self.wakeup.wait(d);
        if woken {
            eprintln!("claude-usage: manual refresh requested");
        }
        woken
    }

    fn creds_mtime(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(&self.creds_path)
            .and_then(|m| m.modified())
            .ok()
    }

    /// Waits until the next poll is due.
    ///
    /// `pulse_debt` is how many seconds the previous cycle already spent
    /// animating the icon (0 most of the time); it is subtracted from the
    /// normal interval so the animation eats into the existing gap between
    /// polls instead of adding to it. If a pulse ever ran longer than the
    /// gap itself (`[reset].duration` set past `poll_seconds`, especially at
    /// the 10s floor), the saturating subtraction floors at zero rather than
    /// underflowing, and the next poll simply lands late by the difference —
    /// a documented cost, not a panic.
    ///
    /// Every sleep here is interruptible by `Atualizar agora`; the menu is
    /// only ever clicked while this function is the one running.
    ///
    /// While healthy this is a plain sleep. While backing off it also watches
    /// the credentials file: Claude Code rewrites it when the user
    /// re-authenticates, and making them sit out a 15-minute backoff after
    /// they have already fixed the problem is its own kind of broken.
    ///
    /// A manual refresh deliberately leaves `failures` alone. It gets its
    /// poll immediately, and if that poll also fails the backoff carries on
    /// from where it was — clicking cannot be turned into a way to hammer an
    /// endpoint that is already saying no.
    fn wait(&mut self, pulse_debt: u64) {
        let seconds = self.wait_seconds().saturating_sub(pulse_debt);
        if self.failures == 0 {
            self.nap(Duration::from_secs(seconds));
            return;
        }

        let before = self.creds_mtime();
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if self.nap(CREDS_CHECK_INTERVAL.min(left)) {
                return;
            }
            if self.creds_mtime() != before {
                return;
            }
        }
    }
}

/// What `animate` needs from a tray handle: push a mutation, report whether
/// anything was still there to receive it. `ksni::blocking::Handle` is the
/// only real implementation; tests substitute a same-process fake so the
/// frame sequence can be checked without a D-Bus connection or a display.
trait FramePusher {
    fn push(&self, f: impl FnOnce(&mut tray::Indicator)) -> bool;
}

impl FramePusher for ksni::blocking::Handle<tray::Indicator> {
    fn push(&self, f: impl FnOnce(&mut tray::Indicator)) -> bool {
        self.update(f).is_some()
    }
}

/// Plays a pulse on the tray icon, one frame at a time, then clears the
/// override so the icon falls back to whatever `bar`/`snap` already imply.
///
/// Deliberately not a background thread: `ksni`'s `Handle::update` is the
/// only way to change what the tray shows, and if a second thread called it
/// on its own timer while this loop's normal per-cycle update also called
/// it, the two would race — whichever update lands last wins, which can
/// leave the icon stuck mid-pulse. Calling it only from here, synchronously,
/// between cycles, means there is exactly one writer, ever.
///
/// A shutdown does not wait on this: the "Sair" menu item calls
/// `std::process::exit` directly from `ksni`'s own dispatch thread, which
/// tears down the whole process — including this thread, mid-sleep if need
/// be — regardless of how many frames are left.
fn animate(handle: &impl FramePusher, count: usize) {
    for i in 0..count {
        if !handle.push(move |t: &mut tray::Indicator| {
            t.pulse = Some((i, count));
        }) {
            return;
        }
        std::thread::sleep(Duration::from_millis(icon::FRAME_INTERVAL_MS));
    }
    handle.push(|t: &mut tray::Indicator| {
        t.pulse = None;
    });
}

fn main() {
    let cfg = config::load(&home().join(".config/claude-usage/config.toml"));
    let state_path = home().join(".local/state/claude-usage/state.json");
    let cache_path = state_path.with_file_name("quota.json");
    let now = OffsetDateTime::now_utc();
    // Restores the last reading so a restart mid-outage shows stale truth
    // rather than a blank bar. `health` still reports the age, so nothing
    // here passes a cached number off as a current one.
    let (last_quota, last_ok) = persist::load(&cache_path, now).unzip();
    let wakeup = Arc::new(tray::Wakeup::default());

    let mut daemon = Daemon {
        agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
        creds_path: home().join(".claude/.credentials.json"),
        detector: events::Detector::load(&state_path, cfg.threshold.clone()),
        notifier: notify::Notifier::new(),
        projects_dir: home().join(".claude/projects"),
        // Filled by the first cycle's `refresh_tailer`.
        tailer: None,
        state_path,
        cache_path,
        cfg,
        last_quota,
        last_ok,
        retry_after: None,
        wakeup: Arc::clone(&wakeup),
        notes: diag::Notes::default(),
        failures: 0,
    };

    // First reading before the item appears, so the panel never shows a
    // placeholder percentage. The first cycle never yields events (the
    // detector is silent on its first diff), so there is nothing to animate.
    let (snap, bar, _events) = daemon.cycle(now);

    let handle = match tray::Indicator::new(bar, snap, wakeup)
        .assume_sni_available(true)
        .spawn()
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("claude-usage: could not register the tray item: {e}");
            return;
        }
    };

    let mut pulse_debt = 0;
    loop {
        daemon.wait(pulse_debt);
        let (snap, bar, events) = daemon.cycle(OffsetDateTime::now_utc());
        let pulse_secs = daemon.pulse_seconds(&events, &bar);
        let updated = handle.update(move |t: &mut tray::Indicator| {
            t.bar = bar;
            t.snap = snap;
            t.pulse = None;
        });
        if updated.is_none() {
            // Nothing left to draw on. Exit non-zero on purpose: a clean exit
            // would leave `Restart=on-failure` satisfied and the user without
            // an indicator and without a line in the journal saying why.
            eprintln!("claude-usage: lost the tray connection, exiting to be restarted");
            std::process::exit(1);
        }

        pulse_debt = 0;
        if pulse_secs > 0 {
            animate(&handle, icon::frame_count(pulse_secs));
            pulse_debt = pulse_secs;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::SystemTime;
    use time::macros::datetime;

    /// Shortly after the timestamp `write_transcript` puts in every line, so
    /// the session those lines describe counts as live.
    const NOW: OffsetDateTime = datetime!(2026-09-15 14:50 UTC);

    #[test]
    fn backoff_doubles_and_caps_at_fifteen_minutes() {
        assert_eq!(backoff_seconds(0, 60), 60);
        assert_eq!(backoff_seconds(1, 60), 120);
        assert_eq!(backoff_seconds(2, 60), 240);
        assert_eq!(backoff_seconds(10, 60), 900);
    }

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cu-main-{}-{}", name, std::process::id()));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A daemon pointed at paths that do not exist, so `cycle` takes the
    /// no-credentials branch: no network, no D-Bus, no notifier.
    fn offline_daemon(dir: &Path, last_quota: Option<quota::Quota>) -> Daemon {
        Daemon {
            cfg: config::Config::default(),
            agent: ureq::AgentBuilder::new().build(),
            creds_path: dir.join("absent/.credentials.json"),
            state_path: dir.join("state.json"),
            cache_path: dir.join("quota.json"),
            projects_dir: dir.join("projects"),
            tailer: None,
            detector: events::Detector::new(config::ThresholdConfig::default()),
            notifier: None,
            last_quota,
            last_ok: None,
            retry_after: None,
            wakeup: Arc::new(tray::Wakeup::default()),
            notes: diag::Notes::default(),
            failures: 0,
        }
    }

    #[test]
    fn every_kind_of_failed_poll_counts_toward_the_backoff() {
        let dir = scratch("backoff");
        let mut d = offline_daemon(&dir, None);
        let mut snap = Snapshot::default();

        // Unauthorized used to be the exempt one: a dead refresh token meant
        // one 401 per poll, 1440 a day, against an endpoint that is not
        // public. It backs off like everything else now.
        d.note_failure(&mut snap, quota::FetchError::Unauthorized, NOW);
        assert_eq!(snap.health, Health::Unauthorized);
        assert_eq!(d.wait_seconds(), 60);
        d.note_failure(&mut snap, quota::FetchError::Unauthorized, NOW);
        assert_eq!(d.wait_seconds(), 120);
        for _ in 0..10 {
            d.note_failure(&mut snap, quota::FetchError::Unauthorized, NOW);
        }
        assert_eq!(d.wait_seconds(), BACKOFF_CAP_SECONDS);

        // A success clears it, so recovery is immediate.
        d.failures = 0;
        assert_eq!(d.wait_seconds(), 60);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_failed_poll_keeps_the_last_reading_instead_of_zeroing_it() {
        let dir = scratch("carry");
        let mut q = quota::Quota::default();
        q.weekly.percent = 83.0;
        let mut d = offline_daemon(&dir, Some(q));

        let (snap, bar, _events) = d.cycle(NOW);

        // The credentials are gone, and health says exactly that...
        assert_eq!(snap.health, Health::NoCredentials);
        assert_eq!(bar.text, "⚠ sem credencial");
        // ...but the menu still shows the last reading rather than a
        // fresh-looking 0%.
        assert_eq!(snap.weekly_pct(), 83.0);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_failed_poll_with_nothing_ever_read_carries_no_quota() {
        let dir = scratch("nocarry");
        let (snap, bar, _events) = offline_daemon(&dir, None).cycle(NOW);
        assert!(snap.quota.is_none());
        assert_eq!(bar.text, "⚠ sem credencial");
        std::fs::remove_dir_all(dir).ok();
    }

    fn quota_with(session: f32, weekly: f32) -> quota::Quota {
        let mut q = quota::Quota::default();
        q.session.get_or_insert_default().percent = session;
        q.weekly.percent = weekly;
        q
    }

    #[test]
    fn merge_quota_fills_a_missing_window_from_the_last_reading() {
        let dir = scratch("merge-fill");
        let d = offline_daemon(&dir, Some(quota_with(92.0, 10.0)));

        let merged = d
            .merge_quota(quota::ParsedUsage {
                session: None,
                weekly: Some(quota::Window {
                    percent: 55.0,
                    ..Default::default()
                }),
                breakdown: vec![],
            })
            .expect("a prior reading exists to merge from");

        // The dropped window is carried forward, not zeroed...
        assert_eq!(merged.quota.session.as_ref().unwrap().percent, 92.0);
        // ...and the fresh one is used as-is.
        assert_eq!(merged.quota.weekly.percent, 55.0);
        assert_eq!(merged.carried_over, Some("five_hour"));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn merge_quota_does_not_mistake_a_genuine_zero_for_a_missing_window() {
        let dir = scratch("merge-zero");
        let d = offline_daemon(&dir, Some(quota_with(92.0, 10.0)));

        let merged = d
            .merge_quota(quota::ParsedUsage {
                session: Some(Some(quota::Window::default())), // a real 0%
                weekly: Some(quota::Window {
                    percent: 11.0,
                    ..Default::default()
                }),
                breakdown: vec![],
            })
            .unwrap();

        // Present-and-zero must overwrite the old 92.0, not be confused
        // with absent-and-carried-forward.
        assert_eq!(merged.quota.session.as_ref().unwrap().percent, 0.0);
        assert_eq!(merged.carried_over, None);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn merge_quota_takes_no_session_window_as_a_reading_not_a_gap() {
        // `five_hour: null` is the endpoint saying there is no such window
        // (idle, or a plan without one). Borrowing the old 92% would keep a
        // stale session number on the bar indefinitely, and failing without
        // a prior reading left fresh installs with nothing at all.
        let dir = scratch("merge-nosession");
        for prior in [Some(quota_with(92.0, 10.0)), None] {
            let d = offline_daemon(&dir, prior);
            let merged = d
                .merge_quota(quota::ParsedUsage {
                    session: Some(None),
                    weekly: Some(quota::Window {
                        percent: 35.0,
                        ..Default::default()
                    }),
                    breakdown: vec![],
                })
                .expect("a complete reading needs nothing to fall back on");
            assert!(merged.quota.session.is_none());
            assert_eq!(merged.carried_over, None);
        }

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn merge_quota_gives_up_when_a_window_is_missing_with_nothing_to_fall_back_on() {
        let dir = scratch("merge-nofallback");
        let d = offline_daemon(&dir, None);

        let merged = d.merge_quota(quota::ParsedUsage {
            session: None,
            weekly: Some(quota::Window {
                percent: 11.0,
                ..Default::default()
            }),
            breakdown: vec![],
        });

        assert!(
            merged.is_none(),
            "the very first poll has nothing to carry a missing window forward from"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_absent_window_does_not_dearm_a_previously_crossed_threshold() {
        // The exact production symptom: five_hour drops out of one
        // response, the fabricated 0% used to dearm level 90, and the next
        // real reading at 92% then looked like a brand new crossing —
        // notification spam for a threshold that was never actually
        // re-crossed.
        let dir = scratch("partial-no-dearm");
        let mut d = offline_daemon(&dir, None);
        d.detector = events::Detector::new(config::ThresholdConfig {
            levels: vec![90],
            hysteresis: 2.0,
            notify: true,
        });

        let snap_for = |q: quota::Quota| Snapshot {
            quota: Some(q),
            ..Default::default()
        };

        // First poll: session crosses 90. The first cycle never emits
        // events by design, but the level is armed internally.
        d.detector.diff(&snap_for(quota_with(92.0, 10.0)));
        d.last_quota = Some(quota_with(92.0, 10.0));

        // Second poll: five_hour is missing from the response. Merging
        // must carry the 92.0 forward instead of feeding a fabricated 0.0
        // into the detector, which would otherwise dearm level 90 here.
        let merged = d
            .merge_quota(quota::ParsedUsage {
                session: None,
                weekly: Some(quota::Window {
                    percent: 11.0,
                    ..Default::default()
                }),
                breakdown: vec![],
            })
            .expect("a prior reading exists to merge from");
        assert_eq!(
            merged.quota.session.as_ref().unwrap().percent,
            92.0,
            "carried forward, not zeroed"
        );
        d.detector.diff(&snap_for(merged.quota));

        // Third poll: five_hour comes back at the same 92%. If the absent
        // window had wrongly dearmed level 90, this would refire
        // ThresholdCrossed even though nothing actually re-crossed it.
        let evs = d.detector.diff(&snap_for(quota_with(92.0, 12.0)));
        assert!(
            evs.is_empty(),
            "an absent window must not cause a false re-crossing: {evs:?}"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    fn write_transcript(path: &Path, out: u64, modified: SystemTime) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!(
                r#"{{"type":"assistant","timestamp":"2026-09-15T14:43:12.045Z","cwd":"/home/x/proj","message":{{"model":"claude-opus-5","usage":{{"input_tokens":1,"output_tokens":{out},"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
            ) + "\n",
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap()
    }

    #[test]
    fn the_tailer_follows_the_session_that_becomes_active() {
        let dir = scratch("follow");
        let old = dir.join("projects/yesterday/a.jsonl");
        let new = dir.join("projects/today/b.jsonl");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        // At startup the newest transcript is the one written most recently.
        write_transcript(&old, 10, t0);
        let mut d = offline_daemon(&dir, None);
        assert_eq!(d.cycle(NOW).0.session.unwrap().tokens.output, 10);

        // An unchanged path must keep its tailer: rebuilding would re-read the
        // whole file from offset zero on every poll, which is the I/O this
        // tailer exists to avoid. (It would not double the totals — `fold`
        // re-derives them from the full content and lands on the same number.)
        assert_eq!(d.cycle(NOW).0.session.unwrap().tokens.output, 10);

        // A session started after the daemon takes over on the next poll.
        write_transcript(&new, 7, t0 + Duration::from_secs(60));
        let session = d.cycle(NOW).0.session.unwrap();
        assert_eq!(session.tokens.output, 7);
        assert!(d.tailer.as_ref().map(session::Tailer::path) == Some(new.as_path()));

        std::fs::remove_dir_all(dir).ok();
    }

    fn bar_with_effect(effect: render::Effect) -> render::BarOutput {
        render::BarOutput {
            text: String::new(),
            color: render::DEFAULT_COLOR,
            effect,
        }
    }

    #[test]
    fn no_events_means_no_pulse() {
        let dir = scratch("pulse-none");
        let d = offline_daemon(&dir, None);
        assert_eq!(
            d.pulse_seconds(&[], &bar_with_effect(render::Effect::Pulse)),
            0
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_window_reset_with_pulse_configured_animates_for_its_duration() {
        let dir = scratch("pulse-reset");
        let mut d = offline_daemon(&dir, None);
        d.cfg.reset.effect = "pulse".into();
        d.cfg.reset.duration = 3;
        let events = [events::Event::WindowReset {
            metric: events::Metric::Weekly,
        }];
        assert_eq!(
            d.pulse_seconds(&events, &bar_with_effect(render::Effect::None)),
            3
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_window_reset_with_effect_none_does_not_animate() {
        let dir = scratch("pulse-reset-off");
        let mut d = offline_daemon(&dir, None);
        d.cfg.reset.effect = "none".into();
        let events = [events::Event::WindowReset {
            metric: events::Metric::Weekly,
        }];
        assert_eq!(
            d.pulse_seconds(&events, &bar_with_effect(render::Effect::None)),
            0
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_window_reset_with_zero_duration_does_not_animate() {
        let dir = scratch("pulse-reset-zero");
        let mut d = offline_daemon(&dir, None);
        d.cfg.reset.duration = 0;
        let events = [events::Event::WindowReset {
            metric: events::Metric::Weekly,
        }];
        assert_eq!(
            d.pulse_seconds(&events, &bar_with_effect(render::Effect::None)),
            0
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_threshold_crossing_animates_only_when_the_matched_rule_pulses() {
        let dir = scratch("pulse-threshold");
        let d = offline_daemon(&dir, None);
        let events = [events::Event::ThresholdCrossed {
            metric: events::Metric::Weekly,
            level: 90,
        }];
        assert_eq!(
            d.pulse_seconds(&events, &bar_with_effect(render::Effect::Pulse)),
            THRESHOLD_PULSE_SECONDS
        );
        assert_eq!(
            d.pulse_seconds(&events, &bar_with_effect(render::Effect::None)),
            0
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// A same-process stand-in for the tray handle, so `animate`'s loop can
    /// be tested without a D-Bus connection or a display. Every `push`
    /// mutates a scratch `Indicator` and records the resulting `pulse`
    /// field, so the log shows exactly what the real tray would have been
    /// told, in order.
    struct FakeSink {
        calls: std::cell::RefCell<Vec<Option<(usize, usize)>>>,
        /// If `Some(n)`, `push` reports "tray gone" starting from the nth
        /// call (0-indexed), so the early-abort path is reachable too.
        gone_from: Option<usize>,
    }

    impl FakeSink {
        fn new() -> Self {
            FakeSink {
                calls: std::cell::RefCell::new(Vec::new()),
                gone_from: None,
            }
        }
    }

    impl FramePusher for FakeSink {
        fn push(&self, f: impl FnOnce(&mut tray::Indicator)) -> bool {
            let mut i = tray::Indicator::new(
                bar_with_effect(render::Effect::None),
                Snapshot::default(),
                Arc::default(),
            );
            f(&mut i);
            let n = self.calls.borrow().len();
            self.calls.borrow_mut().push(i.pulse);
            self.gone_from != Some(n)
        }
    }

    #[test]
    fn animate_drives_every_frame_in_order_then_clears() {
        let sink = FakeSink::new();
        animate(&sink, 5);
        let calls = sink.calls.into_inner();
        assert_eq!(
            calls,
            vec![
                Some((0, 5)),
                Some((1, 5)),
                Some((2, 5)),
                Some((3, 5)),
                Some((4, 5)),
                None,
            ]
        );
    }

    #[test]
    fn animate_stops_pushing_once_the_tray_is_gone() {
        let sink = FakeSink {
            gone_from: Some(2),
            ..FakeSink::new()
        };
        animate(&sink, 10);
        // Calls 0 and 1 succeed; call 2 reports "gone" and animate returns
        // immediately — no further frames, and no trailing clear either.
        assert_eq!(
            sink.calls.into_inner(),
            vec![Some((0, 10)), Some((1, 10)), Some((2, 10))]
        );
    }

    #[test]
    fn a_session_that_went_quiet_drops_out_of_the_snapshot() {
        let dir = scratch("stale-session");
        let t = dir.join("projects/friday/a.jsonl");
        write_transcript(
            &t,
            10,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        );
        let mut d = offline_daemon(&dir, None);

        // Minutes after the last turn, it is the session on screen.
        assert!(d.cycle(NOW).0.session.is_some());

        // The next morning the same file is still the newest transcript on
        // disk, and it is still not a session anybody is in.
        assert!(d.cycle(datetime!(2026-09-16 09:00 UTC)).0.session.is_none());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_rate_limit_waits_as_long_as_the_server_asked() {
        let dir = scratch("retry-after");
        let mut d = offline_daemon(&dir, None);
        let mut snap = Snapshot::default();

        // One failure alone would mean a 60-second backoff. The server said
        // five minutes, and it is the one that knows.
        d.note_failure(
            &mut snap,
            quota::FetchError::RateLimited {
                retry_after: Some(300),
            },
            NOW,
        );
        assert_eq!(d.wait_seconds(), 300);
        // A rate limit is not an authentication problem; the last reading
        // stays on screen with its age attached.
        assert!(matches!(snap.health, Health::Stale { .. }));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_short_retry_after_never_shortens_a_long_backoff() {
        let dir = scratch("retry-floor");
        let mut d = offline_daemon(&dir, None);
        let mut snap = Snapshot::default();

        // Twelve straight failures put the backoff at its cap. A server
        // asking for 30 seconds must not undo that: we are already the
        // reason it is saying no.
        for _ in 0..12 {
            d.note_failure(
                &mut snap,
                quota::FetchError::RateLimited {
                    retry_after: Some(30),
                },
                NOW,
            );
        }
        assert_eq!(d.wait_seconds(), BACKOFF_CAP_SECONDS);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_servers_interval_does_not_outlive_the_rate_limit() {
        let dir = scratch("retry-clear");
        let mut d = offline_daemon(&dir, None);
        let mut snap = Snapshot::default();

        d.note_failure(
            &mut snap,
            quota::FetchError::RateLimited {
                retry_after: Some(600),
            },
            NOW,
        );
        assert_eq!(d.wait_seconds(), 600);

        // The next failure is an ordinary one. A stale 600 would keep pacing
        // the loop long after the 429 that justified it.
        d.note_failure(&mut snap, quota::FetchError::Network("down".into()), NOW);
        assert_eq!(d.wait_seconds(), 120);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_clock_that_steps_backwards_does_not_report_a_vast_age() {
        let dir = scratch("clockskew");
        let mut d = offline_daemon(&dir, None);
        d.last_ok = Some(NOW + time::Duration::hours(1));

        // Now that the age is absolute it can outrun the clock, which an
        // `Instant` never could. Unsigned subtraction here would render as
        // billions of seconds of staleness.
        assert_eq!(d.age_seconds(NOW), 0);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_restart_mid_outage_comes_back_with_the_last_reading() {
        let dir = scratch("restart");
        let cache = dir.join("quota.json");

        // A daemon that had a good reading four minutes before it died.
        let mut q = quota::Quota::default();
        q.session.get_or_insert_default().percent = 47.0;
        q.weekly.percent = 26.0;
        persist::save(&cache, &q, NOW - time::Duration::minutes(4));

        // Its replacement starts the way `main` starts it, and the boundary
        // is still broken.
        let (last_quota, last_ok) = persist::load(&cache, NOW).unzip();
        let mut d = offline_daemon(&dir, last_quota);
        d.last_ok = last_ok;

        let (mut snap, bar, _events) = d.cycle(NOW);

        // The reading crossed the restart and reached the snapshot, so the
        // menu is populated on the very first cycle.
        assert_eq!(snap.session_pct(), Some(47.0));
        assert_eq!(snap.weekly_pct(), 26.0);
        // Absent credentials are what this particular daemon has to report...
        assert_eq!(bar.text, "⚠ sem credencial");

        // ...but in the case the cache exists for — a restart while the
        // endpoint is rate limiting — the bar now has a number. Before this
        // was persisted, `quota` was `None` here and the bar read
        // "⚠ sem dados" however recently the daemon had known better.
        snap.health = Health::Stale { seconds: 240 };
        assert!(render::render(&d.cfg.bar, &snap, NOW).text.contains("47"));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_manual_refresh_cuts_the_wait_short_without_resetting_the_backoff() {
        let dir = scratch("manual");
        let mut d = offline_daemon(&dir, None);
        let mut snap = Snapshot::default();

        // Deep into a backoff: fifteen minutes until the loop would poll on
        // its own, which is the whole reason the menu item exists.
        for _ in 0..12 {
            d.note_failure(&mut snap, quota::FetchError::Network("down".into()), NOW);
        }
        assert_eq!(d.wait_seconds(), BACKOFF_CAP_SECONDS);

        d.wakeup.request();
        let t = Instant::now();
        d.wait(0);
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

        // The backoff itself is untouched. One click is one poll, not a
        // licence to hammer an endpoint that is already refusing.
        assert_eq!(d.wait_seconds(), BACKOFF_CAP_SECONDS);

        std::fs::remove_dir_all(dir).ok();
    }
}
