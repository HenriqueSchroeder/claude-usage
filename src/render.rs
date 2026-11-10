use crate::config::{BarConfig, Rule};
use crate::state::{Health, Snapshot};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const DEFAULT_COLOR: Rgb = Rgb(0x87, 0xb1, 0x5b);

/// What a placeholder renders as when the value behind it is not known. A
/// dash is read as "no answer"; a zero is read as an answer.
pub const NO_VALUE: &str = "—";

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
    // `get` returns None on a non-char-boundary index instead of panicking,
    // which matters here: a multi-byte character can make a 6-BYTE string
    // that is not 6 ASCII hex digits (e.g. "1ä234").
    Some(Rgb(
        u8::from_str_radix(h.get(0..2)?, 16).ok()?,
        u8::from_str_radix(h.get(2..4)?, 16).ok()?,
        u8::from_str_radix(h.get(4..6)?, 16).ok()?,
    ))
}

fn metric_value(snap: &Snapshot, metric: &str) -> Option<f32> {
    match metric {
        "session" => snap.session_pct(),
        "weekly" => Some(snap.weekly_pct()),
        // An unknown cost matches no rule: a threshold on a number we do not
        // have is not crossed, it is unanswerable.
        "cost" => snap.cost_usd.map(|c| c as f32),
        _ => None,
    }
}

/// Duration formatter shared by the bar's `{reset}`/`{session_reset}`/
/// `{weekly_reset}` placeholders and the tray menu's quota lines.
///
/// Drops precision as the magnitude grows: minutes alone under an hour,
/// hours+minutes under a day, whole days+hours from a day up — at that
/// scale minutes are noise.
pub fn human_duration(d: time::Duration) -> String {
    let total = d.whole_minutes().max(0);
    let days = total / (24 * 60);
    if days > 0 {
        format!("{days}d{:02}h", (total / 60) % 24)
    } else if total >= 60 {
        format!("{}h{:02}m", total / 60, total % 60)
    } else {
        format!("{total}m")
    }
}

/// How old the reading on screen is.
///
/// Integer division renders the first minute of an outage as `0m`, which
/// reads as "just now" — the one message a staleness marker must not send.
pub fn stale_age(seconds: u64) -> String {
    if seconds < 60 {
        "<1m".into()
    } else {
        format!("{}m", seconds / 60)
    }
}

fn some_or_dash(v: Option<String>) -> String {
    v.filter(|s| !s.is_empty())
        .unwrap_or_else(|| NO_VALUE.into())
}

