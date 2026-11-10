use serde::Deserialize;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Severity {
    #[default]
    Normal,
    Warning,
    Critical,
}

impl From<&str> for Severity {
    fn from(s: &str) -> Self {
        match s {
            "critical" => Severity::Critical,
            "warning" => Severity::Warning,
            _ => Severity::Normal,
        }
    }
}

impl Severity {
    /// The inverse of the `From<&str>` above. The two live together so a name
    /// can never be added to one without the other losing track of it.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::Warning => "warning",
            Severity::Normal => "normal",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Window {
    pub percent: f32,
    pub resets_at: Option<OffsetDateTime>,
    pub severity: Severity,
}

#[derive(Debug, Clone)]
pub struct BreakdownRow {
    pub display_name: String,
    pub percent: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Quota {
    pub session: Window,
    pub weekly: Window,
    pub breakdown: Vec<BreakdownRow>,
}

/// What a single fetch produced, before the caller reconciles it against
/// the last known reading.
///
/// `None` means this response carried no number for that window — the same
/// distinction `parse_usage` needs internally to decide whether the
/// response is recognised at all — and is not the same thing as
/// `Some(Window { percent: 0.0, .. })`, a genuine zero reading.
#[derive(Debug, Default)]
pub struct ParsedUsage {
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    pub breakdown: Vec<BreakdownRow>,
}

#[derive(Deserialize, Default)]
struct RawWindow {
    #[serde(default)]
    utilization: Option<f32>,
    #[serde(default)]
    resets_at: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawLimit {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    percent: Option<f32>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    resets_at: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawBreakdownRow {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    percent: Option<f32>,
}

#[derive(Deserialize, Default)]
struct RawBreakdown {
    #[serde(default)]
    rows: Option<Vec<RawBreakdownRow>>,
}

/// Tells a key sent as `null` (`Some(None)`) from a key not sent at all
/// (`None`, via `#[serde(default)]`). Plain `Option` folds the two together.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

#[derive(Deserialize, Default)]
struct RawUsage {
    #[serde(default, deserialize_with = "present")]
    five_hour: Option<Option<RawWindow>>,
    #[serde(default)]
    seven_day: Option<RawWindow>,
    #[serde(default)]
    limits: Option<Vec<RawLimit>>,
    #[serde(default)]
    seven_day_breakdown: Option<RawBreakdown>,
}

fn parse_ts(s: &Option<String>) -> Option<OffsetDateTime> {
    let s = s.as_ref()?;
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

/// `None` when the entry carries no percentage. The object being present is
/// not a reading — a `limits[]` entry whose `percent` was renamed is exactly
/// as uninformative as a missing entry, and must not become 0%.
fn window_from_limit(l: &RawLimit) -> Option<Window> {
    Some(Window {
        percent: l.percent?,
        resets_at: parse_ts(&l.resets_at),
        severity: l.severity.as_deref().unwrap_or("normal").into(),
    })
}

/// `None` when the window carries no utilization, for the same reason.
fn window_from_raw(w: Option<&RawWindow>) -> Option<Window> {
    let w = w?;
    Some(Window {
        percent: w.utilization?,
        resets_at: parse_ts(&w.resets_at),
        severity: Severity::Normal,
    })
}

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
pub const USER_AGENT: &str = concat!("claude-usage/", env!("CARGO_PKG_VERSION"));

/// A `Retry-After` longer than this is ignored in favour of the caller's own
/// backoff. The header is server-controlled; without a ceiling a malformed or
/// hostile value could park the daemon for days.
const RETRY_AFTER_CAP_SECONDS: u64 = 3600;

#[derive(Debug)]
pub enum FetchError {
    Unauthorized,
    /// The server asked us to slow down, and said for how long when it sent a
    /// readable `Retry-After`. Distinct from `Network` because the interval to
    /// wait is the server's to choose, not ours to guess.
    RateLimited {
        retry_after: Option<u64>,
    },
    Network(String),
    Parse(String),
}

/// Seconds from a `Retry-After` header.
///
/// The header also permits an HTTP date. This endpoint sends seconds, and a
/// form we cannot read falls back to the caller's own backoff — exactly the
/// behaviour that existed before the header was read at all.
///
/// Zero is treated as no interval at all. This endpoint answers a 429 with
/// `Retry-After: 0`, observed in production, and retrying immediately is the
/// one thing a rate limit is asking us not to do. `wait_seconds` already
/// floors at the backoff, so a zero was harmless there — but it reached the
/// diagnostics as "the server asked for 0s", which is a sentence that means
/// nothing.
fn parse_retry_after(h: Option<&str>) -> Option<u64> {
    let secs: u64 = h?.trim().parse().ok()?;
    (secs > 0).then(|| secs.min(RETRY_AFTER_CAP_SECONDS))
}

fn map_status(code: u16, retry_after: Option<u64>, body: String) -> FetchError {
    match code {
        401 | 403 => FetchError::Unauthorized,
        429 => FetchError::RateLimited { retry_after },
        _ => FetchError::Network(format!("HTTP {code}: {body}")),
    }
}

/// Fetches and parses the quota endpoint response over HTTP.
pub fn fetch(agent: &ureq::Agent, token: &str) -> Result<ParsedUsage, FetchError> {
    let resp = agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", USER_AGENT)
        .call();

    let body = match resp {
        Ok(r) => r
            .into_string()
            .map_err(|e| FetchError::Network(e.to_string()))?,
        Err(ureq::Error::Status(code, r)) => {
            // The header has to come off before `into_string` consumes `r`.
            let retry_after = parse_retry_after(r.header("retry-after"));
            return Err(map_status(
                code,
                retry_after,
                r.into_string().unwrap_or_default(),
            ));
        }
        Err(e) => return Err(FetchError::Network(e.to_string())),
    };

    parse_usage(body.as_bytes()).map_err(FetchError::Parse)
}

/// Parses the quota endpoint response.
///
/// The endpoint is undocumented and ships codenamed keys that are mostly
/// null, so each individual field stays optional: a response carrying only
/// one of the two windows still parses, and unknown keys are ignored.
///
/// A response in which *no* window carries a usable number is a different
/// thing entirely — not a reading of zero, but a shape we no longer
/// understand. That fails, so the caller keeps the last good reading and
/// backs off instead of painting a confident green 0%.
///
/// The test is the number, not the object around it. A retained `five_hour`
/// whose `utilization` was renamed is exactly as uninformative as a missing
/// `five_hour`, and a payload reshuffle is at least as likely as a dropped
/// key.
///
/// A window this response simply didn't carry comes back as `None`, not a
/// default `Window`. This function has no memory of the last reading, so it
/// cannot decide whether a missing window should fall back to something
/// carried forward or be treated as never-read — that decision, and the
/// data needed to make it, belongs to the caller.
pub fn parse_usage(bytes: &[u8]) -> Result<ParsedUsage, String> {
    let raw: RawUsage = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;

    let mut session = window_from_raw(raw.five_hour.as_ref().and_then(Option::as_ref));
    let mut weekly = window_from_raw(raw.seven_day.as_ref());

    // limits[] is the normalized form and carries server-decided severity, so
    // it wins over the top-level windows — but only when it actually brought a
    // number. `or` keeps whatever the top level already gave us otherwise.
    for l in raw.limits.iter().flatten() {
        match l.kind.as_deref() {
            Some("session") => session = window_from_limit(l).or(session),
            Some("weekly_all") => weekly = window_from_limit(l).or(weekly),
            _ => {}
        }
    }

    if session.is_none() && weekly.is_none() {
        return Err(
            "no window in the response carried a usage value (five_hour/seven_day \
             utilization, or limits[].percent)"
                .into(),
        );
    }

    // With no five-hour window running, the endpoint sends `five_hour: null`
    // and no `session` entry in limits[]. Next to a real weekly number that
    // is a reading — nothing used, nothing to reset — not a dropped key: a
    // renamed key would be absent, not present and null. Checked after the
    // test above so an all-null response still fails.
    if session.is_none() && matches!(raw.five_hour, Some(None)) {
        session = Some(Window::default());
    }

    let breakdown = raw
        .seven_day_breakdown
        .and_then(|b| b.rows)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some(BreakdownRow {
                display_name: r.display_name?,
                percent: r.percent.unwrap_or(0.0),
            })
        })
        .collect();

    // At least one window is real; the other stays `None` rather than a
    // fabricated 0%. That is the tolerance this module is meant to have —
    // half a known response is still a reading, but the missing half must
    // say so.
    Ok(ParsedUsage {
        session,
        weekly,
        breakdown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/usage.json");

    #[test]
    fn parses_the_real_response() {
        let q = parse_usage(FIXTURE).unwrap();
        let session = q.session.unwrap().percent;
        let weekly = q.weekly.unwrap().percent;
        assert!((0.0..=100.0).contains(&session));
        assert!((0.0..=100.0).contains(&weekly));
    }

    #[test]
    fn prefers_the_limits_array() {
        let json = br#"{
            "five_hour": {"utilization": 1.0},
            "seven_day": {"utilization": 2.0},
            "limits": [
                {"kind": "session", "percent": 33, "severity": "normal"},
                {"kind": "weekly_all", "percent": 77, "severity": "critical"}
            ]
        }"#;
        let q = parse_usage(json).unwrap();
        assert_eq!(q.session.unwrap().percent, 33.0);
        let weekly = q.weekly.unwrap();
        assert_eq!(weekly.percent, 77.0);
        assert!(matches!(weekly.severity, Severity::Critical));
    }

    #[test]
    fn falls_back_to_top_level_windows_when_limits_absent() {
        let json = br#"{"five_hour":{"utilization":24.0},"seven_day":{"utilization":98.0}}"#;
        let q = parse_usage(json).unwrap();
        assert_eq!(q.session.unwrap().percent, 24.0);
        assert_eq!(q.weekly.unwrap().percent, 98.0);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let json = br#"{"five_hour":{"utilization":5.0},"nimbus_quill":{"utilization":0.0},
                        "a_key_invented_tomorrow":{"nested":[1,2,3]}}"#;
        assert_eq!(parse_usage(json).unwrap().session.unwrap().percent, 5.0);
    }

    #[test]
    fn everything_null_is_an_error_not_a_reading_of_zero() {
        // Null windows carry no number. Parsing them as 0% would paint a
        // confident empty tank over whatever the user is actually at.
        let json = br#"{"five_hour":null,"seven_day":null,"limits":null,
                        "seven_day_breakdown":null}"#;
        assert!(parse_usage(json).is_err());
    }

