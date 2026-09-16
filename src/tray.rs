use crate::icon::ring;
use crate::quota::Severity;
use crate::render::{human_duration, BarOutput, Effect};
use crate::state::{Health, Snapshot};
use ksni::{menu::StandardItem, Icon, MenuItem, OfflineReason, Status, Tray};
use std::sync::atomic::{AtomicBool, Ordering};
use time::OffsetDateTime;

/// The panel item.
///
/// Everything it shows is derived from `snap` and `bar` on demand; it keeps
/// no percentages of its own, which is what stops the label and the menu
/// from ever disagreeing.
pub struct Indicator {
    pub bar: BarOutput,
    pub snap: Snapshot,
    /// Whether the missing-host diagnostic has already been printed. `ksni`
    /// hands `watcher_offline` a `&self`, and the alternative to an atomic
    /// here is one identical line in the journal per reconnection attempt.
    offline_reported: AtomicBool,
}

impl Indicator {
    pub fn new(bar: BarOutput, snap: Snapshot) -> Self {
        Indicator {
            bar,
            snap,
            offline_reported: AtomicBool::new(false),
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

/// When a window clears, shown beside its percentage.
///
/// This is what gives `{reset}` on the panel something to be checked
/// against: the bar shows the nearest of the two windows, and only here can
/// the user see which one that was. Relative, not a wall-clock time —
/// `time` cannot read the local UTC offset from a multi-threaded process,
/// and an hour-wrong "reset 12:40" would be worse than no clock at all.
fn reset_note(remaining: Option<time::Duration>) -> String {
    remaining.map_or_else(String::new, |d| format!("  reset em {}", human_duration(d)))
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
                data: ring(percent, self.bar.color, s as u32),
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
                items.push(info(format!(
                    "Sessão 5h      {:.0}%{}{}",
                    self.snap.session_pct(),
                    reset_note(self.snap.session_reset_in(now)),
                    severity_note(q.session.severity)
                )));
                items.push(info(format!(
                    "Semanal        {:.0}%{}{}",
                    self.snap.weekly_pct(),
                    reset_note(self.snap.weekly_reset_in(now)),
                    severity_note(q.weekly.severity)
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
    fn a_reset_note_is_empty_when_the_window_has_no_reset_time() {
        assert_eq!(reset_note(None), "");
        assert_eq!(
            reset_note(Some(time::Duration::minutes(130))),
            "  reset em 2h10"
        );
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
}