fn expand(template: &str, snap: &Snapshot, now: OffsetDateTime) -> String {
    let until = |d: Option<time::Duration>| d.map_or_else(|| NO_VALUE.into(), human_duration);
    let session = snap.session.as_ref();

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}').map(|i| start + i) else {
            // No closing brace: push the unterminated remainder exactly
            // once, then clear `rest` so the trailing `push_str` below
            // does not re-push the prefix already written this iteration.
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        // A stray `{` before the real one is literal text, not the start of a
        // key: in "{ {weekly}%" the placeholder is `weekly`, not " {weekly".
        if let Some(inner) = rest[start + 1..end].rfind('{').map(|i| start + 1 + i) {
            out.push_str(&rest[start..inner]);
            rest = &rest[inner..];
            continue;
        }
        let key = &rest[start + 1..end];
        let value = match key {
            "session" => Some(
                snap.session_pct()
                    .map_or_else(|| NO_VALUE.into(), |s| format!("{s:.0}")),
            ),
            // Both windows when there are two, the weekly one alone when the
            // plan has no five-hour window or none is running. A template
            // can't branch, and "—% · 35%" reads as a broken bar.
            "auto" => Some(match snap.session_pct() {
                Some(s) => format!("{s:.0}% · {:.0}%", snap.weekly_pct()),
                None => format!("{:.0}%", snap.weekly_pct()),
            }),
            "weekly" => Some(format!("{:.0}", snap.weekly_pct())),
            // {reset} is the nearest of the two windows, which is almost
            // always the 5-hour one. The two named placeholders exist so a
            // template can say which window it means.
            "reset" => Some(until(snap.reset_in(now))),
            "session_reset" => Some(until(snap.session_reset_in(now))),
            "weekly_reset" => Some(until(snap.weekly_reset_in(now))),
            "cost" => Some(match snap.cost_usd {
                Some(c) => format!("{c:.2}"),
                None => NO_VALUE.into(),
            }),
            // A known placeholder with nothing behind it renders as a dash,
            // not as itself: with no session running these are empty every
            // day, and a literal "{project}" on the panel reads as a broken
            // program rather than as "no session".
            "project" => Some(some_or_dash(session.map(|s| s.project.clone()))),
            "model" => Some(some_or_dash(session.map(|s| s.model.clone()))),
            "tokens" => Some(some_or_dash(
                session.map(|s| format!("{}", s.tokens.output)),
            )),
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

fn problem(text: &str) -> BarOutput {
    BarOutput {
        text: text.into(),
        color: Rgb(0xe0, 0x1b, 0x24),
        effect: Effect::None,
    }
}

pub fn render(cfg: &BarConfig, snap: &Snapshot, now: OffsetDateTime) -> BarOutput {
    match snap.health {
        Health::Unauthorized => return problem("⚠ reautenticar"),
        Health::NoCredentials => return problem("⚠ sem credencial"),
        // No reading has ever landed. A rendered "0%" would look like a full
        // tank, the opposite of what is known.
        _ if snap.quota.is_none() => return problem("⚠ sem dados"),
        _ => {}
    }

    let matched = first_match(&cfg.rules, snap);
    let template = matched
        .and_then(|r| r.format.as_deref())
        .unwrap_or(&cfg.format);

    let mut text = expand(template, snap, now);
    if let Health::Stale { seconds } = snap.health {
        text.push_str(&format!(" · {}", stale_age(seconds)));
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
        q.session.get_or_insert_default().percent = session;
        q.weekly.percent = weekly;
        q.session.get_or_insert_default().resets_at = Some(datetime!(2026-09-15 15:40 UTC));
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
    fn a_stray_open_brace_does_not_swallow_the_next_placeholder() {
        let fmt = |f: &str| {
            let cfg = BarConfig {
                format: f.into(),
                rules: vec![],
            };
            render(&cfg, &snap(24.0, 98.0), NOW).text
        };
        assert_eq!(fmt("{ {weekly}%"), "{ 98%");
        assert_eq!(fmt("{{weekly}}"), "{98}");
        assert_eq!(fmt("{nope {weekly}"), "{nope 98");
        assert_eq!(fmt("{nope}"), "{nope}");
    }

    #[test]
    fn auto_shows_both_windows_or_only_the_week() {
        let cfg = BarConfig {
            format: "{auto}".into(),
            rules: vec![],
        };
        assert_eq!(render(&cfg, &snap(24.0, 98.0), NOW).text, "24% · 98%");

        let mut no_session = snap(0.0, 35.0);
        no_session.quota.as_mut().unwrap().session = None;
        assert_eq!(render(&cfg, &no_session, NOW).text, "35%");
    }

    #[test]
    fn a_missing_session_window_is_a_dash_and_matches_no_rule() {
        let mut s = snap(0.0, 35.0);
        s.quota.as_mut().unwrap().session = None;
        let cfg = BarConfig {
            format: "{session} {session_reset}".into(),
            rules: vec![rule("session", -1.0, "#e01b24")],
        };
        let out = render(&cfg, &s, NOW);
        assert_eq!(out.text, "— —");
        assert_eq!(out.color, DEFAULT_COLOR);
    }

    #[test]
    fn renders_reset_as_human_duration() {
        let cfg = BarConfig {
            format: "{reset}".into(),
            rules: vec![],
        };
        assert_eq!(render(&cfg, &snap(24.0, 98.0), NOW).text, "1h00m");
    }

    #[test]
    fn each_reset_placeholder_names_its_own_window() {
        let mut s = snap(24.0, 96.0);
        // 5-hour window clears in an hour, weekly in four days.
        s.quota.as_mut().unwrap().weekly.resets_at = Some(datetime!(2026-09-19 14:40 UTC));

        let text = |fmt: &str| {
            render(
                &BarConfig {
                    format: fmt.into(),
                    rules: vec![],
                },
                &s,
                NOW,
            )
            .text
        };

        // The spec's own example rule used to claim the weekly window was
        // an hour out because {reset} silently meant the nearer one.
        assert_eq!(
            text("⚠ {weekly}% · reset {weekly_reset}"),
            "⚠ 96% · reset 4d00h"
        );
        assert_eq!(text("{session_reset}"), "1h00m");
        // {reset} keeps meaning "the nearest of the two".
        assert_eq!(text("{reset}"), "1h00m");
    }

    #[test]
    fn a_window_with_no_reset_time_renders_a_dash() {
        let cfg = BarConfig {
            format: "{weekly_reset}".into(),
            rules: vec![],
        };
        // snap() sets only the session window's reset.
        assert_eq!(render(&cfg, &snap(1.0, 1.0), NOW).text, "—");
    }

    #[test]
    fn session_placeholders_render_a_dash_when_no_session_is_running() {
        let cfg = BarConfig {
            format: "{project} {model} {tokens} {cost}".into(),
            rules: vec![],
        };
        // The everyday case now that a finished session drops out of the
        // snapshot: a literal "{project}" would look like a broken program.
        assert_eq!(render(&cfg, &snap(24.0, 80.0), NOW).text, "— — — —");
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
    fn a_stale_snapshot_with_no_reading_says_so_instead_of_zero() {
        let cfg = BarConfig {
            format: "{session}% · {weekly}%".into(),
            rules: vec![],
        };
        let s = Snapshot {
            health: crate::state::Health::Stale { seconds: 0 },
            ..Default::default()
        };
        assert_eq!(render(&cfg, &s, NOW).text, "⚠ sem dados");
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
    fn the_first_minute_of_an_outage_does_not_read_as_fresh() {
        // "· 0m" says "just now" about a reading that is already wrong.
        assert_eq!(stale_age(0), "<1m");
        assert_eq!(stale_age(59), "<1m");
        assert_eq!(stale_age(60), "1m");
    }

    #[test]
    fn human_duration_drops_precision_as_magnitude_grows() {
        assert_eq!(human_duration(time::Duration::minutes(51)), "51m");
        assert_eq!(human_duration(time::Duration::minutes(291)), "4h51m");
        // A day or more rounds to hours; minutes are noise at that scale.
        assert_eq!(human_duration(time::Duration::minutes(8400)), "5d20h");
    }

    #[test]
    fn human_duration_boundary_transitions() {
        // Sub-minute durations floor to zero, not a blank or negative value.
        assert_eq!(human_duration(time::Duration::seconds(30)), "0m");
        // Bare minutes give way to h+m the instant an hour is reached.
        assert_eq!(human_duration(time::Duration::minutes(59)), "59m");
        assert_eq!(human_duration(time::Duration::minutes(60)), "1h00m");
        // h+m gives way to day+hour the instant a full day is reached.
        assert_eq!(human_duration(time::Duration::minutes(1439)), "23h59m");
        assert_eq!(human_duration(time::Duration::minutes(1440)), "1d00h");
    }

    #[test]
    fn parses_hex_colors_and_rejects_junk() {
        assert_eq!(parse_color("#ff8800"), Some(Rgb(255, 136, 0)));
        assert_eq!(parse_color("ff8800"), Some(Rgb(255, 136, 0)));
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn parse_color_rejects_a_six_byte_multi_byte_string() {
        // "1ä234" is 6 BYTES but not 6 ASCII hex digits; slicing at fixed
        // byte offsets must not panic on the split multi-byte char.
        assert_eq!(parse_color("1ä234"), None);
    }

    #[test]
    fn unterminated_brace_does_not_duplicate_text() {
        assert_eq!(expand("X{Y", &snap(0.0, 0.0), NOW), "X{Y");
    }

    #[test]
    fn placeholder_then_unterminated_brace_does_not_duplicate_text() {
        assert_eq!(
            expand("{weekly}% {nope", &snap(0.0, 80.0), NOW),
            "80% {nope"
        );
    }
}
