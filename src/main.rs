mod config;
mod creds;
mod diag;
mod events;
mod icon;
mod notify;
mod pricing;
mod quota;
mod render;
mod session;
mod state;
mod tray;

use ksni::blocking::TrayMethods;
use state::{Health, Snapshot};
use std::path::PathBuf;
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
    /// Last successful reading, kept so a failed poll shows stale truth
    /// instead of a fresh-looking zero.
    last_quota: Option<quota::Quota>,
    last_ok: Option<Instant>,
    notes: diag::Notes,
    failures: u32,
}

impl Daemon {
    fn read_quota(&mut self, snap: &mut Snapshot) {
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
            Ok(q) => {
                snap.health = Health::Ok;
                self.last_quota = Some(q);
                self.last_ok = Some(Instant::now());
                self.failures = 0;
                self.notes.clear();
            }
            Err(e) => self.note_failure(snap, e),
        }
    }

    /// Records a failed poll. Counting happens here, once, for every variant:
    /// an expired refresh token used to be exempt, which meant one 401 per
    /// `poll_seconds`, forever, from an identifiable User-Agent against an
    /// endpoint that is not public. Backing off is the whole point.
    fn note_failure(&mut self, snap: &mut Snapshot, e: quota::FetchError) {
        self.failures = self.failures.saturating_add(1);
        let stale = Health::Stale {
            seconds: self.last_ok.map_or(0, |t| t.elapsed().as_secs()),
        };
        match e {
            quota::FetchError::Unauthorized => {
                snap.health = Health::Unauthorized;
                self.notes
                    .say("token rejected by the usage endpoint".into());
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
        self.read_quota(&mut snap);
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
        match self.failures {
            0 => base,
            n => backoff_seconds(n - 1, base),
        }
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
    /// While healthy this is a plain sleep. While backing off it also watches
    /// the credentials file: Claude Code rewrites it when the user
    /// re-authenticates, and making them sit out a 15-minute backoff after
    /// they have already fixed the problem is its own kind of broken.
    fn wait(&mut self, pulse_debt: u64) {
        let seconds = self.wait_seconds().saturating_sub(pulse_debt);
        if self.failures == 0 {
            std::thread::sleep(Duration::from_secs(seconds));
            return;
        }

        let before = self.creds_mtime();
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(CREDS_CHECK_INTERVAL.min(left));
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

    let mut daemon = Daemon {
        agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
        creds_path: home().join(".claude/.credentials.json"),
        detector: events::Detector::load(&state_path, cfg.threshold.clone()),
        notifier: notify::Notifier::new(),
        projects_dir: home().join(".claude/projects"),
        // Filled by the first cycle's `refresh_tailer`.
        tailer: None,
        state_path,
        cfg,
        last_quota: None,
        last_ok: None,
        notes: diag::Notes::default(),
        failures: 0,
    };

    // First reading before the item appears, so the panel never shows a
    // placeholder percentage. The first cycle never yields events (the
    // detector is silent on its first diff), so there is nothing to animate.
    let (snap, bar, _events) = daemon.cycle(OffsetDateTime::now_utc());

    let handle = match tray::Indicator::new(bar, snap)
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
            projects_dir: dir.join("projects"),
            tailer: None,
            detector: events::Detector::new(config::ThresholdConfig::default()),
            notifier: None,
            last_quota,
            last_ok: None,
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
        d.note_failure(&mut snap, quota::FetchError::Unauthorized);
        assert_eq!(snap.health, Health::Unauthorized);
        assert_eq!(d.wait_seconds(), 60);
        d.note_failure(&mut snap, quota::FetchError::Unauthorized);
        assert_eq!(d.wait_seconds(), 120);
        for _ in 0..10 {
            d.note_failure(&mut snap, quota::FetchError::Unauthorized);
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
            let mut i =
                tray::Indicator::new(bar_with_effect(render::Effect::None), Snapshot::default());
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
}
