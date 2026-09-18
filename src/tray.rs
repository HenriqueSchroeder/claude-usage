use crate::icon::{pulse_frame, ring};
use crate::quota::Severity;
use crate::render::{human_duration, BarOutput, Effect, NO_VALUE};
use crate::state::{Health, Snapshot};
use ksni::{menu::StandardItem, Icon, MenuItem, OfflineReason, Status, Tray};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Lets the menu wake the poll loop.
///
/// The loop spends almost all of its life asleep, and a backoff now runs as
/// long as an hour when the server asks for it. Without this the only way to
/// force a reading is to restart the service.
///
/// `Condvar` rather than a flag polled on a timer: the point of this daemon
/// is to cost nothing while idle, and waking once a second to ask whether
/// anyone clicked would trade a permanent cost for a latency nobody notices.
///
/// The locks are unwrapped because the crate builds with `panic = "abort"`,
/// so a poisoned mutex cannot exist — a panic takes the process with it.
#[derive(Default)]
pub struct Wakeup {
    requested: Mutex<bool>,
    ready: Condvar,
}

impl Wakeup {
    pub fn request(&self) {
        *self.requested.lock().unwrap() = true;
        self.ready.notify_all();
    }

    /// Sleeps up to `timeout`, returning `true` if a refresh was asked for
    /// instead of the time simply running out.
    ///
    /// The request is consumed on the way out, so one click buys one poll;
    /// the loop does not come back round and immediately skip its next wait.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut requested = self.requested.lock().unwrap();
        // A loop, not a bare `wait_timeout`: a condvar may wake spuriously,
        // which would otherwise cut the poll interval short at random.
        while !*requested {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            requested = self.ready.wait_timeout(requested, left).unwrap().0;
        }
        *requested = false;
        true
    }
}

/// The panel item.
///
/// Everything it shows is derived from `snap` and `bar` on demand; it keeps
/// no percentages of its own, which is what stops the label and the menu
/// from ever disagreeing. `pulse` is the one transient exception: it
/// overrides the icon for a single animation frame and is always cleared
/// back to `None` when the animation ends.
pub struct Indicator {
    pub bar: BarOutput,
    pub snap: Snapshot,
    /// Set by the main loop while a pulse animation is playing:
    /// `Some((frame, count))` overrides the drawn icon with that one frame.
    /// `None` is the steady state — both the resting condition and what an
    /// animation is cleared back to when it ends — and draws exactly the
    /// ring `bar`/`snap` already imply.
    pub pulse: Option<(usize, usize)>,
    /// Whether the missing-host diagnostic has already been printed. `ksni`
    /// hands `watcher_offline` a `&self`, and the alternative to an atomic
    /// here is one identical line in the journal per reconnection attempt.
    offline_reported: AtomicBool,
    /// Shared with the poll loop, which is asleep whenever a menu item can
    /// be clicked.
    wakeup: Arc<Wakeup>,
}

impl Indicator {
    pub fn new(bar: BarOutput, snap: Snapshot, wakeup: Arc<Wakeup>) -> Self {
        Indicator {
            bar,
            snap,
            pulse: None,
            offline_reported: AtomicBool::new(false),
            wakeup,
        }
    }
}

/// Normal and HiDPI. Hosts pick whichever matches the panel scale.
const ICON_SIZES: [i32; 2] = [22, 44];

fn info(label: String) -> MenuItem<Indicator> {
    StandardItem {
        label,
        enabled: false,
        ..Default::default()
    }
    .into()
}

/// One quota row: `label  ·  percent%  ·  time`, e.g. `5h  ·  2%  ·  4h51m`.
///
/// `time` is what gives `{reset}` on the panel something to be checked
/// against: the bar shows the nearest of the two windows, and only here can
/// the user see which one that was. Relative, not a wall-clock time —
/// `time` (the crate) cannot read the local UTC offset from a
/// multi-threaded process, and an hour-wrong wall-clock reset would be
/// worse than no clock at all. `—` (the project's usual placeholder, see
/// `render::NO_VALUE`) stands in when the endpoint never said, same as
/// everywhere else on the panel.
fn quota_line(
    label: &str,
    percent: f32,
    remaining: Option<time::Duration>,
    severity: Severity,
) -> String {
    let time = remaining.map_or_else(|| NO_VALUE.into(), human_duration);
    format!(
        "{label}  ·  {percent:.0}%  ·  {time}{}",
        severity_note(severity)
    )
}

