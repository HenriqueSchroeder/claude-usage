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
    projects_dir: PathBuf,
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
    /// up in `Snapshot::health`.
    fn cycle(&mut self) -> (Snapshot, render::BarOutput) {
        let mut snap = Snapshot::default();
        self.read_quota(&mut snap);
        // The last good reading outlives a failed poll; `health` is what says
        // how much to trust it.
        snap.quota = self.last_quota.clone();

        self.refresh_tailer();
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
        projects_dir: home().join(".claude/projects"),
        // Filled by the first cycle's `refresh_tailer`.
        tailer: None,
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
            // Nothing left to draw on. Exit non-zero on purpose: a clean exit
            // would leave `Restart=on-failure` satisfied and the user without
            // an indicator and without a line in the journal saying why.
            eprintln!("claude-usage: lost the tray connection, exiting to be restarted");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::SystemTime;

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
            last_note: None,
            failures: 0,
        }
    }

    #[test]
    fn a_failed_poll_keeps_the_last_reading_instead_of_zeroing_it() {
        let dir = scratch("carry");
        let mut q = quota::Quota::default();
        q.weekly.percent = 83.0;
        let mut d = offline_daemon(&dir, Some(q));

        let (snap, bar) = d.cycle();

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
        let (snap, bar) = offline_daemon(&dir, None).cycle();
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

        // At startup the newest transcript is yesterday's finished session.
        write_transcript(&old, 10, t0);
        let mut d = offline_daemon(&dir, None);
        assert_eq!(d.cycle().0.session.unwrap().tokens.output, 10);

        // An unchanged path must keep its tailer: rebuilding would re-read the
        // file from zero and double every total.
        assert_eq!(d.cycle().0.session.unwrap().tokens.output, 10);

        // A session started after the daemon takes over on the next poll.
        write_transcript(&new, 7, t0 + Duration::from_secs(60));
        let session = d.cycle().0.session.unwrap();
        assert_eq!(session.tokens.output, 7);
        assert!(d.tailer.as_ref().map(session::Tailer::path) == Some(new.as_path()));

        std::fs::remove_dir_all(dir).ok();
    }
}
