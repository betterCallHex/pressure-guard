use crate::config::Config;
use crate::system::{describe_saturation, ProcessSample, Snapshot, Stress};
use crate::friendly_duration;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Throttle {
    pub name: String,
    pub cpu_percent: f32,
    pub reason: Reason,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    Runaway { hot_for: Duration },
    Stress { memory: Stress, thermal: Stress, saturated_clusters: Vec<String> },
    StillCooling,
}

pub fn decide(snapshot: &Snapshot, config: &Config, throttled: &BTreeSet<u32>) -> BTreeMap<u32, Throttle> {
    background_services(snapshot, config)
        .filter_map(|process| {
            let reason = reason_to_throttle(process, snapshot, config, throttled)?;
            Some((process.pid, Throttle { name: process.name.clone(), cpu_percent: process.cpu_percent, reason }))
        })
        .collect()
}

pub fn scan_interval(snapshot: &Snapshot, config: &Config, throttled: &BTreeSet<u32>) -> Duration {
    let running_hot = background_services(snapshot, config)
        .any(|process| process.cpu_percent >= config.runaway_cpu_percent && !throttled.contains(&process.pid));
    config.scan_interval(snapshot.stress() != Stress::Calm || running_hot)
}

fn background_services<'a>(snapshot: &'a Snapshot, config: &'a Config) -> impl Iterator<Item = &'a ProcessSample> {
    snapshot
        .processes
        .iter()
        .filter(|process| process.made_by_apple && !process.user_facing && !config.is_protected(&process.name))
}

fn reason_to_throttle(
    process: &ProcessSample,
    snapshot: &Snapshot,
    config: &Config,
    throttled: &BTreeSet<u32>,
) -> Option<Reason> {
    let window = config.runaway_duration();
    let stress = snapshot.stress();
    if process.hot_for >= window {
        return Some(Reason::Runaway { hot_for: process.hot_for });
    }
    if stress != Stress::Calm && process.cpu_percent >= config.hog_cpu_percent_under(stress) {
        return Some(Reason::Stress {
            memory: snapshot.memory,
            thermal: snapshot.thermal,
            saturated_clusters: snapshot.saturated_clusters.clone(),
        });
    }
    let cooling = throttled.contains(&process.pid) && (stress != Stress::Calm || process.quiet_for < window);
    cooling.then_some(Reason::StillCooling)
}

impl Reason {
    pub fn kind(&self) -> &'static str {
        match self {
            Reason::Runaway { .. } => "runaway",
            Reason::Stress { .. } => "stress",
            Reason::StillCooling => "still_cooling",
        }
    }
}

