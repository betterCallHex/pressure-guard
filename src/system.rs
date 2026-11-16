use crate::config::Config;
use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_ANCESTRY: usize = 64;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum Stress {
    Calm,
    Elevated,
    Critical,
}

impl Stress {
    pub fn label(self) -> &'static str {
        match self {
            Stress::Calm => "normal",
            Stress::Elevated => "elevated",
            Stress::Critical => "critical",
        }
    }
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
    pub cpu: Stress,
    pub processes: Vec<ProcessSample>,
}

impl Snapshot {
    pub fn stress(&self) -> Stress {
        self.memory.max(self.thermal).max(self.cpu)
    }
}

pub struct Probe {
    tracked: HashMap<u32, Tracked>,
    user_id: u32,
    own_pid: u32,
    timebase: MachTimebase,
    host: libc::mach_port_t,
    cpu_ticks: Option<CpuTicks>,
}

#[derive(Clone, Copy)]
struct CpuTicks {
    busy: u64,
    total: u64,
}

#[derive(Clone, Copy)]
struct Tracked {
    started: u64,
    cpu_time: Duration,
    read_at: Instant,
    cpu_percent: f32,
    streak: Streak,
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

#[derive(Clone, Copy)]
struct ProcessEntry {
    parent: u32,
    user_id: u32,
}

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

struct CpuUsage {
    started: u64,
    ticks: u64,
}

unsafe extern "C" {
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
    fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
    fn mach_host_self() -> libc::mach_port_t;
}

impl Probe {
    pub fn new() -> Probe {
        let mut timebase = MachTimebase { numer: 1, denom: 1 };
        unsafe { mach_timebase_info(&mut timebase) };
        Probe {
            tracked: HashMap::new(),
            user_id: unsafe { libc::getuid() },
            own_pid: std::process::id(),
            timebase,
            host: unsafe { mach_host_self() },
            cpu_ticks: None,
        }
    }

    pub fn sample(&mut self, config: &Config, throttled: &BTreeSet<u32>) -> Snapshot {
        let now = Instant::now();
        let table = process_table();
        self.tracked = table
            .iter()
            .filter(|(pid, entry)| entry.user_id == self.user_id && **pid != self.own_pid)
            .filter_map(|(pid, _)| self.measure(*pid, now, config).map(|tracked| (*pid, tracked)))
            .collect();
        let busy: Vec<u32> = self
            .tracked
            .iter()
            .filter(|(pid, tracked)| tracked.cpu_percent >= config.lowest_trigger_cpu_percent() || throttled.contains(pid))
            .map(|(pid, _)| *pid)
            .collect();
        let apps = if busy.is_empty() { HashSet::new() } else { app_pids() };
        let processes = busy.into_iter().map(|pid| self.describe(pid, now, &table, &apps)).collect();
        Snapshot { memory: memory_pressure(), thermal: thermal_state(), cpu: self.cpu_load(config), processes }
    }

    fn cpu_load(&mut self, config: &Config) -> Stress {
        let current = host_cpu_ticks(self.host);
        let busy_percent = match (self.cpu_ticks, current) {
            (Some(previous), Some(current)) if current.total > previous.total => {
                (current.busy - previous.busy) as f64 / (current.total - previous.total) as f64 * 100.0
            }
            _ => 0.0,
        };
        self.cpu_ticks = current;
        if busy_percent as f32 >= config.busy_cpu_percent { Stress::Elevated } else { Stress::Calm }
    }

    fn measure(&self, pid: u32, now: Instant, config: &Config) -> Option<Tracked> {
        let usage = cpu_usage(pid)?;
        let cpu_time = self.ticks_to_duration(usage.ticks);
        let previous = self.tracked.get(&pid).filter(|previous| previous.started == usage.started);
        let cpu_percent = previous.map_or(0.0, |previous| {
            percent(cpu_time.saturating_sub(previous.cpu_time), now - previous.read_at)
        });
        let streak = previous.map(|previous| previous.streak).unwrap_or_default().extend(cpu_percent, now, config);
        Some(Tracked { started: usage.started, cpu_time, read_at: now, cpu_percent, streak })
    }

    fn ticks_to_duration(&self, ticks: u64) -> Duration {
        let nanoseconds = u128::from(ticks) * u128::from(self.timebase.numer) / u128::from(self.timebase.denom.max(1));
        Duration::from_nanos(u64::try_from(nanoseconds).unwrap_or(u64::MAX))
    }

    fn describe(&self, pid: u32, now: Instant, table: &HashMap<u32, ProcessEntry>, apps: &HashSet<u32>) -> ProcessSample {
        let tracked = self.tracked[&pid];
        ProcessSample {
            pid,
            name: process_name(pid),
            cpu_percent: tracked.cpu_percent,
            hot_for: tracked.streak.hot_for(now),
            quiet_for: tracked.streak.quiet_for(now),
            user_facing: belongs_to_app(pid, table, apps),
        }
    }
}

fn process_table() -> HashMap<u32, ProcessEntry> {
    all_pids().into_iter().filter_map(|pid| process_entry(pid).map(|entry| (pid, entry))).collect()
}

fn all_pids() -> Vec<u32> {
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids = vec![0 as libc::pid_t; usize::try_from(estimate).unwrap_or(0) + 256];
    let bytes = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())).unwrap_or(libc::c_int::MAX);
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(count).unwrap_or(0));
    pids.into_iter().filter_map(|pid| u32::try_from(pid).ok()).collect()
}