/// The endpoint's own verdict on a window, shown beside our percentage.
fn severity_note(severity: Severity) -> &'static str {
    match severity {
        Severity::Normal => "",
        Severity::Warning => "  (atenção)",
        Severity::Critical => "  (crítico)",
    }
}

fn health_line(health: &Health) -> Option<String> {
    match health {
        Health::Ok => None,
        Health::Stale { seconds } if *seconds < 60 => Some("Dados de menos de 1 min atrás".into()),
        Health::Stale { seconds } => Some(format!("Dados de {} min atrás", seconds / 60)),
        Health::Unauthorized => Some("Sem autorização — abra o Claude Code".into()),
        Health::NoCredentials => Some("Credenciais não encontradas".into()),
    }
}

impl Indicator {
    /// How much of the ring to fill.
    ///
    /// `Stale` still draws the real arc — the number is the last good one and
    /// the label says how old it is. The auth states do not: the label there
    /// reads `⚠ reautenticar`, and an arc drawn at a percentage from before
    /// the token died would contradict it with a plausible-looking lie.
    fn arc_percent(&self) -> f32 {
        match self.snap.health {
            Health::Ok | Health::Stale { .. } => self.snap.weekly_pct(),
            Health::Unauthorized | Health::NoCredentials => 0.0,
        }
    }

    fn pixmaps(&self) -> Vec<Icon> {
        let percent = self.arc_percent();
        ICON_SIZES
            .iter()
            .map(|&s| Icon {
                width: s,
                height: s,
                data: match self.pulse {
                    Some((i, count)) => pulse_frame(percent, self.bar.color, s as u32, i, count),
                    None => ring(percent, self.bar.color, s as u32),
                },
            })
            .collect()
    }
}

impl Tray for Indicator {
    fn id(&self) -> String {
        "claude-usage".into()
    }

    fn title(&self) -> String {
        "claude-usage".into()
    }

    /// Exported as `XAyatanaLabel`: the text the panel draws next to the
    /// icon. Plain StatusNotifierItem hosts ignore it.
    fn label(&self) -> String {
        self.bar.text.clone()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.pixmaps()
    }

