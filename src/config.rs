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
}
