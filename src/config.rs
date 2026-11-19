use crate::system::Stress;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub calm_scan_seconds: u64,
    pub stressed_scan_seconds: u64,
    pub busy_cpu_percent: f32,
    pub hog_cpu_percent: f32,
    pub critical_hog_cpu_percent: f32,
    pub runaway_cpu_percent: f32,
    pub runaway_seconds: u64,
    pub protect: Vec<String>,
    pub agent_programs: Vec<String>,
    pub agent_nice: i32,
    pub agent_release_after_seconds: u64,
    pub agent_idle_cpu_percent: f32,
    pub agent_idle_seconds: u64,
    pub early_memory_free_percent: i32,
    pub thrashing_megabytes_per_second: f64,
    pub turn_seconds: u64,
    pub turn_megabytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            calm_scan_seconds: 30,
            stressed_scan_seconds: 5,
            busy_cpu_percent: 90.0,
            hog_cpu_percent: 30.0,
            critical_hog_cpu_percent: 10.0,
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
            agent_programs: ["claude", "codex"].map(String::from).to_vec(),
            agent_nice: 10,
            agent_release_after_seconds: 60,
            agent_idle_cpu_percent: 5.0,
            agent_idle_seconds: 10,
            early_memory_free_percent: 30,
            thrashing_megabytes_per_second: 600.0,
            turn_seconds: 30,
            turn_megabytes: 256,
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

    pub fn modified() -> Option<SystemTime> {
        std::fs::metadata(Config::path()).and_then(|metadata| metadata.modified()).ok()
    }

    pub fn runaway_duration(&self) -> Duration {
        Duration::from_secs(self.runaway_seconds)
    }

    pub fn scan_interval(&self, urgent: bool) -> Duration {
        let seconds = if urgent { self.stressed_scan_seconds } else { self.calm_scan_seconds };
        Duration::from_secs(seconds.max(1))
    }

    pub fn hog_cpu_percent_under(&self, stress: Stress) -> f32 {
        match stress {
            Stress::Critical => self.critical_hog_cpu_percent,
            Stress::Calm | Stress::Elevated => self.hog_cpu_percent,
        }
    }

    pub fn lowest_trigger_cpu_percent(&self) -> f32 {
        self.hog_cpu_percent.min(self.critical_hog_cpu_percent).min(self.runaway_cpu_percent)
    }

    pub fn is_protected(&self, name: &str) -> bool {
        any_pattern_matches(&self.protect, name)
    }

    pub fn agent_release_after(&self) -> Duration {
        Duration::from_secs(self.agent_release_after_seconds)
    }

    pub fn agent_idle(&self) -> Duration {
        Duration::from_secs(self.agent_idle_seconds)
    }

    pub fn is_agent_program(&self, name: &str) -> bool {
        any_pattern_matches(&self.agent_programs, name)
    }

    pub fn is_thrashing(&self, repacked_megabytes_per_second: f64) -> bool {
        self.thrashing_megabytes_per_second > 0.0 && repacked_megabytes_per_second >= self.thrashing_megabytes_per_second
    }

    pub fn turn(&self) -> Option<Duration> {
        (self.turn_seconds > 0).then(|| Duration::from_secs(self.turn_seconds))
    }

    pub fn takes_turns_from(&self, footprint_bytes: u64) -> bool {
        footprint_bytes >= self.turn_megabytes << 20
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
    fn critical_stress_lowers_the_busy_threshold() {
        let config = Config::default();
        assert_eq!(config.hog_cpu_percent_under(Stress::Elevated), 30.0);
        assert_eq!(config.hog_cpu_percent_under(Stress::Critical), 10.0);
        assert_eq!(config.lowest_trigger_cpu_percent(), 10.0);
    }

    #[test]
    fn memory_thrashes_from_600_megabytes_repacked_a_second_unless_the_check_is_turned_off() {
        let config = Config::default();
        assert!(config.is_thrashing(1609.0));
        assert!(config.is_thrashing(600.0));
        assert!(!config.is_thrashing(250.0));
        assert!(!Config { thrashing_megabytes_per_second: 0.0, ..Config::default() }.is_thrashing(1609.0));
    }
}