    /// Hosts switch to this one while the status is `NeedsAttention`; without
    /// it they would have no icon to draw at exactly the moment the item is
    /// meant to stand out.
    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        self.pixmaps()
    }

    /// A `pulse` rule in the config asks the panel to emphasize the item.
    /// StatusNotifier says that with a status, not with an animation.
    fn status(&self) -> Status {
        match self.bar.effect {
            Effect::Pulse => Status::NeedsAttention,
            Effect::None => Status::Active,
        }
    }

    /// No `org.kde.StatusNotifierWatcher` on the bus.
    ///
    /// `assume_sni_available(true)` turns this from a spawn error into a
    /// silent wait, which is right — the host may well show up later — but
    /// left someone on stock GNOME with `active (running)`, an empty panel
    /// and an empty journal, and no way to find out why. Returning `true`
    /// keeps waiting; the line says what to look at meanwhile.
    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        if !self.offline_reported.swap(true, Ordering::Relaxed) {
            eprintln!(
                "claude-usage: no StatusNotifier host on the session bus ({reason:?}). \
                 On GNOME this usually means the AppIndicator extension is not enabled. \
                 The daemon keeps running and the icon will appear when a host does."
            );
        }
        true
    }

    fn watcher_online(&self) {
        if self.offline_reported.swap(false, Ordering::Relaxed) {
            eprintln!("claude-usage: StatusNotifier host is back, registering the icon");
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();

        if let Some(line) = health_line(&self.snap.health) {
            items.push(info(line));
            items.push(MenuItem::Separator);
        }

        match self.snap.quota.as_ref() {
            None => items.push(info("Sem dados de quota".into())),
            Some(q) => {
                // Read at the moment the menu opens, not at the last poll:
                // the countdown shown is the countdown as of now.
                let now = OffsetDateTime::now_utc();
                items.push(info(quota_line(
                    "5h",
                    self.snap.session_pct(),
                    self.snap.session_reset_in(now),
                    q.session.severity,
                )));
                items.push(info(quota_line(
                    "Semanal",
                    self.snap.weekly_pct(),
                    self.snap.weekly_reset_in(now),
                    q.weekly.severity,
                )));
                for row in &q.breakdown {
                    if row.percent > 0.0 {
                        items.push(info(format!("  {}  {:.0}%", row.display_name, row.percent)));
                    }
                }
            }
        }

        if let Some(s) = self.snap.session.as_ref() {
            items.push(MenuItem::Separator);
            let age = s
                .elapsed()
                .map(|d| format!(" · {}", human_duration(d)))
                .unwrap_or_default();
            items.push(info(format!("Sessão ativa   {}{age}", s.project)));
            items.push(info(format!(
                "Tokens         {} in · {} out · {} cache",
                s.tokens.input, s.tokens.output, s.tokens.cache_read
            )));
            items.push(info(match self.snap.cost_usd {
                Some(c) => format!("Custo est.     US$ {c:.2}  (teórico)"),
                // A model with no price beats printing US$ 0,00 beside
                // millions of tokens and calling it an estimate.
                None => "Custo est.     —  (modelo sem preço conhecido)".into(),
            }));
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Atualizar agora".into(),
                activate: Box::new(|t: &mut Indicator| t.wakeup.request()),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Sair".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::DEFAULT_COLOR;

    fn indicator() -> Indicator {
        Indicator::new(
            BarOutput {
                text: String::new(),
                color: DEFAULT_COLOR,
                effect: Effect::None,
            },
            Snapshot::default(),
            Arc::default(),
        )
    }

    #[test]
    fn the_missing_host_warning_is_printed_once_per_outage() {
        let t = indicator();
        assert!(t.watcher_offline(OfflineReason::No));
        assert!(t.offline_reported.load(Ordering::Relaxed));

        // ksni retries; the journal must not get one line per retry.
        assert!(t.watcher_offline(OfflineReason::No));
        assert!(t.offline_reported.load(Ordering::Relaxed));

        // A host appearing re-arms it, so a later outage is reported again.
        t.watcher_online();
        assert!(!t.offline_reported.load(Ordering::Relaxed));
    }

    #[test]
    fn the_menu_does_not_call_a_fresh_outage_zero_minutes_old() {
        assert_eq!(
            health_line(&Health::Stale { seconds: 12 }).unwrap(),
            "Dados de menos de 1 min atrás"
        );
        assert_eq!(
            health_line(&Health::Stale { seconds: 180 }).unwrap(),
            "Dados de 3 min atrás"
        );
    }

    #[test]
    fn a_quota_line_is_label_percent_dot_time() {
        assert_eq!(
            quota_line(
                "5h",
                2.0,
                Some(time::Duration::minutes(291)),
                Severity::Normal
            ),
            "5h  ·  2%  ·  4h51m"
        );
        assert_eq!(
            quota_line(
                "Semanal",
                26.0,
                Some(time::Duration::minutes(8400)),
                Severity::Normal
            ),
            "Semanal  ·  26%  ·  5d20h"
        );
    }

    #[test]
    fn a_quota_line_shows_a_dash_instead_of_a_zero_when_there_is_no_reset_time() {
        assert_eq!(
            quota_line("5h", 2.0, None, Severity::Normal),
            "5h  ·  2%  ·  —"
        );
    }

    #[test]
    fn a_quota_line_still_appends_the_severity_note() {
        assert_eq!(
            quota_line("Semanal", 96.0, None, Severity::Critical),
            "Semanal  ·  96%  ·  —  (crítico)"
        );
    }

    #[test]
    fn a_pulse_frame_overrides_the_icon_and_clearing_it_restores_the_steady_state() {
        let mut t = indicator();
        let steady: Vec<Vec<u8>> = t.icon_pixmap().into_iter().map(|i| i.data).collect();

        t.pulse = Some((0, 10));
        let mid: Vec<Vec<u8>> = t.icon_pixmap().into_iter().map(|i| i.data).collect();
        assert_ne!(mid, steady, "a pulse frame must not look identical to rest");

        t.pulse = None;
        let restored: Vec<Vec<u8>> = t.icon_pixmap().into_iter().map(|i| i.data).collect();
        assert_eq!(
            restored, steady,
            "clearing the pulse must return exactly the state bar/snap imply"
        );
    }

    #[test]
    fn the_menu_renders_the_compact_quota_lines_end_to_end() {
        let mut t = indicator();
        let mut q = crate::quota::Quota::default();
        q.session.percent = 2.0;
        q.weekly.percent = 26.0;
        // Session clears in ~4h51m; weekly carries no resets_at at all,
        // which must render a dash rather than a fabricated time.
        q.session.resets_at = Some(OffsetDateTime::now_utc() + time::Duration::minutes(291));
        t.snap.quota = Some(q);

        let labels: Vec<String> = t
            .menu()
            .iter()
            .filter_map(|i| match i {
                MenuItem::Standard(s) => Some(s.label.clone()),
                _ => None,
            })
            .collect();

        // Not checking the exact minute: the countdown is read against
        // `OffsetDateTime::now_utc()` inside `menu()`, a moment after this
        // test computed `resets_at`, so the rendered minute can be one off.
        assert!(labels.iter().any(|l| l.starts_with("5h  ·  2%  ·  4h")));
        assert!(labels.iter().any(|l| l == "Semanal  ·  26%  ·  —"));
        assert!(!labels
            .iter()
            .any(|l| l.contains("reset") || l.contains("Sessão")));
    }

    #[test]
    fn an_unpriced_session_shows_a_dash_instead_of_zero() {
        let mut t = indicator();
        t.snap.session = Some(crate::session::SessionInfo::default());
        t.snap.quota = Some(crate::quota::Quota::default());

        let labels = |t: &Indicator| {
            t.menu()
                .iter()
                .filter_map(|i| match i {
                    MenuItem::Standard(s) => Some(s.label.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        assert!(labels(&t)
            .iter()
            .any(|l| l.starts_with("Custo est.") && l.contains('—') && !l.contains("0.00")));

        t.snap.cost_usd = Some(3.42);
        assert!(labels(&t)
            .iter()
            .any(|l| l == "Custo est.     US$ 3.42  (teórico)"));
    }

    #[test]
    fn a_wait_that_nobody_interrupts_runs_out() {
        let w = Wakeup::default();
        let t = Instant::now();
        assert!(!w.wait(Duration::from_millis(50)));
        assert!(t.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn one_click_buys_exactly_one_poll() {
        let w = Wakeup::default();
        w.request();
        // The click cuts this wait short...
        assert!(w.wait(Duration::from_secs(30)));
        // ...and is spent doing it. A request that survived would collapse
        // every interval after it, turning one click into a poll loop with
        // no delay at all.
        assert!(!w.wait(Duration::from_millis(50)));
    }

    #[test]
    fn a_click_from_another_thread_cuts_a_long_wait_short() {
        let w = Arc::new(Wakeup::default());
        let clicker = Arc::clone(&w);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            clicker.request();
        });

        // ksni runs the menu on its own thread, so this is the real shape of
        // a click: the loop is already blocked when it arrives.
        let t = Instant::now();
        assert!(w.wait(Duration::from_secs(30)));
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn the_menu_offers_a_manual_refresh_that_wakes_the_loop() {
        let mut i = indicator();
        let wakeup = Arc::clone(&i.wakeup);

        let activate = i
            .menu()
            .into_iter()
            .find_map(|m| match m {
                MenuItem::Standard(s) if s.label == "Atualizar agora" => Some(s.activate),
                _ => None,
            })
            .expect("the menu offers a manual refresh");
        activate(&mut i);

        assert!(wakeup.wait(Duration::from_millis(0)));
    }
}
