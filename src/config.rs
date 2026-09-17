use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub metric: String,
    pub above: f32,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub effect: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BarConfig {
    pub format: String,
    pub rules: Vec<Rule>,
}

impl Default for BarConfig {
    fn default() -> Self {
        BarConfig {
            format: "{session}% · {weekly}%".into(),
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThresholdConfig {
    pub levels: Vec<u8>,
    pub hysteresis: f32,
    pub notify: bool,
}

impl Default for ThresholdConfig {
    fn default() -> Self {
        ThresholdConfig {
            levels: vec![75, 90, 95],
            hysteresis: 2.0,
            notify: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ResetConfig {
    pub notify: bool,
    pub effect: String,
    pub duration: u64,
}

impl Default for ResetConfig {
    fn default() -> Self {
        ResetConfig {
            notify: true,
            effect: "pulse".into(),
            duration: 2,
        }
    }
}

/// Ceiling on an animation's length, in seconds.
///
/// A misconfigured huge `duration` would otherwise push a frame every
/// `icon::FRAME_INTERVAL_MS` for however long the value says, one D-Bus
/// update at a time, instead of the brief nudge the feature is for.
const MAX_PULSE_SECONDS: u64 = 30;

impl ResetConfig {
    /// The animation length to use, or `None` for "do not animate".
    ///
    /// Both `effect = "none"` and `duration = 0` mean silence, not a
    /// degenerate one-frame flicker — a user turning this off must get
    /// nothing, not a blink.
    pub fn pulse_duration(&self) -> Option<u64> {
        (self.effect == "pulse" && self.duration > 0)
            .then_some(self.duration.min(MAX_PULSE_SECONDS))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub poll_seconds: u64,
    pub bar: BarConfig,
    pub threshold: ThresholdConfig,
    pub reset: ResetConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            poll_seconds: 60,
            bar: BarConfig::default(),
            threshold: ThresholdConfig::default(),
            reset: ResetConfig::default(),
        }
    }
}

fn parse(text: &str) -> Config {
    match toml::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("claude-usage: invalid config, using defaults: {e}");
            Config::default()
        }
    }
}

/// Loads the config, falling back to defaults on any problem.
/// A broken config must never stop the daemon from starting.
pub fn load(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => Config::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let c = load(std::path::Path::new("/nonexistent/config.toml"));
        assert_eq!(c.poll_seconds, 60);
        assert_eq!(c.bar.format, "{session}% · {weekly}%");
        assert_eq!(c.threshold.levels, vec![75, 90, 95]);
    }

    #[test]
    fn parses_a_full_config() {
        let toml = r##"
poll_seconds = 30
[bar]
format = "{weekly}%"
[[bar.rules]]
metric = "weekly"
above = 95
color = "#e01b24"
effect = "pulse"
"##;
        let c = parse(toml);
        assert_eq!(c.poll_seconds, 30);
        assert_eq!(c.bar.rules.len(), 1);
        assert_eq!(c.bar.rules[0].above, 95.0);
    }

    #[test]
    fn invalid_toml_falls_back_to_defaults_not_panic() {
        let c = parse("this is not = = toml");
        assert_eq!(c.poll_seconds, 60);
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let c = parse("poll_seconds = 15");
        assert_eq!(c.poll_seconds, 15);
        assert_eq!(c.threshold.hysteresis, 2.0);
    }

    #[test]
    fn reset_pulse_duration_is_some_for_the_default() {
        assert_eq!(ResetConfig::default().pulse_duration(), Some(2));
    }

    #[test]
    fn reset_pulse_duration_is_none_when_effect_is_not_pulse() {
        let r = ResetConfig {
            effect: "none".into(),
            ..ResetConfig::default()
        };
        assert_eq!(r.pulse_duration(), None);
    }

    #[test]
    fn reset_pulse_duration_is_none_when_duration_is_zero() {
        let r = ResetConfig {
            duration: 0,
            ..ResetConfig::default()
        };
        assert_eq!(r.pulse_duration(), None);
    }

    #[test]
    fn reset_pulse_duration_is_capped() {
        let r = ResetConfig {
            duration: 999,
            ..ResetConfig::default()
        };
        assert_eq!(r.pulse_duration(), Some(30));
    }
}