    #[test]
    fn a_null_session_beside_a_real_week_is_an_idle_session() {
        // What the endpoint answers when no five-hour window is running.
        // Treating it as a dropped window left a fresh install with no
        // reading at all until the user happened to start a session.
        let q = parse_usage(
            br#"{"five_hour":null,"seven_day":{"utilization":35.0},
                 "limits":[{"kind":"weekly_all","percent":35}]}"#,
        )
        .unwrap();
        let session = q.session.expect("an idle session is a reading");
        assert_eq!(session.percent, 0.0);
        assert!(session.resets_at.is_none());
        assert_eq!(q.weekly.unwrap().percent, 35.0);
    }

    #[test]
    fn empty_object_is_an_error() {
        assert!(parse_usage(b"{}").is_err());
    }

    #[test]
    fn a_response_of_only_renamed_keys_is_an_error() {
        // The renaming scenario: the endpoint still answers 200 with JSON,
        // but nothing in it is a window we know how to read.
        let json = br#"{"nimbus_quill":{"utilization":97.0},
                        "limits":[{"kind":"invented_tomorrow","percent":97}]}"#;
        assert!(parse_usage(json).is_err());
    }

    #[test]
    fn a_missing_window_is_none_not_a_reading_of_zero() {
        // Tolerance is still this module's job: half a known response is a
        // reading, not a failure. But the missing half must surface as
        // `None` so the caller can tell "never sent" apart from "sent as
        // 0" — filling it with a default here would be the original bug
        // verbatim, just moved from the caller into this function.
        let q = parse_usage(br#"{"five_hour":{"utilization":42.0}}"#).unwrap();
        assert_eq!(q.session.unwrap().percent, 42.0);
        assert!(q.weekly.is_none(), "seven_day was never in the response");

        let q = parse_usage(br#"{"limits":[{"kind":"weekly_all","percent":8}]}"#).unwrap();
        assert_eq!(q.weekly.unwrap().percent, 8.0);
        assert!(q.session.is_none());
    }

    #[test]
    fn a_genuine_zero_window_is_still_reported_as_zero() {
        // The over-correction this must not make: a window that IS in the
        // response, carrying an honest 0, is `Some(Window{percent: 0.0})` —
        // not treated as absent just because the number is falsy.
        let json = br#"{"five_hour":{"utilization":0.0},"seven_day":{"utilization":50.0}}"#;
        let q = parse_usage(json).unwrap();
        assert_eq!(q.session.unwrap().percent, 0.0);
        assert_eq!(q.weekly.unwrap().percent, 50.0);
    }

    #[test]
    fn a_window_whose_value_field_was_renamed_is_not_a_reading_of_zero() {
        // The likeliest shape change of all: the objects stay, the field
        // inside them is renamed. Reading that as 0% would be the original
        // bug verbatim — green zeros, last good value destroyed, armed
        // thresholds cleared.
        let json = br#"{"five_hour":{"used_pct":24.0},"seven_day":{"used_pct":96.0}}"#;
        assert!(parse_usage(json).is_err());
    }

    #[test]
    fn a_window_object_carrying_no_number_is_not_a_reading() {
        // Present but empty, explicitly null, and a limits[] entry of a known
        // kind with no percent: none of these is a reading.
        assert!(parse_usage(br#"{"five_hour":{}}"#).is_err());
        assert!(parse_usage(br#"{"five_hour":{"utilization":null}}"#).is_err());
        assert!(parse_usage(br#"{"limits":[{"kind":"session"}]}"#).is_err());
    }

    #[test]
    fn a_valueless_limits_entry_does_not_erase_the_top_level_window() {
        // limits[] wins only when it brought a number. Otherwise the
        // top-level reading stands rather than being overwritten with 0%.
        let json = br#"{"five_hour":{"utilization":37.0},
                        "limits":[{"kind":"session","severity":"critical"}]}"#;
        let q = parse_usage(json).unwrap();
        assert_eq!(q.session.unwrap().percent, 37.0);
    }

    #[test]
    fn reads_the_breakdown_rows() {
        let q = parse_usage(FIXTURE).unwrap();
        assert!(q.breakdown.iter().any(|r| r.display_name == "Claude Code"));
    }

    #[test]
    fn http_401_maps_to_unauthorized() {
        assert!(matches!(
            map_status(401, None, String::new()),
            FetchError::Unauthorized
        ));
    }

    #[test]
    fn other_http_errors_map_to_network() {
        assert!(matches!(
            map_status(503, None, "down".into()),
            FetchError::Network(_)
        ));
    }

    #[test]
    fn http_429_carries_the_interval_the_server_asked_for() {
        assert!(matches!(
            map_status(429, Some(120), String::new()),
            FetchError::RateLimited {
                retry_after: Some(120)
            }
        ));
    }

    #[test]
    fn a_429_without_a_readable_interval_is_still_a_rate_limit() {
        // The caller falls back to its own backoff; what it must not do is
        // treat this as an ordinary network blip and keep the old pacing.
        assert!(matches!(
            map_status(429, None, String::new()),
            FetchError::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn retry_after_reads_seconds_and_refuses_everything_else() {
        assert_eq!(parse_retry_after(Some("120")), Some(120));
        assert_eq!(parse_retry_after(Some("  120 ")), Some(120));
        assert_eq!(parse_retry_after(None), None);
        // The HTTP-date form is legal but unused by this endpoint; unreadable
        // is the correct answer, not a guess.
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        assert_eq!(parse_retry_after(Some("")), None);
        assert_eq!(parse_retry_after(Some("-5")), None);
        // What this endpoint actually sends with a 429.
        assert_eq!(parse_retry_after(Some("0")), None);
    }

    #[test]
    fn a_hostile_retry_after_cannot_park_the_daemon_for_days() {
        assert_eq!(
            parse_retry_after(Some("999999")),
            Some(RETRY_AFTER_CAP_SECONDS)
        );
    }
}
