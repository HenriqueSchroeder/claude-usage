// Task 14 will wire this module into main.rs; until then nothing outside
// the tests calls it, so allow the dead-code lint the same way state.rs does.
#![allow(dead_code)]

use crate::config::{BarConfig, Rule};
use crate::state::{Health, Snapshot};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const DEFAULT_COLOR: Rgb = Rgb(0x87, 0xb1, 0x5b);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Pulse,
}

#[derive(Debug, Clone)]
pub struct BarOutput {
    pub text: String,
    pub color: Rgb,
    pub effect: Effect,
}

pub fn parse_color(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 {
        return None;
    }
    Some(Rgb(
        u8::from_str_radix(&h[0..2], 16).ok()?,
        u8::from_str_radix(&h[2..4], 16).ok()?,
        u8::from_str_radix(&h[4..6], 16).ok()?,
    ))
}

fn metric_value(snap: &Snapshot, metric: &str) -> Option<f32> {
    match metric {
        "session" => Some(snap.session_pct()),
        "weekly" => Some(snap.weekly_pct()),
        "cost" => Some(snap.cost_usd as f32),
        _ => None,
    }
}

fn human_duration(d: time::Duration) -> String {
    let total = d.whole_minutes().max(0);
    if total >= 60 {
        format!("{}h{:02}", total / 60, total % 60)
    } else {
        format!("{total}m")
    }
}

fn expand(template: &str, snap: &Snapshot, now: OffsetDateTime) -> String {
    let reset = snap
        .reset_in(now)
        .map(human_duration)
        .unwrap_or_else(|| "—".into());
    let session = snap.session.as_ref();

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}').map(|i| start + i) else {
            break;
        };
        let key = &rest[start + 1..end];
        let value = match key {
            "session" => Some(format!("{:.0}", snap.session_pct())),
            "weekly" => Some(format!("{:.0}", snap.weekly_pct())),
            "reset" => Some(reset.clone()),
            "cost" => Some(format!("{:.2}", snap.cost_usd)),
            "project" => session.map(|s| s.project.clone()),
            "model" => session.map(|s| s.model.clone()),
            "tokens" => session.map(|s| format!("{}", s.tokens.output)),
            _ => None,
        };
        match value {
            // An unknown placeholder stays literal so a typo in the config is
            // visible on the panel instead of silently vanishing.
            Some(v) => out.push_str(&v),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn first_match<'a>(rules: &'a [Rule], snap: &Snapshot) -> Option<&'a Rule> {
    rules
        .iter()
        .find(|r| metric_value(snap, &r.metric).is_some_and(|v| v >= r.above))
}

