use crate::config::Config;
use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum Stress {
    Calm,
    Elevated,
    Critical,
}

#[derive(Debug, Clone)]
pub struct ProcessSample {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub hot_for: Duration,
    pub quiet_for: Duration,
    pub user_facing: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub memory: Stress,
    pub thermal: Stress,
    pub processes: Vec<ProcessSample>,
}

impl Snapshot {
    pub fn stress(&self) -> Stress {
        self.memory.max(self.thermal)
    }
}

pub struct Probe {
    system: System,
    streaks: HashMap<u32, Streak>,
    user_id: u32,
    own_pid: u32,
}

#[derive(Default, Clone, Copy)]
struct Streak {
    hot_since: Option<Instant>,
    quiet_since: Option<Instant>,
}

impl Streak {
    fn extend(self, cpu_percent: f32, now: Instant, config: &Config) -> Streak {
        let hot = cpu_percent >= config.runaway_cpu_percent;
        let quiet = cpu_percent < config.hog_cpu_percent;
        Streak {
            hot_since: hot.then(|| self.hot_since.unwrap_or(now)),
            quiet_since: quiet.then(|| self.quiet_since.unwrap_or(now)),
        }
    }

    fn hot_for(&self, now: Instant) -> Duration {
        self.hot_since.map_or(Duration::ZERO, |since| now - since)
    }

    fn quiet_for(&self, now: Instant) -> Duration {
        self.quiet_since.map_or(Duration::ZERO, |since| now - since)
    }
}

unsafe extern "C" {
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
}

impl Probe {
    pub fn new() -> Result<Probe, sysinfo::Error> {
        Ok(Probe {
            system: System::new()?,
            streaks: HashMap::new(),
            user_id: unsafe { libc::getuid() },
            own_pid: std::process::id(),
        })
    }

    pub fn sample(&mut self, config: &Config) -> Snapshot {
        let refresh = ProcessRefreshKind::nothing().with_cpu().with_user(UpdateKind::OnlyIfNotSet);
        self.system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let apps = app_pids();
        let processes = self.own_processes(config, &apps);
        Snapshot { memory: memory_pressure(), thermal: thermal_state(), processes }
    }

    fn own_processes(&mut self, config: &Config, apps: &HashSet<u32>) -> Vec<ProcessSample> {
        let now = Instant::now();
        let mut samples = Vec::new();
        let mut streaks = HashMap::new();
        for (pid, process) in self.system.processes() {
            let owned = process.user_id().is_some_and(|uid| **uid == self.user_id);
            if !owned || pid.as_u32() == self.own_pid {
                continue;
            }
            let cpu_percent = process.cpu_usage();
            let streak = self.streaks.get(&pid.as_u32()).copied().unwrap_or_default().extend(cpu_percent, now, config);
            streaks.insert(pid.as_u32(), streak);
            samples.push(ProcessSample {
                pid: pid.as_u32(),
                name: process.name().to_string_lossy().into_owned(),
                cpu_percent,
                hot_for: streak.hot_for(now),
                quiet_for: streak.quiet_for(now),
                user_facing: self.belongs_to_app(*pid, apps),
            });
        }
        self.streaks = streaks;
        samples
    }

    fn belongs_to_app(&self, pid: Pid, apps: &HashSet<u32>) -> bool {
        let mut current = Some(pid);
        while let Some(pid) = current {
            if apps.contains(&pid.as_u32()) || apps.contains(&responsible_pid(pid)) {
                return true;
            }
            current = self.system.process(pid).and_then(|process| process.parent());
        }
        false
    }
}

fn responsible_pid(pid: Pid) -> u32 {
    let responsible = unsafe { responsibility_get_pid_responsible_for_pid(pid.as_u32() as libc::pid_t) };
    u32::try_from(responsible).unwrap_or(0)
}

fn app_pids() -> HashSet<u32> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|app| app.activationPolicy() != NSApplicationActivationPolicy::Prohibited)
        .filter_map(|app| u32::try_from(app.processIdentifier()).ok())
        .collect()
}

fn memory_pressure() -> Stress {
    const WARNING: i32 = 2;
    const CRITICAL: i32 = 4;
    let mut level: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            (&mut level as *mut i32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    match (status, level) {
        (0, CRITICAL) => Stress::Critical,
        (0, WARNING) => Stress::Elevated,
        _ => Stress::Calm,
    }
}

fn thermal_state() -> Stress {
    let state = NSProcessInfo::processInfo().thermalState();
    if state == NSProcessInfoThermalState::Critical {
        Stress::Critical
    } else if state == NSProcessInfoThermalState::Serious {
        Stress::Elevated
    } else {
        Stress::Calm
    }
}
