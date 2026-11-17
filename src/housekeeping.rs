use crate::config::Config;
use crate::system::{self, AgentWork, Stress};
use crate::{friendly_duration, log};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

pub struct Housekeeper {
    dry_run: bool,
    yielding_agents: BTreeSet<u32>,
    mode: Mode,
    calm_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    FullSpeed,
    Background { since: Instant },
}

impl Housekeeper {
    pub fn new(dry_run: bool) -> Housekeeper {
        Housekeeper { dry_run, yielding_agents: BTreeSet::new(), mode: Mode::FullSpeed, calm_since: None }
    }

    pub fn tend(&mut self, work: &AgentWork, pressure: Stress, config: &Config) {
        self.yield_to_user(work, config);
        let now = Instant::now();
        self.calm_since = if pressure == Stress::Calm { Some(self.calm_since.unwrap_or(now)) } else { None };
        let next = next_mode(self.mode, pressure, self.calm_since.map(|since| now - since), config.agent_release_after(), now);
        match (self.mode, next) {
            (Mode::FullSpeed, Mode::Background { .. }) => {
                let moved = self.set_background(&work.everything, true);
                log(&format!(
                    "pressure {}: {moved} agent processes now run in the background, so their memory is reclaimed \
                     before yours and they use the efficiency cores; nothing is paused",
                    pressure.label(),
                ));
            }
            (Mode::Background { .. }, Mode::Background { .. }) => {
                self.set_background(&work.everything, true);
            }
            (Mode::Background { since }, Mode::FullSpeed) => {
                let restored = self.set_background(&work.everything, false);
                log(&format!(
                    "calm for {}: {restored} agent processes back to full speed after {} in the background",
                    friendly_duration(config.agent_release_after()),
                    friendly_duration(now - since),
                ));
            }
            (Mode::FullSpeed, Mode::FullSpeed) => {
                self.set_background(&work.everything, false);
            }
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

    #[test]
    fn agents_return_to_full_speed_only_after_a_calm_minute() {
        let since = Instant::now();
        let background = Mode::Background { since };
        let later = since + Duration::from_secs(300);
        assert_eq!(next_mode(background, Stress::Calm, Some(Duration::from_secs(20)), RELEASE, later), background);
        assert_eq!(next_mode(background, Stress::Elevated, None, RELEASE, later), background);
        assert_eq!(next_mode(background, Stress::Calm, Some(RELEASE), RELEASE, later), Mode::FullSpeed);
    }
}
