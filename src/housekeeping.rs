use crate::config::{Config, home};
use crate::system::{self, AgentWork, OrphanedJob};
use crate::{friendly_duration, log};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::time::{Duration, Instant};

const ARGUMENTS_SHOWN: usize = 6;

pub struct Housekeeper {
    dry_run: bool,
    yielding_agents: BTreeSet<u32>,
    orphaned_since: HashMap<(u32, u64), Instant>,
}

impl Housekeeper {
    pub fn new(dry_run: bool) -> Housekeeper {
        Housekeeper { dry_run, yielding_agents: BTreeSet::new(), orphaned_since: HashMap::new() }
    }

    pub fn tend(&mut self, work: &AgentWork, config: &Config) {
        self.yield_to_user(work, config);
        self.clear_abandoned_jobs(work, config, Instant::now());
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

    fn clear_abandoned_jobs(&mut self, work: &AgentWork, config: &Config, now: Instant) {
        let mut still_orphaned = HashMap::new();
        for job in &work.orphans {
            let key = (job.pid, job.started);
            let since = self.orphaned_since.get(&key).copied().unwrap_or(now);
            let abandoned_for = now - since;
            if abandoned_for < config.abandoned_after() {
                still_orphaned.insert(key, since);
                continue;
            }
            self.end(job, abandoned_for);
        }
        self.orphaned_since = still_orphaned;
    }

    fn end(&self, job: &OrphanedJob, abandoned_for: Duration) {
        let description = describe(job, abandoned_for);
        if self.dry_run {
            log(&format!("[dry run] would end {description}"));
            return;
        }
        let result: io::Result<()> = job.tree.iter().rev().map(|pid| system::end_process(*pid)).fold(Ok(()), Result::and);
        match result {
            Ok(()) => log(&format!("ended {description}")),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => log(&format!("ended {description}")),
            Err(error) => log(&format!("could not end {description}: {error}")),
        }
    }
}

fn describe(job: &OrphanedJob, abandoned_for: Duration) -> String {
    let command: Vec<&str> = job
        .arguments
        .iter()
        .take(ARGUMENTS_SHOWN)
        .map(|argument| argument.rsplit('/').next().unwrap_or_default())
        .collect();
    let directory = job.directory.as_ref().map_or_else(String::new, |directory| {
        let shown = directory.strip_prefix(home()).map_or_else(|_| directory.display().to_string(), |relative| {
            format!("~/{}", relative.display())
        });
        format!(" in {shown}")
    });
    format!(
        "abandoned job `{}` ({}, {} MB){directory}: no agent has been waiting on it for {}",
        command.join(" "),
        job.pid,
        job.bytes >> 20,
        friendly_duration(abandoned_for),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn job(pid: u32) -> OrphanedJob {
        OrphanedJob {
            pid,
            started: 7,
            arguments: ["node", "/Users/a/.nvm/bin/pnpm", "install", "--frozen-lockfile"].map(String::from).to_vec(),
            directory: Some(PathBuf::from("/work/shop")),
            tree: vec![pid],
            bytes: 240 << 20,
        }
    }

    fn work(orphans: Vec<OrphanedJob>) -> AgentWork {
        AgentWork { orphans, ..AgentWork::default() }
    }

    #[test]
    fn an_orphaned_job_gets_the_whole_grace_period_before_it_is_ended() {
        let config = Config::default();
        let mut keeper = Housekeeper::new(true);
        let start = Instant::now();
        keeper.clear_abandoned_jobs(&work(vec![job(10)]), &config, start);
        keeper.clear_abandoned_jobs(&work(vec![job(10)]), &config, start + config.abandoned_after() / 2);
        assert_eq!(keeper.orphaned_since.get(&(10, 7)), Some(&start));
        keeper.clear_abandoned_jobs(&work(vec![job(10)]), &config, start + config.abandoned_after());
        assert!(keeper.orphaned_since.is_empty());
    }

    #[test]
    fn a_job_that_finished_or_was_replaced_starts_a_fresh_grace_period() {
        let config = Config::default();
        let mut keeper = Housekeeper::new(true);
        let start = Instant::now();
        keeper.clear_abandoned_jobs(&work(vec![job(10)]), &config, start);
        keeper.clear_abandoned_jobs(&work(vec![]), &config, start + Duration::from_secs(1));
        let later = start + config.abandoned_after();
        keeper.clear_abandoned_jobs(&work(vec![job(10)]), &config, later);
        assert_eq!(keeper.orphaned_since.get(&(10, 7)), Some(&later));
    }

    #[test]
    fn the_log_says_what_was_ended_where_and_why() {
        let line = describe(&job(42), Duration::from_secs(300));
        assert_eq!(
            line,
            "abandoned job `node pnpm install --frozen-lockfile` (42, 240 MB) in /work/shop: no agent has been waiting on it for 5m"
        );
    }
}
