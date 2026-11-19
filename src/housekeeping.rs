use crate::config::Config;
use crate::system::{self, AgentWork, Snapshot, Stress};
use crate::{friendly_duration, log};
use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

pub struct Housekeeper {
    dry_run: bool,
    yielding_agents: BTreeSet<u32>,
    mode: Mode,
    calm_since: Option<Instant>,
    last_strained: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    FullSpeed,
    Background { since: Instant },
}

impl Housekeeper {
    pub fn new(dry_run: bool) -> Housekeeper {
        Housekeeper { dry_run, yielding_agents: BTreeSet::new(), mode: Mode::FullSpeed, calm_since: None, last_strained: None }
    }

    pub fn tend(
        &mut self,
        work: &AgentWork,
        snapshot: &Snapshot,
        idle: &HashSet<u32>,
        taking_their_turn: &BTreeSet<u32>,
        config: &Config,
    ) {
        self.yield_to_user(work, config);
        let pressure = snapshot.stress();
        let now = Instant::now();
        self.calm_since = if pressure == Stress::Calm { Some(self.calm_since.unwrap_or(now)) } else { None };
        let thrashing = config.is_thrashing(snapshot.repacked_megabytes_per_second);
        if snapshot.thermal != Stress::Calm || !snapshot.saturated_clusters.is_empty() || thrashing {
            self.last_strained = Some(now);
        }
        let all_work_yields = self.last_strained.is_some_and(|at| now - at < config.agent_release_after());
        let next = next_mode(self.mode, pressure, self.calm_since.map(|since| now - since), config.agent_release_after(), now);
        let background = match next {
            Mode::FullSpeed => Vec::new(),
            Mode::Background { .. } => to_background(work, all_work_yields, idle, taking_their_turn),
        };
        let full_speed: Vec<u32> = work.everything.iter().filter(|pid| !background.contains(pid)).copied().collect();
        let moved = self.set_background(&background, true);
        let restored = self.set_background(&full_speed, false);
        match (self.mode, next) {
            (Mode::FullSpeed, Mode::Background { .. }) if !all_work_yields => log(&format!(
                "pressure {}: agent processes idle for {} now run in the background, so their memory is reclaimed \
                 before yours; busy agent work keeps every core",
                pressure.label(),
                friendly_duration(config.agent_idle()),
            )),
            (Mode::FullSpeed, Mode::Background { .. }) => log(&format!(
                "pressure {}: {moved} agent processes now run in the background on the efficiency cores, and their \
                 memory is reclaimed before yours",
                pressure.label(),
            )),
            (Mode::Background { since }, Mode::FullSpeed) => log(&format!(
                "calm for {}: {restored} agent processes back to full speed after {} in the background",
                friendly_duration(config.agent_release_after()),
                friendly_duration(now - since),
            )),
            _ => {}
        }
        self.mode = next;
    }

    pub fn restore_full_speed(&mut self, work: &AgentWork) {
        self.set_background(&work.everything, false);
        self.mode = Mode::FullSpeed;
    }

    fn yield_to_user(&mut self, work: &AgentWork, config: &Config) {
        if config.agent_nice <= 0 {
            return;
        }
        for (pid, name) in work.agents.iter().filter(|(pid, _)| !self.yielding_agents.contains(pid)) {
            log(&format!("{name} agent ({pid}) and everything it starts now yield the CPU to you (nice {})", config.agent_nice));
        }
        self.yielding_agents = work.agents.iter().map(|(pid, _)| *pid).collect();
        if self.dry_run {
            return;
        }
        for pid in &work.members {
            if system::niceness(*pid).is_some_and(|nice| nice < config.agent_nice) {
                let _ = system::set_niceness(*pid, config.agent_nice);
            }
        }
    }

    fn set_background(&self, pids: &[u32], background: bool) -> usize {
        let changing: Vec<u32> = pids.iter().copied().filter(|pid| system::is_background(*pid) != background).collect();
        if !self.dry_run {
            for pid in &changing {
                let _ = system::set_background(*pid, background);
            }
        }
        changing.len()
    }
}

fn to_background(
    work: &AgentWork,
    all_work_yields: bool,
    idle: &HashSet<u32>,
    taking_their_turn: &BTreeSet<u32>,
) -> Vec<u32> {
    if all_work_yields {
        return work.everything.iter().filter(|pid| !taking_their_turn.contains(pid)).copied().collect();
    }
    work.everything.iter().filter(|pid| idle.contains(pid) && !work.starters.contains(pid)).copied().collect()
}

