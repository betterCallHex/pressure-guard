use crate::system::Stress;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub calm_scan_seconds: u64,
    pub stressed_scan_seconds: u64,
    pub hog_cpu_percent: f32,
    pub runaway_cpu_percent: f32,
    pub runaway_seconds: u64,
    pub protect: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            calm_scan_seconds: 30,
            stressed_scan_seconds: 5,
            hog_cpu_percent: 30.0,
            runaway_cpu_percent: 80.0,
            runaway_seconds: 180,
            protect: [
                "universalaccessd",
                "sharingd",
                "rapportd",
                "useractivityd",
                "callservicesd",
                "avconferenced",
                "identityservicesd",
                "bluetoothuserd",
                "corespeechd",
                "assistantd",
                "pressure-guard",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        home().join(".config/pressure-guard/config.toml")
    }

    pub fn load() -> Result<Config, String> {
        let path = Config::path();
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn runaway_duration(&self) -> Duration {
        Duration::from_secs(self.runaway_seconds)
    }

    pub fn scan_interval(&self, stress: Stress) -> Duration {
        let seconds = match stress {
            Stress::Calm => self.calm_scan_seconds,
            Stress::Elevated | Stress::Critical => self.stressed_scan_seconds,
        };
        Duration::from_secs(seconds.max(1))
    }

    pub fn is_protected(&self, name: &str) -> bool {
        any_pattern_matches(&self.protect, name)
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn any_pattern_matches(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pattern| pattern_matches(pattern, name))
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    let name = name.to_lowercase();
    let pattern = pattern.to_lowercase();
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_names_do_not_match_longer_names() {
        assert!(pattern_matches("Dock", "dock"));
        assert!(!pattern_matches("Dock", "Docker"));
    }

    #[test]
    fn trailing_star_matches_by_prefix() {
        assert!(pattern_matches("com.docker.*", "com.docker.backend"));
        assert!(!pattern_matches("com.docker.*", "Docker Desktop"));
    }

    #[test]
    fn scans_rarely_when_calm_and_often_under_stress() {
        let config = Config::default();
        assert_eq!(config.scan_interval(Stress::Calm), Duration::from_secs(30));
        assert_eq!(config.scan_interval(Stress::Elevated), Duration::from_secs(5));
        assert_eq!(config.scan_interval(Stress::Critical), Duration::from_secs(5));
    }
}
