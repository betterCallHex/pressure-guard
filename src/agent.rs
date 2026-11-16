use crate::config::Config;
use crate::enforcer::Enforcer;
use crate::log;
use crate::policy;
use crate::system::{Probe, Snapshot, Stress};
use dispatch2::{DispatchRetained, DispatchSource, DispatchTime};
use std::time::Duration;

pub struct Agent {
    config: Config,
    probe: Probe,
    enforcer: Enforcer,
    condition: String,
    timer: Option<DispatchRetained<DispatchSource>>,
    scan_interval: Option<Duration>,
}

impl Agent {
    pub fn new(config: Config, probe: Probe, enforcer: Enforcer) -> Agent {
        Agent { config, probe, enforcer, condition: String::new(), timer: None, scan_interval: None }
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
        let snapshot = self.probe.sample(&self.config);
        self.log_condition(&snapshot);
        let plan = policy::decide(&snapshot, &self.config, &self.enforcer.throttled());
        self.enforcer.apply(&plan);
        self.reschedule(snapshot.stress());
    }

    pub fn shut_down(&mut self) {
        self.enforcer.release_all();
        log("stopped, every process restored");
    }

    fn log_condition(&mut self, snapshot: &Snapshot) {
        let condition = format!("memory pressure {}, thermal {}", label(snapshot.memory), label(snapshot.thermal));
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

fn label(stress: Stress) -> &'static str {
    match stress {
        Stress::Calm => "normal",
        Stress::Elevated => "elevated",
        Stress::Critical => "critical",
    }
}
