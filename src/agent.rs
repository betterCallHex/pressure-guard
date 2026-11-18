use crate::config::Config;
use crate::enforcer::Enforcer;
use crate::housekeeping::Housekeeper;
use crate::log;
use crate::policy;
use crate::system::{self, Probe, Snapshot, Stress};
use dispatch2::{DispatchRetained, DispatchSource, DispatchTime};
use std::time::{Duration, Instant, SystemTime};

const MIN_SCAN_GAP: Duration = Duration::from_secs(1);

pub struct Agent {
    config: Config,
    config_modified: Option<SystemTime>,
    probe: Probe,
    enforcer: Enforcer,
    housekeeper: Housekeeper,
    condition: String,
    timer: Option<DispatchRetained<DispatchSource>>,
    scan_interval: Option<Duration>,
    timer_off_beat: bool,
    last_scan: Option<Instant>,
}

impl Agent {
    pub fn new(config: Config, probe: Probe, enforcer: Enforcer, housekeeper: Housekeeper) -> Agent {
        Agent {
            config,
            config_modified: Config::modified(),
            probe,
            enforcer,
            housekeeper,
            condition: String::new(),
            timer: None,
            scan_interval: None,
            timer_off_beat: false,
            last_scan: None,
        }
    }

    pub fn attach_timer(&mut self, timer: DispatchRetained<DispatchSource>) {
        self.timer = Some(timer);
        self.scan_interval = None;
    }

    pub fn start(&mut self) {
        self.enforcer.recover_from_previous_run();
        self.scan();
        self.scan_after(MIN_SCAN_GAP);
    }

    pub fn wake(&mut self, reason: &str) {
        log(&format!("woken by {reason}"));
        let delay = delay_before_scan(self.last_scan.map(|at| at.elapsed()));
        if delay.is_zero() {
            self.scan();
            return;
        }
        self.scan_after(delay);
    }

    pub fn scan(&mut self) {
        self.last_scan = Some(Instant::now());
        if Config::modified() != self.config_modified {
            self.reload_config();
        }
        let throttled = self.enforcer.throttled();
        let snapshot = self.probe.sample(&self.config, &throttled);
        self.log_condition(&snapshot);
        let plan = policy::decide(&snapshot, &self.config, &throttled);
        self.enforcer.apply(&plan);
        let idle = self.probe.idle_pids(&self.config);
        self.housekeeper.tend(&system::agent_work(&self.config), &snapshot, &idle, &self.config);
        self.reschedule(policy::scan_interval(&snapshot, &self.config, &self.enforcer.throttled()));
    }

    pub fn reload_config(&mut self) {
        self.config_modified = Config::modified();
        match Config::load() {
            Ok(config) => {
                self.config = config;
                self.scan_interval = None;
                log(&format!("config reloaded from {}", Config::path().display()));
            }
            Err(error) => log(&format!("kept the previous config, invalid config {error}")),
        }
    }

    pub fn shut_down(&mut self) {
        self.enforcer.release_all();
        self.housekeeper.restore_full_speed(&system::agent_work(&self.config));
        log("stopped, every process restored");
    }

    fn log_condition(&mut self, snapshot: &Snapshot) {
        let cpu = match snapshot.saturated_clusters.as_slice() {
            [] => "CPU normal".to_string(),
            clusters => system::describe_saturation(clusters),
        };
        let condition =
            format!("memory pressure {}, thermal {}, {cpu}", snapshot.memory.label(), snapshot.thermal.label());
        if condition == self.condition {
            return;
        }
        match system::free_memory_percent() {
            Some(free) => log(&format!("{condition}, {free}% memory free")),
            None => log(&condition),
        }
        self.condition = condition;
        if snapshot.memory == Stress::Critical {
            log_biggest_memory_users();
        }
    }

    fn reschedule(&mut self, interval: Duration) {
        let changed = self.scan_interval != Some(interval);
        if !changed && !self.timer_off_beat {
            return;
        }
        self.arm_timer(interval, interval);
        self.scan_interval = Some(interval);
        self.timer_off_beat = false;
        if changed {
            log(&format!("next scans every {}s", interval.as_secs()));
        }
    }

    fn scan_after(&mut self, delay: Duration) {
        self.arm_timer(delay, self.scan_interval.unwrap_or(MIN_SCAN_GAP));
        self.timer_off_beat = true;
    }

    fn arm_timer(&self, delay: Duration, interval: Duration) {
        let Some(timer) = &self.timer else {
            return;
        };
        let nanoseconds = u64::try_from(interval.as_nanos()).unwrap_or(u64::MAX);
        let leeway = u64::try_from(delay.min(interval).as_nanos() / 10).unwrap_or(u64::MAX);
        let start = DispatchTime::try_from(delay).unwrap_or(DispatchTime::NOW);
        timer.set_timer(start, nanoseconds, leeway);
    }
}

fn log_biggest_memory_users() {
    const PROJECTS_LOGGED: usize = 5;
    for project in system::memory_by_project().iter().take(PROJECTS_LOGGED) {
        log(&format!("  using memory: {project}"));
    }
}

fn delay_before_scan(since_last_scan: Option<Duration>) -> Duration {
    since_last_scan.map_or(Duration::ZERO, |elapsed| MIN_SCAN_GAP.saturating_sub(elapsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_wake_ups_is_folded_into_one_scan_a_second() {
        assert_eq!(delay_before_scan(None), Duration::ZERO);
        assert_eq!(delay_before_scan(Some(Duration::from_millis(200))), Duration::from_millis(800));
        assert_eq!(delay_before_scan(Some(Duration::from_secs(5))), Duration::ZERO);
    }
}
