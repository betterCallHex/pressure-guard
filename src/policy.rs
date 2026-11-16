use crate::config::Config;
use crate::system::{ProcessSample, Snapshot, Stress};
use std::collections::{BTreeMap, BTreeSet};

pub fn decide(snapshot: &Snapshot, config: &Config, throttled: &BTreeSet<u32>) -> BTreeMap<u32, String> {
    let stress = snapshot.stress();
    snapshot
        .processes
        .iter()
        .filter(|process| !process.user_facing && !config.is_protected(&process.name))
        .filter(|process| should_throttle(process, stress, config, throttled))
        .map(|process| (process.pid, process.name.clone()))
        .collect()
}

fn should_throttle(process: &ProcessSample, stress: Stress, config: &Config, throttled: &BTreeSet<u32>) -> bool {
    let window = config.runaway_duration();
    if process.hot_for >= window {
        return true;
    }
    if throttled.contains(&process.pid) {
        return stress != Stress::Calm || process.quiet_for < window;
    }
    stress != Stress::Calm && process.cpu_percent >= config.hog_cpu_percent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn process(pid: u32, name: &str, cpu_percent: f32) -> ProcessSample {
        ProcessSample { pid, name: name.to_string(), cpu_percent, hot_for: Duration::ZERO, quiet_for: Duration::ZERO, user_facing: false }
    }

    fn snapshot(memory: Stress, thermal: Stress, processes: Vec<ProcessSample>) -> Snapshot {
        Snapshot { memory, thermal, processes }
    }

    fn pids(plan: BTreeMap<u32, String>) -> Vec<u32> {
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
}