pub fn render(cfg: &BarConfig, snap: &Snapshot, now: OffsetDateTime) -> BarOutput {
    match snap.health {
        Health::Unauthorized => {
            return BarOutput {
                text: "⚠ reautenticar".into(),
                color: Rgb(0xe0, 0x1b, 0x24),
                effect: Effect::None,
            }
        }
        Health::NoCredentials => {
            return BarOutput {
                text: "⚠ sem credencial".into(),
                color: Rgb(0xe0, 0x1b, 0x24),
                effect: Effect::None,
            }
        }
        _ => {}
    }

    let matched = first_match(&cfg.rules, snap);
    let template = matched
        .and_then(|r| r.format.as_deref())
        .unwrap_or(&cfg.format);

    let mut text = expand(template, snap, now);
    if let Health::Stale { seconds } = snap.health {
        text.push_str(&format!(" · {}m", seconds / 60));
    }

    BarOutput {
        text,
        color: matched
            .and_then(|r| r.color.as_deref())
            .and_then(parse_color)
            .unwrap_or(DEFAULT_COLOR),
        effect: match matched.and_then(|r| r.effect.as_deref()) {
            Some("pulse") => Effect::Pulse,
            _ => Effect::None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BarConfig, Rule};
    use crate::quota::Quota;
    use crate::state::Snapshot;
    use time::macros::datetime;

    const NOW: time::OffsetDateTime = datetime!(2026-09-15 14:40 UTC);

    fn snap(session: f32, weekly: f32) -> Snapshot {
        let mut q = Quota::default();
        q.session.percent = session;
        q.weekly.percent = weekly;
        q.session.resets_at = Some(datetime!(2026-09-15 15:40 UTC));
        Snapshot {
            quota: Some(q),
            ..Default::default()
        }
    }

    fn rule(metric: &str, above: f32, color: &str) -> Rule {
        Rule {
            metric: metric.into(),
            above,
            format: None,
            color: Some(color.into()),
            effect: None,
        }
    }

    #[test]
    fn substitutes_placeholders() {
        let cfg = BarConfig {
            format: "{session}% · {weekly}%".into(),
            rules: vec![],
        };
        assert_eq!(render(&cfg, &snap(24.0, 98.0), NOW).text, "24% · 98%");
    }

    #[test]
    fn renders_reset_as_human_duration() {
        let cfg = BarConfig {
            format: "{reset}".into(),
            rules: vec![],
        };
        assert_eq!(render(&cfg, &snap(24.0, 98.0), NOW).text, "1h00");
    }

    #[test]
    fn unknown_placeholder_is_left_literal() {
        let cfg = BarConfig {
            format: "{nope}".into(),
            rules: vec![],
        };
        assert_eq!(render(&cfg, &snap(1.0, 1.0), NOW).text, "{nope}");
    }

    #[test]
    fn first_matching_rule_wins() {
        let cfg = BarConfig {
            format: "{weekly}%".into(),
            rules: vec![
                rule("weekly", 95.0, "#e01b24"),
                rule("weekly", 75.0, "#e5a50a"),
            ],
        };
        assert_eq!(
            render(&cfg, &snap(0.0, 98.0), NOW).color,
            Rgb(0xe0, 0x1b, 0x24)
        );
    }

    #[test]
    fn lower_rule_applies_when_higher_does_not_match() {
        let cfg = BarConfig {
            format: "{weekly}%".into(),
            rules: vec![
                rule("weekly", 95.0, "#e01b24"),
                rule("weekly", 75.0, "#e5a50a"),
            ],
        };
        assert_eq!(
            render(&cfg, &snap(0.0, 80.0), NOW).color,
            Rgb(0xe5, 0xa5, 0x0a)
        );
    }

    #[test]
    fn rule_omitting_format_inherits_the_default() {
        let cfg = BarConfig {
            format: "{weekly}%".into(),
            rules: vec![rule("weekly", 75.0, "#e5a50a")],
        };
        assert_eq!(render(&cfg, &snap(0.0, 80.0), NOW).text, "80%");
    }

    #[test]
    fn rule_format_overrides_the_default() {
        let cfg = BarConfig {
            format: "{weekly}%".into(),
            rules: vec![Rule {
                metric: "weekly".into(),
                above: 75.0,
                format: Some("ALERTA {weekly}%".into()),
                color: None,
                effect: None,
            }],
        };
        assert_eq!(render(&cfg, &snap(0.0, 80.0), NOW).text, "ALERTA 80%");
    }

    #[test]
    fn rule_with_unknown_metric_never_matches() {
        let cfg = BarConfig {
            format: "{weekly}%".into(),
            rules: vec![rule("bogus", 0.0, "#e01b24")],
        };
        assert_eq!(render(&cfg, &snap(0.0, 98.0), NOW).color, DEFAULT_COLOR);
    }

    #[test]
    fn unauthorized_health_overrides_everything() {
        let cfg = BarConfig {
            format: "{weekly}%".into(),
            rules: vec![],
        };
        let s = Snapshot {
            health: crate::state::Health::Unauthorized,
            ..Default::default()
        };
        assert_eq!(render(&cfg, &s, NOW).text, "⚠ reautenticar");
    }

    #[test]
    fn stale_health_annotates_the_age() {
        let cfg = BarConfig {
            format: "{session}%".into(),
            rules: vec![],
        };
        let mut s = snap(24.0, 98.0);
        s.health = crate::state::Health::Stale { seconds: 180 };
        assert_eq!(render(&cfg, &s, NOW).text, "24% · 3m");
    }

    #[test]
    fn parses_hex_colors_and_rejects_junk() {
        assert_eq!(parse_color("#ff8800"), Some(Rgb(255, 136, 0)));
        assert_eq!(parse_color("ff8800"), Some(Rgb(255, 136, 0)));
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("nope"), None);
    }
}