fn next_mode(current: Mode, pressure: Stress, calm_for: Option<Duration>, release_after: Duration, now: Instant) -> Mode {
    match current {
        Mode::FullSpeed if pressure != Stress::Calm => Mode::Background { since: now },
        Mode::Background { .. } if calm_for.is_some_and(|calm| calm >= release_after) => Mode::FullSpeed,
        unchanged => unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: Duration = Duration::from_secs(60);

    #[test]
    fn plenty_of_memory_keeps_agents_at_full_speed() {
        let now = Instant::now();
        assert_eq!(next_mode(Mode::FullSpeed, Stress::Calm, Some(Duration::ZERO), RELEASE, now), Mode::FullSpeed);
    }

    #[test]
    fn the_first_sign_of_pressure_moves_agent_work_to_the_background() {
        let now = Instant::now();
        assert_eq!(next_mode(Mode::FullSpeed, Stress::Elevated, None, RELEASE, now), Mode::Background { since: now });
        assert_eq!(next_mode(Mode::FullSpeed, Stress::Critical, None, RELEASE, now), Mode::Background { since: now });
    }

    fn work(pids: &[u32]) -> AgentWork {
        AgentWork { everything: pids.to_vec(), ..AgentWork::default() }
    }

    #[test]
    fn under_memory_pressure_only_idle_agent_work_goes_to_the_background() {
        let idle = HashSet::from([11, 13]);
        assert_eq!(to_background(&work(&[10, 11, 12, 13]), false, &idle, &BTreeSet::new()), vec![11, 13]);
    }

    #[test]
    fn processes_that_start_work_stay_out_so_what_they_start_is_not_born_in_the_background() {
        let work = AgentWork { starters: BTreeSet::from([10, 11]), everything: vec![10, 11, 12], ..AgentWork::default() };
        assert_eq!(to_background(&work, false, &HashSet::from([10, 11, 12]), &BTreeSet::new()), vec![12]);
    }

    #[test]
    fn while_heat_or_a_saturated_cluster_strains_the_cores_busy_agent_work_also_goes_to_the_efficiency_cores() {
        let idle = HashSet::from([11]);
        assert_eq!(to_background(&work(&[10, 11, 12]), true, &idle, &BTreeSet::new()), vec![10, 11, 12]);
    }

    #[test]
    fn a_job_whose_turn_it_is_stays_out_of_the_background_so_macos_leaves_the_memory_it_is_using_alone() {
        let taking_their_turn = BTreeSet::from([12]);
        assert_eq!(to_background(&work(&[10, 11, 12]), true, &HashSet::new(), &taking_their_turn), vec![10, 11]);
    }

    #[test]
    fn agents_return_to_full_speed_only_after_a_calm_minute() {
        let since = Instant::now();
        let background = Mode::Background { since };
        let later = since + Duration::from_secs(300);
        assert_eq!(next_mode(background, Stress::Calm, Some(Duration::from_secs(20)), RELEASE, later), background);
        assert_eq!(next_mode(background, Stress::Elevated, None, RELEASE, later), background);
        assert_eq!(next_mode(background, Stress::Calm, Some(RELEASE), RELEASE, later), Mode::FullSpeed);
    }

    #[test]
    fn a_saturated_cpu_alone_moves_agent_work_to_the_efficiency_cores() {
        let mut housekeeper = Housekeeper::new(true);
        let saturated = Snapshot { saturated_clusters: vec!["performance".to_string()], ..calm() };
        housekeeper.tend(&AgentWork::default(), &saturated, &HashSet::new(), &BTreeSet::new(), &Config::default());
        assert!(matches!(housekeeper.mode, Mode::Background { .. }));
        assert!(housekeeper.last_strained.is_some(), "busy agent work goes to the efficiency cores too");
    }

    #[test]
    fn thrashing_memory_moves_busy_agent_work_to_the_background_too() {
        let mut housekeeper = Housekeeper::new(true);
        let thrashing = Snapshot { memory: Stress::Elevated, repacked_megabytes_per_second: 1609.0, ..calm() };
        housekeeper.tend(&AgentWork::default(), &thrashing, &HashSet::new(), &BTreeSet::new(), &Config::default());
        assert!(matches!(housekeeper.mode, Mode::Background { .. }));
        assert!(housekeeper.last_strained.is_some(), "busy agent memory is reclaimed before the user's");
    }

    #[test]
    fn tight_memory_that_is_not_thrashing_leaves_busy_agent_work_at_full_speed() {
        let mut housekeeper = Housekeeper::new(true);
        let tight = Snapshot { memory: Stress::Elevated, repacked_megabytes_per_second: 40.0, ..calm() };
        housekeeper.tend(&AgentWork::default(), &tight, &HashSet::new(), &BTreeSet::new(), &Config::default());
        assert!(matches!(housekeeper.mode, Mode::Background { .. }));
        assert!(housekeeper.last_strained.is_none());
    }

    fn calm() -> Snapshot {
        Snapshot {
            memory: Stress::Calm,
            repacked_megabytes_per_second: 0.0,
            thermal: Stress::Calm,
            saturated_clusters: Vec::new(),
            processes: Vec::new(),
        }
    }
}
