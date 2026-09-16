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

#[derive(Deserialize, Default)]
struct RawUsage {
    #[serde(default)]
    five_hour: Option<RawWindow>,
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

fn window_from_limit(l: &RawLimit) -> Window {
    Window {
        percent: l.percent.unwrap_or(0.0),
        resets_at: parse_ts(&l.resets_at),
        severity: l.severity.as_deref().unwrap_or("normal").into(),
    }
}

fn window_from_raw(w: &Option<RawWindow>) -> Window {
    match w {
        Some(w) => Window {
            percent: w.utilization.unwrap_or(0.0),
            resets_at: parse_ts(&w.resets_at),
            severity: Severity::Normal,
        },
        None => Window::default(),
    }
}

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
pub const USER_AGENT: &str = concat!("claude-usage/", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub enum FetchError {
    Unauthorized,
    Network(String),
    Parse(String),
}

fn map_status(code: u16, body: String) -> FetchError {
    match code {
        401 | 403 => FetchError::Unauthorized,
        _ => FetchError::Network(format!("HTTP {code}: {body}")),
    }
}

/// Fetches and parses the quota endpoint response over HTTP.
pub fn fetch(agent: &ureq::Agent, token: &str) -> Result<Quota, FetchError> {
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
            return Err(map_status(code, r.into_string().unwrap_or_default()))
        }
        Err(e) => return Err(FetchError::Network(e.to_string())),
    };

    parse_usage(body.as_bytes()).map_err(|e| FetchError::Parse(e.to_string()))
}

/// Parses the quota endpoint response.
///
/// The endpoint is undocumented and ships codenamed keys that are mostly
/// null. Every field is optional on purpose: an unknown shape degrades to
/// zeros, it never fails the daemon.
pub fn parse_usage(bytes: &[u8]) -> Result<Quota, serde_json::Error> {
    let raw: RawUsage = serde_json::from_slice(bytes)?;

    let mut session = window_from_raw(&raw.five_hour);
    let mut weekly = window_from_raw(&raw.seven_day);

    // limits[] is the normalized form and carries server-decided severity,
    // so it wins over the top-level windows when present.
    for l in raw.limits.iter().flatten() {
        match l.kind.as_deref() {
            Some("session") => session = window_from_limit(l),
            Some("weekly_all") => weekly = window_from_limit(l),
            _ => {}
        }
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

    Ok(Quota {
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
        assert!(q.session.percent >= 0.0 && q.session.percent <= 100.0);
        assert!(q.weekly.percent >= 0.0 && q.weekly.percent <= 100.0);
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
        assert_eq!(q.session.percent, 33.0);
        assert_eq!(q.weekly.percent, 77.0);
        assert!(matches!(q.weekly.severity, Severity::Critical));
    }

    #[test]
    fn falls_back_to_top_level_windows_when_limits_absent() {
        let json = br#"{"five_hour":{"utilization":24.0},"seven_day":{"utilization":98.0}}"#;
        let q = parse_usage(json).unwrap();
        assert_eq!(q.session.percent, 24.0);
        assert_eq!(q.weekly.percent, 98.0);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let json = br#"{"five_hour":{"utilization":5.0},"nimbus_quill":{"utilization":0.0},
                        "a_key_invented_tomorrow":{"nested":[1,2,3]}}"#;
        assert_eq!(parse_usage(json).unwrap().session.percent, 5.0);
    }

    #[test]
    fn everything_null_yields_zeros_not_panic() {
        let json = br#"{"five_hour":null,"seven_day":null,"limits":null,
                        "seven_day_breakdown":null}"#;
        let q = parse_usage(json).unwrap();
        assert_eq!(q.session.percent, 0.0);
        assert!(q.breakdown.is_empty());
    }

    #[test]
    fn empty_object_yields_zeros() {
        let q = parse_usage(b"{}").unwrap();
        assert_eq!(q.weekly.percent, 0.0);
        assert!(q.weekly.resets_at.is_none());
    }

    #[test]
    fn reads_the_breakdown_rows() {
        let q = parse_usage(FIXTURE).unwrap();
        assert!(q.breakdown.iter().any(|r| r.display_name == "Claude Code"));
    }

    #[test]
    fn http_401_maps_to_unauthorized() {
        assert!(matches!(
            map_status(401, String::new()),
            FetchError::Unauthorized
        ));
    }

    #[test]
    fn other_http_errors_map_to_network() {
        assert!(matches!(
            map_status(503, "down".into()),
            FetchError::Network(_)
        ));
    }
}
