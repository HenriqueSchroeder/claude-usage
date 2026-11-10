use crate::events::{Event, Metric};
use std::collections::HashMap;
use zbus::blocking::Connection;
use zbus::zvariant::Value;

pub fn message_for(event: &Event) -> (String, String, bool) {
    match event {
        Event::ThresholdCrossed {
            metric: Metric::Weekly,
            level,
        } => (
            format!("Quota semanal em {level}%"),
            "O limite semanal está perto do fim.".into(),
            *level >= 90,
        ),
        Event::ThresholdCrossed {
            metric: Metric::Session,
            level,
        } => (
            format!("Janela de 5h em {level}%"),
            "O bloco atual está perto do fim.".into(),
            false,
        ),
        Event::WindowReset {
            metric: Metric::Session,
        } => (
            "Janela de 5h resetada".into(),
            "A quota do bloco voltou ao início.".into(),
            false,
        ),
        Event::WindowReset {
            metric: Metric::Weekly,
        } => (
            "Quota semanal resetada".into(),
            "O limite semanal voltou ao início.".into(),
            false,
        ),
        Event::Unauthorized => (
            "Sem autorização".into(),
            "Abra o Claude Code uma vez para renovar o acesso.".into(),
            true,
        ),
    }
}

pub struct Notifier {
    conn: Connection,
    last_id: u32,
    last_critical: bool,
}

impl Notifier {
    pub fn new() -> Option<Self> {
        Some(Notifier {
            conn: Connection::session().ok()?,
            last_id: 0,
            last_critical: false,
        })
    }

    /// Replaces the previous notification instead of stacking banners —
    /// unless that one was critical. A critical banner stays up until
    /// dismissed, and a later routine one taking its place would turn
    /// "weekly at 95%" into a five-second toast about the session.
    pub fn send(&mut self, summary: &str, body: &str, critical: bool) {
        let replaces = if self.last_critical { 0 } else { self.last_id };
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("urgency", Value::U8(if critical { 2 } else { 1 }));

        let reply = self.conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "claude-usage",
                replaces,
                "utilities-system-monitor-symbolic",
                summary,
                body,
                Vec::<&str>::new(),
                hints,
                if critical { 0i32 } else { 5000i32 },
            ),
        );

        if let Ok(r) = reply {
            if let Ok(id) = r.body().deserialize::<u32>() {
                self.last_id = id;
                self.last_critical = critical;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, Metric};

    #[test]
    fn weekly_threshold_message_is_critical_and_in_portuguese() {
        let (summary, body, critical) = message_for(&Event::ThresholdCrossed {
            metric: Metric::Weekly,
            level: 95,
        });
        assert!(summary.contains("95%"));
        assert!(body.contains("semanal"));
        assert!(critical);
    }

    #[test]
    fn session_threshold_is_not_critical() {
        let (_, _, critical) = message_for(&Event::ThresholdCrossed {
            metric: Metric::Session,
            level: 75,
        });
        assert!(!critical);
    }

    #[test]
    fn reset_message_mentions_the_window() {
        let (summary, _, _) = message_for(&Event::WindowReset {
            metric: Metric::Session,
        });
        assert!(summary.contains("5h"));
    }
}