impl fmt::Display for Throttle {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "at {:.0}% CPU, {}", self.cpu_percent, self.reason)
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Reason::Runaway { hot_for } => write!(formatter, "busy for {} straight", friendly_duration(*hot_for)),
            Reason::Stress { memory, thermal, saturated_clusters } => {
                let mut causes: Vec<String> = [("memory pressure", memory), ("thermal state", thermal)]
                    .into_iter()
                    .filter(|(_, level)| **level != Stress::Calm)
                    .map(|(cause, level)| format!("{cause} {}", level.label()))
                    .collect();
                if !saturated_clusters.is_empty() {
                    causes.push(describe_saturation(saturated_clusters));
                }
                write!(formatter, "{}", causes.join(" and "))
            }
            Reason::StillCooling => write!(formatter, "not quiet long enough to release"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::Swapping;

    fn process(pid: u32, name: &str, cpu_percent: f32) -> ProcessSample {
        ProcessSample {
            pid,
            name: name.to_string(),
            cpu_percent,
            hot_for: Duration::ZERO,
            quiet_for: Duration::ZERO,
            user_facing: false,
            made_by_apple: true,
        }
    }

    fn snapshot(memory: Stress, thermal: Stress, processes: Vec<ProcessSample>) -> Snapshot {
        Snapshot {
            memory,
            repacked_megabytes_per_second: 0.0,
            swapping: Swapping::default(),
            thermal,
            saturated_clusters: Vec::new(),
            processes,
        }
    }

    fn pids(plan: BTreeMap<u32, Throttle>) -> Vec<u32> {
        plan.into_keys().collect()
    }

    #[test]
    fn calm_system_leaves_busy_background_services_alone() {
        let snapshot = snapshot(Stress::Calm, Stress::Calm, vec![process(10, "mediaanalysisd", 95.0)]);
        assert!(decide(&snapshot, &Config::default(), &BTreeSet::new()).is_empty());
    }

    #[test]
    fn heat_slows_busy_background_services() {
        let processes = vec![process(10, "mediaanalysisd", 95.0), process(11, "cloudd", 2.0)];
        let snapshot = snapshot(Stress::Calm, Stress::Elevated, processes);
        assert_eq!(pids(decide(&snapshot, &Config::default(), &BTreeSet::new())), vec![10]);
    }

    #[test]
    fn anything_owned_by_an_app_is_never_touched_even_at_critical() {
        let mut build = process(10, "node", 400.0);
        build.user_facing = true;
        build.hot_for = Duration::from_secs(3600);
        let snapshot = snapshot(Stress::Critical, Stress::Critical, vec![build]);
        assert!(decide(&snapshot, &Config::default(), &BTreeSet::from([10])).is_empty());
    }

    #[test]
    fn third_party_services_like_a_homebrew_database_are_never_touched() {
        let mut database = process(10, "postgres", 95.0);
        database.made_by_apple = false;
        database.hot_for = Duration::from_secs(3600);
        let snapshot = snapshot(Stress::Critical, Stress::Critical, vec![database]);
        assert!(decide(&snapshot, &Config::default(), &BTreeSet::from([10])).is_empty());
    }

    #[test]
    fn protected_services_are_never_touched() {
        let snapshot = snapshot(Stress::Critical, Stress::Critical, vec![process(10, "sharingd", 90.0)]);
        assert!(decide(&snapshot, &Config::default(), &BTreeSet::new()).is_empty());
    }

    #[test]
    fn stuck_background_service_is_slowed_even_when_calm() {
        let config = Config::default();
        let mut stuck = process(10, "photoanalysisd", 99.0);
        stuck.hot_for = config.runaway_duration();
        let snapshot = snapshot(Stress::Calm, Stress::Calm, vec![stuck]);
        assert_eq!(pids(decide(&snapshot, &config, &BTreeSet::new())), vec![10]);
    }

    #[test]
    fn slowed_service_is_released_only_after_quiet_for_the_whole_window() {
        let config = Config::default();
        let throttled = BTreeSet::from([10]);
        let mut briefly_quiet = process(10, "photoanalysisd", 20.0);
        briefly_quiet.quiet_for = Duration::from_secs(5);
        let mut quiet_long_enough = briefly_quiet.clone();
        quiet_long_enough.quiet_for = config.runaway_duration();
        let calm = |process| snapshot(Stress::Calm, Stress::Calm, vec![process]);
        assert_eq!(pids(decide(&calm(briefly_quiet), &config, &throttled)), vec![10]);
        assert!(decide(&calm(quiet_long_enough), &config, &throttled).is_empty());
    }

    #[test]
    fn slowed_service_stays_slowed_while_pressure_lasts() {
        let throttled = BTreeSet::from([10]);
        let mut idle = process(10, "cloudd", 1.0);
        idle.quiet_for = Config::default().runaway_duration();
        let elevated = snapshot(Stress::Elevated, Stress::Calm, vec![idle.clone()]);
        let calm = snapshot(Stress::Calm, Stress::Calm, vec![idle]);
        assert_eq!(pids(decide(&elevated, &Config::default(), &throttled)), vec![10]);
        assert!(decide(&calm, &Config::default(), &throttled).is_empty());
    }

    #[test]
    fn runaway_reason_says_how_long_the_service_was_busy() {
        let mut stuck = process(10, "photoanalysisd", 99.4);
        stuck.hot_for = Duration::from_secs(190);
        let plan = decide(&snapshot(Stress::Calm, Stress::Calm, vec![stuck]), &Config::default(), &BTreeSet::new());
        assert_eq!(plan[&10].to_string(), "at 99% CPU, busy for 3m 10s straight");
    }

    #[test]
    fn stress_reason_names_every_active_cause() {
        let processes = vec![process(10, "mediaanalysisd", 45.0)];
        let both = snapshot(Stress::Elevated, Stress::Critical, processes.clone());
        let heat_only = snapshot(Stress::Calm, Stress::Elevated, processes);
        let describe = |snapshot| decide(&snapshot, &Config::default(), &BTreeSet::new())[&10].to_string();
        assert_eq!(describe(both), "at 45% CPU, memory pressure elevated and thermal state critical");
        assert_eq!(describe(heat_only), "at 45% CPU, thermal state elevated");
    }

    fn saturated(processes: Vec<ProcessSample>) -> Snapshot {
        Snapshot { saturated_clusters: vec!["efficiency".to_string()], ..snapshot(Stress::Calm, Stress::Calm, processes) }
    }

    #[test]
    fn saturated_cpu_slows_busy_background_services_at_once() {
        let processes = vec![process(10, "mediaanalysisd", 60.0), process(11, "cloudd", 2.0)];
        let plan = decide(&saturated(processes), &Config::default(), &BTreeSet::new());
        assert_eq!(plan[&10].to_string(), "at 60% CPU, efficiency cores saturated");
        assert_eq!(pids(plan), vec![10]);
    }

    #[test]
    fn critical_stress_also_slows_smaller_background_services() {
        let processes = vec![process(10, "mds_stores", 15.0), process(11, "cloudd", 2.0)];
        let elevated = snapshot(Stress::Elevated, Stress::Calm, processes.clone());
        let critical = snapshot(Stress::Critical, Stress::Calm, processes);
        assert!(decide(&elevated, &Config::default(), &BTreeSet::new()).is_empty());
        assert_eq!(pids(decide(&critical, &Config::default(), &BTreeSet::new())), vec![10]);
    }

    #[test]
    fn scans_often_while_a_background_service_runs_hot_on_a_calm_mac() {
        let config = Config::default();
        let calm = |processes| snapshot(Stress::Calm, Stress::Calm, processes);
        let mut app_build = process(12, "node", 400.0);
        app_build.user_facing = true;
        let quiet = calm(vec![process(10, "cloudd", 40.0), app_build]);
        let hot = calm(vec![process(10, "photoanalysisd", 95.0)]);
        assert_eq!(scan_interval(&quiet, &config, &BTreeSet::new()), Duration::from_secs(30));
        assert_eq!(scan_interval(&hot, &config, &BTreeSet::new()), Duration::from_secs(5));
        assert_eq!(scan_interval(&hot, &config, &BTreeSet::from([10])), Duration::from_secs(30));
    }

    #[test]
    fn scans_often_under_any_stress() {
        assert_eq!(scan_interval(&saturated(vec![]), &Config::default(), &BTreeSet::new()), Duration::from_secs(5));
    }
}
