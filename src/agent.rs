use crate::config::Config;
use crate::enforcer::Enforcer;
use crate::log;
use crate::policy;
use crate::system::{Probe, Snapshot, Stress};
use dispatch2::{DispatchRetained, DispatchSource, DispatchTime};
use std::time::{Duration, SystemTime};

pub struct Agent {
    config: Config,
    config_modified: Option<SystemTime>,
    probe: Probe,
    enforcer: Enforcer,
    condition: String,
    timer: Option<DispatchRetained<DispatchSource>>,
    scan_interval: Option<Duration>,
}

impl Agent {
    pub fn new(config: Config, probe: Probe, enforcer: Enforcer) -> Agent {
        Agent {
            config,
            config_modified: Config::modified(),
            probe,
            enforcer,
            condition: String::new(),
            timer: None,
            scan_interval: None,
        }
    }

    pub fn attach_timer(&mut self, timer: DispatchRetained<DispatchSource>) {
        self.timer = Some(timer);
        self.scan_interval = None;
    }

    pub fn start(&mut self) {
        self.enforcer.recover_from_previous_run();
        self.scan();
    }

    pub fn scan(&mut self) {
        if Config::modified() != self.config_modified {
            self.reload_config();
        }
        let throttled = self.enforcer.throttled();
        let snapshot = self.probe.sample(&self.config, &throttled);
        self.log_condition(&snapshot);
        let plan = policy::decide(&snapshot, &self.config, &throttled);
        self.enforcer.apply(&plan);
        self.reschedule(snapshot.stress());
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
        log("stopped, every process restored");
    }

    fn log_condition(&mut self, snapshot: &Snapshot) {
        let condition = format!("memory pressure {}, thermal {}", snapshot.memory.label(), snapshot.thermal.label());
        if condition != self.condition {
            log(&condition);
            self.condition = condition;
        }
    }

    fn reschedule(&mut self, stress: Stress) {
        let interval = self.config.scan_interval(stress);
        if self.scan_interval == Some(interval) {
            return;
        }
        let Some(timer) = &self.timer else {
            return;
        };
        let nanoseconds = u64::try_from(interval.as_nanos()).unwrap_or(u64::MAX);
        let start = DispatchTime::try_from(interval).unwrap_or(DispatchTime::NOW);
        timer.set_timer(start, nanoseconds, nanoseconds / 10);
        self.scan_interval = Some(interval);
        log(&format!("next scans every {}s", interval.as_secs()));
    }
}
