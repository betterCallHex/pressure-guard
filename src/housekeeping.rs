use crate::config::Config;
use crate::log;
use crate::system::{self, AgentWork};
use std::collections::BTreeSet;

pub struct Housekeeper {
    dry_run: bool,
    yielding_agents: BTreeSet<u32>,
}

impl Housekeeper {
    pub fn new(dry_run: bool) -> Housekeeper {
        Housekeeper { dry_run, yielding_agents: BTreeSet::new() }
    }

    pub fn tend(&mut self, work: &AgentWork, config: &Config) {
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
}
