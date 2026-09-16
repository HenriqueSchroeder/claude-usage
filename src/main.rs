mod config;
mod creds;
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
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

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
    tailer: Option<session::Tailer>,
    detector: events::Detector,
    notifier: Option<notify::Notifier>,
    /// Last successful reading, kept so a failed poll shows stale truth
    /// instead of a fresh-looking zero.
    last_quota: Option<quota::Quota>,
    last_ok: Option<Instant>,
    last_note: Option<String>,
    failures: u32,
}

impl Daemon {
    /// Prints a diagnostic only when it differs from the previous one, so a
    /// permanently broken setup does not write one identical line per poll.
    fn note(&mut self, msg: String) {
        if self.last_note.as_deref() != Some(msg.as_str()) {
            eprintln!("claude-usage: {msg}");
            self.last_note = Some(msg);
        }
    }

    fn read_quota(&mut self, snap: &mut Snapshot) {
        let token = match creds::read_access_token(&self.creds_path) {
            Ok(t) => t,
            Err(e) => {
                snap.health = Health::NoCredentials;
                self.note(match e {
                    creds::CredsError::Missing => "credentials file not found".into(),
                    creds::CredsError::Unreadable(m) => format!("credentials unreadable: {m}"),
                    creds::CredsError::Malformed(m) => format!("credentials malformed: {m}"),
                });
                return;
            }
        };

        let stale = |last_ok: Option<Instant>| Health::Stale {
            seconds: last_ok.map_or(0, |t| t.elapsed().as_secs()),
        };

        match quota::fetch(&self.agent, &token) {
            Ok(q) => {
                snap.health = Health::Ok;
                self.last_quota = Some(q);
                self.last_ok = Some(Instant::now());
                self.failures = 0;
                self.last_note = None;
            }
            Err(quota::FetchError::Unauthorized) => {
                snap.health = Health::Unauthorized;
                self.note("token rejected by the usage endpoint".into());
            }
            Err(quota::FetchError::Network(m)) => {
                snap.health = stale(self.last_ok);
                self.failures = self.failures.saturating_add(1);
                self.note(format!("network: {m}"));
            }
            Err(quota::FetchError::Parse(m)) => {
                snap.health = stale(self.last_ok);
                self.failures = self.failures.saturating_add(1);
                self.note(format!("unreadable response: {m}"));
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

    /// One poll: read the quota, fold in the active session, notify on what
    /// changed, and render the bar. Never fails; every problem it meets ends
    /// up in `Snapshot::health`.
    fn cycle(&mut self) -> (Snapshot, render::BarOutput) {
        let mut snap = Snapshot::default();
        self.read_quota(&mut snap);
        snap.quota = self.last_quota.clone();

        if let Some(t) = self.tailer.as_mut() {
            let info = t.poll().clone();
            snap.cost_usd = pricing::cost_usd(&info.model, &info.tokens);
            snap.session = Some(info);
        }

        let events = self.detector.diff(&snap);
        for event in &events {
            self.announce(event);
        }
        self.detector.save(&self.state_path);

        let bar = render::render(&self.cfg.bar, &snap, OffsetDateTime::now_utc());
        (snap, bar)
    }

    fn wait_seconds(&self) -> u64 {
        let base = self.cfg.poll_seconds.max(MIN_POLL_SECONDS);
        match self.failures {
            0 => base,
            n => backoff_seconds(n - 1, base),
        }
    }
}

fn main() {
    let cfg = config::load(&home().join(".config/claude-usage/config.toml"));
    let state_path = home().join(".local/state/claude-usage/state.json");

    let mut daemon = Daemon {
        agent: ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build(),
        creds_path: home().join(".claude/.credentials.json"),
        detector: events::Detector::load(&state_path, cfg.threshold.clone()),
        notifier: notify::Notifier::new(),
        // Discovered once: rescanning every cycle would walk thousands of
        // transcript files a minute. An inotify watch replaces this later.
        tailer: session::discover_active(&home().join(".claude/projects"))
            .map(session::Tailer::new),
        state_path,
        cfg,
        last_quota: None,
        last_ok: None,
        last_note: None,
        failures: 0,
    };

    // First reading before the item appears, so the panel never shows a
    // placeholder percentage.
    let (snap, bar) = daemon.cycle();

    let handle = match (tray::Indicator { bar, snap })
        .assume_sni_available(true)
        .spawn()
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("claude-usage: could not register the tray item: {e}");
            return;
        }
    };

    loop {
        std::thread::sleep(Duration::from_secs(daemon.wait_seconds()));
        let (snap, bar) = daemon.cycle();
        let updated = handle.update(move |t: &mut tray::Indicator| {
            t.bar = bar;
            t.snap = snap;
        });
        if updated.is_none() {
            return; // tray service gone; there is nothing left to draw on
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps_at_fifteen_minutes() {
        assert_eq!(backoff_seconds(0, 60), 60);
        assert_eq!(backoff_seconds(1, 60), 120);
        assert_eq!(backoff_seconds(2, 60), 240);
        assert_eq!(backoff_seconds(10, 60), 900);
    }
}