fn process_entry(pid: u32) -> Option<ProcessEntry> {
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let written =
        unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDT_SHORTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then_some(ProcessEntry { parent: info.pbsi_ppid, user_id: info.pbsi_uid })
}

fn cpu_usage(pid: u32) -> Option<CpuUsage> {
    let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    let status = unsafe { libc::proc_pid_rusage(pid as libc::c_int, libc::RUSAGE_INFO_V0, (&raw mut usage).cast()) };
    (status == 0).then_some(CpuUsage {
        started: usage.ri_proc_start_abstime,
        ticks: usage.ri_user_time + usage.ri_system_time,
    })
}

fn host_cpu_ticks(host: libc::mach_port_t) -> Option<CpuTicks> {
    let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    let status =
        unsafe { libc::host_statistics64(host, libc::HOST_CPU_LOAD_INFO, (&raw mut info).cast(), &mut count) };
    if status != libc::KERN_SUCCESS {
        return None;
    }
    let ticks = info.cpu_ticks.map(u64::from);
    let total: u64 = ticks.iter().sum();
    Some(CpuTicks { busy: total - ticks[libc::CPU_STATE_IDLE as usize], total })
}

fn percent(cpu_time: Duration, wall_time: Duration) -> f32 {
    if wall_time.is_zero() {
        return 0.0;
    }
    (cpu_time.as_secs_f64() / wall_time.as_secs_f64() * 100.0) as f32
}

fn app_pids() -> HashSet<u32> {
    NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|app| app.activationPolicy() != NSApplicationActivationPolicy::Prohibited)
        .filter_map(|app| u32::try_from(app.processIdentifier()).ok())
        .collect()
}

fn process_name(pid: u32) -> String {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe { libc::proc_pidpath(pid as libc::c_int, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    buffer.truncate(usize::try_from(length).unwrap_or(0));
    let path = String::from_utf8_lossy(&buffer);
    Path::new(path.as_ref()).file_name().map_or_else(|| format!("pid {pid}"), |name| name.to_string_lossy().into_owned())
}

fn belongs_to_app(pid: u32, table: &HashMap<u32, ProcessEntry>, apps: &HashSet<u32>) -> bool {
    std::iter::successors(Some(pid), |pid| table.get(pid).map(|entry| entry.parent).filter(|parent| *parent != 0))
        .take(MAX_ANCESTRY)
        .any(|pid| apps.contains(&pid) || apps.contains(&responsible_pid(pid)))
}

fn responsible_pid(pid: u32) -> u32 {
    let responsible = unsafe { responsibility_get_pid_responsible_for_pid(pid as libc::pid_t) };
    u32::try_from(responsible).unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn own_cpu_time() -> Duration {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let seconds = |time: libc::timeval| Duration::new(time.tv_sec as u64, time.tv_usec as u32 * 1000);
        seconds(usage.ru_utime) + seconds(usage.ru_stime)
    }

    fn burn_cpu(length: Duration) {
        let start = Instant::now();
        let mut counter = 0u64;
        while start.elapsed() < length {
            counter = std::hint::black_box(counter.wrapping_add(1));
        }
    }

    #[test]
    fn cpu_time_is_reported_in_real_time_units() {
        burn_cpu(Duration::from_millis(200));
        let probe = Probe::new();
        let measured = probe.ticks_to_duration(cpu_usage(std::process::id()).unwrap().ticks);
        let expected = own_cpu_time();
        let difference = measured.abs_diff(expected);
        assert!(difference < Duration::from_millis(50), "libproc says {measured:?}, getrusage says {expected:?}");
    }

    #[test]
    fn process_table_includes_parent_links_up_to_launchd() {
        let table = process_table();
        let own = table[&std::process::id()];
        assert_eq!(own.user_id, unsafe { libc::getuid() });
        assert_eq!(own.parent, std::os::unix::process::parent_id());
        assert_eq!(table[&1].parent, 0);
    }

    #[test]
    fn host_cpu_ticks_count_up_with_work() {
        let probe = Probe::new();
        let before = host_cpu_ticks(probe.host).unwrap();
        burn_cpu(Duration::from_millis(300));
        let after = host_cpu_ticks(probe.host).unwrap();
        assert!(after.busy > before.busy && after.total > before.total);
        assert!(after.busy <= after.total);
    }

    #[test]
    fn names_come_from_the_executable_path() {
        let name = process_name(std::process::id());
        assert!(name.starts_with("pressure_guard"), "{name}");
    }
}
