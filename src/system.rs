use crate::config::Config;
use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};
use crate::user_wait::PagingApp;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_ANCESTRY: usize = 64;
const IDLE_DISK_BYTES_PER_SECOND: f64 = 1024.0 * 1024.0;
const IDLE_PAGE_FAULTS_PER_SECOND: f64 = 64.0;
const SHORTEST_REPACK_WINDOW: Duration = Duration::from_millis(500);
const SHELLS: [&str; 8] = ["sh", "bash", "zsh", "dash", "ksh", "fish", "csh", "tcsh"];

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
    pub made_by_apple: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub memory: Stress,
    pub repacked_megabytes_per_second: f64,
    pub thermal: Stress,
    pub saturated_clusters: Vec<String>,
    pub processes: Vec<ProcessSample>,
}

impl Snapshot {
    pub fn stress(&self) -> Stress {
        self.memory.max(self.thermal).max(self.cpu())
    }

    pub fn cpu(&self) -> Stress {
        if self.saturated_clusters.is_empty() { Stress::Calm } else { Stress::Elevated }
    }
}

pub fn describe_saturation(clusters: &[String]) -> String {
    format!("{} cores saturated", clusters.join(" and "))
}

pub struct Probe {
    tracked: HashMap<u32, Tracked>,
    user_id: u32,
    own_pid: u32,
    timebase: MachTimebase,
    host: libc::mach_port_t,
    clusters: Vec<Cluster>,
    cpu_ticks: Vec<CpuTicks>,
    cluster_busy_percent: BTreeMap<String, f64>,
    compressor: Option<CompressorReading>,
    repacked_megabytes_per_second: f64,
    page_bytes: u64,
}

#[derive(Clone, Copy)]
struct CpuTicks {
    busy: u64,
    total: u64,
}

#[derive(Clone, Copy, Debug)]
struct CompressorReading {
    compressed_pages: u64,
    decompressed_pages: u64,
    read_at: Instant,
}

#[derive(Clone, Copy)]
struct Tracked {
    started: u64,
    cpu_time: Duration,
    disk_bytes: u64,
    page_faults: u32,
    page_ins: u64,
    page_ins_per_second: f64,
    responsible: u32,
    read_at: Instant,
    cpu_percent: f32,
    streak: Streak,
}

#[derive(Default, Clone, Copy)]
struct Streak {
    hot_since: Option<Instant>,
    quiet_since: Option<Instant>,
    idle_since: Option<Instant>,
}

impl Streak {
    fn extend(self, cpu_percent: f32, waits_on_disk_or_memory: bool, now: Instant, config: &Config) -> Streak {
        let hot = cpu_percent >= config.runaway_cpu_percent;
        let quiet = cpu_percent < config.hog_cpu_percent;
        let idle = cpu_percent < config.agent_idle_cpu_percent && !waits_on_disk_or_memory;
        Streak {
            hot_since: hot.then(|| self.hot_since.unwrap_or(now)),
            quiet_since: quiet.then(|| self.quiet_since.unwrap_or(now)),
            idle_since: idle.then(|| self.idle_since.unwrap_or(now)),
        }
    }

    fn hot_for(&self, now: Instant) -> Duration {
        self.hot_since.map_or(Duration::ZERO, |since| now - since)
    }

    fn quiet_for(&self, now: Instant) -> Duration {
        self.quiet_since.map_or(Duration::ZERO, |since| now - since)
    }

    fn idle_for(&self, now: Instant) -> Option<Duration> {
        self.idle_since.map(|since| now - since)
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

struct ResourceUsage {
    started: u64,
    ticks: u64,
    disk_bytes: u64,
    page_faults: u32,
    page_ins: u64,
}

unsafe extern "C" {
    fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
    fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
    fn mach_host_self() -> libc::mach_port_t;
    static mach_task_self_: libc::mach_port_t;
    fn csops(pid: libc::pid_t, operation: u32, flags: *mut libc::c_void, size: usize) -> libc::c_int;
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
            clusters: core_clusters(),
            cpu_ticks: Vec::new(),
            cluster_busy_percent: BTreeMap::new(),
            compressor: None,
            repacked_megabytes_per_second: 0.0,
            page_bytes: sysctl_number(c"hw.pagesize").and_then(|bytes| u64::try_from(bytes).ok()).unwrap_or(0),
        }
    }

    pub fn sample(&mut self, config: &Config, throttled: &BTreeSet<u32>) -> Snapshot {
        let now = Instant::now();
        let table = process_table();
        self.tracked = table
            .iter()
            .filter(|(pid, entry)| entry.user_id == self.user_id && **pid != self.own_pid && entry.parent != self.own_pid)
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
        let repacked_megabytes_per_second = self.repacked_megabytes_per_second();
        Snapshot {
            memory: memory_pressure(config.early_memory_free_percent, config.is_thrashing(repacked_megabytes_per_second)),
            repacked_megabytes_per_second,
            thermal: thermal_state(),
            saturated_clusters: self.saturated_clusters(config),
            processes,
        }
    }

    pub fn idle_pids(&self, config: &Config) -> HashSet<u32> {
        self.pids_idle_for(config.agent_idle())
    }

    pub fn pids_idle_for(&self, at_least: Duration) -> HashSet<u32> {
        let now = Instant::now();
        self.tracked
            .iter()
            .filter(|(_, tracked)| tracked.streak.idle_for(now).is_some_and(|idle| idle >= at_least))
            .map(|(pid, _)| *pid)
            .collect()
    }

    pub fn active_pids(&self) -> HashSet<u32> {
        self.tracked.iter().filter(|(_, tracked)| tracked.streak.idle_since.is_none()).map(|(pid, _)| *pid).collect()
    }

    pub fn repacked_megabytes_per_second(&mut self) -> f64 {
        let now = Instant::now();
        let measured_just_now =
            self.compressor.is_some_and(|before| now.saturating_duration_since(before.read_at) < SHORTEST_REPACK_WINDOW);
        if measured_just_now {
            return self.repacked_megabytes_per_second;
        }
        let current = compressor_reading(self.host, now);
        let previous = std::mem::replace(&mut self.compressor, current);
        self.repacked_megabytes_per_second = match (previous, current) {
            (Some(before), Some(after)) => megabytes_repacked_per_second(before, after, self.page_bytes),
            _ => 0.0,
        };
        self.repacked_megabytes_per_second
    }

    pub fn cluster_busy_percent(&self) -> &BTreeMap<String, f64> {
        &self.cluster_busy_percent
    }

    pub fn app_page_ins(&self, agent_work: &[u32]) -> (f64, Vec<PagingApp>) {
        const APPS_NAMED: usize = 3;
        let readings = self.tracked.iter().map(|(pid, tracked)| (*pid, tracked.responsible, tracked.page_ins_per_second));
        let by_app = page_ins_by_app(readings, &app_pids(), agent_work);
        let total = by_app.values().fold(0.0, |sum, rate| sum + rate).round();
        let mut paging: Vec<PagingApp> = by_app
            .into_iter()
            .map(|(pid, rate)| PagingApp { pid, name: process_name(pid), page_ins_per_second: rate.round() })
            .collect();
        paging.sort_by(|left, right| right.page_ins_per_second.total_cmp(&left.page_ins_per_second));
        paging.truncate(APPS_NAMED);
        (total, paging)
    }

    fn saturated_clusters(&mut self, config: &Config) -> Vec<String> {
        let current = per_cpu_ticks(self.host);
        self.cluster_busy_percent = cluster_busy_percent(&self.clusters, &self.cpu_ticks, &current);
        self.cpu_ticks = current;
        saturated(&self.cluster_busy_percent, config.busy_cpu_percent)
    }

    fn measure(&self, pid: u32, now: Instant, config: &Config) -> Option<Tracked> {
        let usage = resource_usage(pid)?;
        let cpu_time = self.ticks_to_duration(usage.ticks);
        let previous = self.tracked.get(&pid).filter(|previous| previous.started == usage.started);
        let cpu_percent = previous.map_or(0.0, |previous| {
            percent(cpu_time.saturating_sub(previous.cpu_time), now - previous.read_at)
        });
        let waits_on_disk_or_memory = previous.is_some_and(|previous| {
            let wall_time = now - previous.read_at;
            busy_disk(usage.disk_bytes.saturating_sub(previous.disk_bytes), wall_time)
                || faulting(usage.page_faults.wrapping_sub(previous.page_faults), wall_time)
        });
        let page_ins_per_second = previous.map_or(0.0, |previous| {
            per_second(usage.page_ins.saturating_sub(previous.page_ins), now - previous.read_at)
        });
        let streak = previous
            .map(|previous| previous.streak)
            .unwrap_or_default()
            .extend(cpu_percent, waits_on_disk_or_memory, now, config);
        Some(Tracked {
            started: usage.started,
            cpu_time,
            disk_bytes: usage.disk_bytes,
            page_faults: usage.page_faults,
            page_ins: usage.page_ins,
            page_ins_per_second,
            responsible: previous.map_or_else(|| responsible_pid(pid), |previous| previous.responsible),
            read_at: now,
            cpu_percent,
            streak,
        })
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
            made_by_apple: made_by_apple(pid, table),
        }
    }
}

fn page_ins_by_app(
    readings: impl Iterator<Item = (u32, u32, f64)>,
    apps: &HashSet<u32>,
    agent_work: &[u32],
) -> HashMap<u32, f64> {
    let mut by_app: HashMap<u32, f64> = HashMap::new();
    for (pid, responsible, page_ins_per_second) in readings {
        if page_ins_per_second <= 0.0 || agent_work.contains(&pid) {
            continue;
        }
        let Some(app) = [pid, responsible].into_iter().find(|candidate| apps.contains(candidate)) else {
            continue;
        };
        *by_app.entry(app).or_default() += page_ins_per_second;
    }
    by_app
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

fn resource_usage(pid: u32) -> Option<ResourceUsage> {
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let status = unsafe { libc::proc_pid_rusage(pid as libc::c_int, libc::RUSAGE_INFO_V2, (&raw mut usage).cast()) };
    (status == 0).then_some(ResourceUsage {
        started: usage.ri_proc_start_abstime,
        ticks: usage.ri_user_time + usage.ri_system_time,
        disk_bytes: usage.ri_diskio_bytesread + usage.ri_diskio_byteswritten,
        page_faults: page_faults(pid),
        page_ins: usage.ri_pageins,
    })
}

fn page_faults(pid: u32) -> u32 {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let written = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
    if written == size { info.pti_faults.cast_unsigned() } else { 0 }
}

#[derive(Debug, Clone, PartialEq)]
struct Cluster {
    name: String,
    cpus: usize,
}

fn core_clusters() -> Vec<Cluster> {
    let levels = sysctl_number(c"hw.nperflevels").unwrap_or(0);
    let clusters: Vec<Cluster> = (0..levels)
        .rev()
        .filter_map(|level| {
            let name = sysctl_text(&format!("hw.perflevel{level}.name"))?.to_lowercase();
            let cpus = usize::try_from(sysctl_number(&sysctl_name(&format!("hw.perflevel{level}.logicalcpu"))?)?).ok()?;
            Some(Cluster { name, cpus })
        })
        .collect();
    if !clusters.is_empty() {
        return clusters;
    }
    let cpus = sysctl_number(c"hw.logicalcpu").and_then(|cpus| usize::try_from(cpus).ok()).unwrap_or(0);
    vec![Cluster { name: "all".to_string(), cpus }]
}

fn saturated(busy: &BTreeMap<String, f64>, busy_percent: f32) -> Vec<String> {
    busy.iter().filter(|(_, busy)| **busy >= f64::from(busy_percent)).map(|(name, _)| name.clone()).collect()
}

fn cluster_busy_percent(clusters: &[Cluster], before: &[CpuTicks], after: &[CpuTicks]) -> BTreeMap<String, f64> {
    let cpus: usize = clusters.iter().map(|cluster| cluster.cpus).sum();
    if before.len() != cpus || after.len() != cpus {
        return BTreeMap::new();
    }
    let mut first = 0;
    clusters
        .iter()
        .filter_map(|cluster| {
            let cpus = first..first + cluster.cpus;
            first = cpus.end;
            let busy: u64 = cpus.clone().map(|cpu| after[cpu].busy.saturating_sub(before[cpu].busy)).sum();
            let total: u64 = cpus.map(|cpu| after[cpu].total.saturating_sub(before[cpu].total)).sum();
            (total > 0).then(|| (cluster.name.clone(), (busy as f64 / total as f64 * 1000.0).round() / 10.0))
        })
        .collect()
}

fn per_cpu_ticks(host: libc::mach_port_t) -> Vec<CpuTicks> {
    let mut cpus: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;
    let status =
        unsafe { libc::host_processor_info(host, libc::PROCESSOR_CPU_LOAD_INFO, &mut cpus, &mut info, &mut count) };
    if status != libc::KERN_SUCCESS {
        return Vec::new();
    }
    let loads = unsafe { std::slice::from_raw_parts(info.cast::<libc::processor_cpu_load_info>(), cpus as usize) };
    let ticks = loads
        .iter()
        .map(|load| {
            let ticks = load.cpu_ticks.map(u64::from);
            let total: u64 = ticks.iter().sum();
            CpuTicks { busy: total - ticks[libc::CPU_STATE_IDLE as usize], total }
        })
        .collect();
    let size = count as usize * std::mem::size_of::<libc::integer_t>();
    unsafe { libc::vm_deallocate(mach_task_self_,info as libc::vm_address_t, size as libc::vm_size_t) };
    ticks
}

fn per_second(count: u64, wall_time: Duration) -> f64 {
    if wall_time.is_zero() { 0.0 } else { count as f64 / wall_time.as_secs_f64() }
}

fn busy_disk(bytes: u64, wall_time: Duration) -> bool {
    !wall_time.is_zero() && bytes as f64 / wall_time.as_secs_f64() >= IDLE_DISK_BYTES_PER_SECOND
}

fn faulting(page_faults: u32, wall_time: Duration) -> bool {
    !wall_time.is_zero() && f64::from(page_faults) / wall_time.as_secs_f64() >= IDLE_PAGE_FAULTS_PER_SECOND
}

fn compressor_reading(host: libc::mach_port_t, now: Instant) -> Option<CompressorReading> {
    let mut statistics: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let status =
        unsafe { libc::host_statistics64(host, libc::HOST_VM_INFO64, (&raw mut statistics).cast(), &mut count) };
    (status == libc::KERN_SUCCESS).then_some(CompressorReading {
        compressed_pages: statistics.compressions,
        decompressed_pages: statistics.decompressions,
        read_at: now,
    })
}

fn megabytes_repacked_per_second(before: CompressorReading, after: CompressorReading, page_bytes: u64) -> f64 {
    let seconds = after.read_at.saturating_duration_since(before.read_at).as_secs_f64();
    if seconds == 0.0 {
        return 0.0;
    }
    let compressed = after.compressed_pages.saturating_sub(before.compressed_pages);
    let decompressed = after.decompressed_pages.saturating_sub(before.decompressed_pages);
    let repacked_bytes = compressed.min(decompressed) as f64 * page_bytes as f64;
    repacked_bytes / f64::from(1u32 << 20) / seconds
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

pub fn process_name(pid: u32) -> String {
    process_path(pid).file_name().map_or_else(|| format!("pid {pid}"), |name| name.to_string_lossy().into_owned())
}

fn process_path(pid: u32) -> PathBuf {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe { libc::proc_pidpath(pid as libc::c_int, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    buffer.truncate(usize::try_from(length).unwrap_or(0));
    PathBuf::from(String::from_utf8_lossy(&buffer).into_owned())
}

fn belongs_to_app(pid: u32, table: &HashMap<u32, ProcessEntry>, apps: &HashSet<u32>) -> bool {
    ancestry(pid, table).any(|pid| apps.contains(&pid) || apps.contains(&responsible_pid(pid)))
}

fn ancestry(pid: u32, table: &HashMap<u32, ProcessEntry>) -> impl Iterator<Item = u32> {
    std::iter::successors(Some(pid), |pid| table.get(pid).map(|entry| entry.parent).filter(|parent| *parent != 0))
        .take(MAX_ANCESTRY)
}

fn responsible_pid(pid: u32) -> u32 {
    let responsible = unsafe { responsibility_get_pid_responsible_for_pid(pid as libc::pid_t) };
    u32::try_from(responsible).unwrap_or(0)
}

fn made_by_apple(pid: u32, table: &HashMap<u32, ProcessEntry>) -> bool {
    ancestry(pid, table).all(is_platform_binary)
}

fn is_platform_binary(pid: u32) -> bool {
    const CS_OPS_STATUS: u32 = 0;
    const CS_PLATFORM_BINARY: u32 = 0x0400_0000;
    let mut flags: u32 = 0;
    let status =
        unsafe { csops(pid as libc::pid_t, CS_OPS_STATUS, (&raw mut flags).cast(), std::mem::size_of::<u32>()) };
    status == 0 && flags & CS_PLATFORM_BINARY != 0
}

fn memory_pressure(early_free_percent: i32, thrashing: bool) -> Stress {
    let kernel_level = sysctl_number(c"kern.memorystatus_vm_pressure_level");
    memory_stress(kernel_level, free_memory_percent(), early_free_percent, thrashing)
}

fn memory_stress(kernel_level: Option<i32>, free_percent: Option<i32>, early_free_percent: i32, thrashing: bool) -> Stress {
    const WARNING: i32 = 2;
    const CRITICAL: i32 = 4;
    match kernel_level {
        Some(CRITICAL) => Stress::Critical,
        Some(WARNING) => Stress::Elevated,
        _ if thrashing => Stress::Elevated,
        _ if free_percent.is_some_and(|free| free <= early_free_percent) => Stress::Elevated,
        _ => Stress::Calm,
    }
}

pub fn free_memory_percent() -> Option<i32> {
    sysctl_number(c"kern.memorystatus_level")
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

fn sysctl_number(name: &std::ffi::CStr) -> Option<i32> {
    let mut value: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    let status = unsafe {
        libc::sysctlbyname(name.as_ptr(), (&mut value as *mut i32).cast(), &mut size, std::ptr::null_mut(), 0)
    };
    (status == 0).then_some(value)
}

fn sysctl_text(name: &str) -> Option<String> {
    let name = sysctl_name(name)?;
    let mut buffer = [0u8; 64];
    let mut size = buffer.len();
    let status =
        unsafe { libc::sysctlbyname(name.as_ptr(), buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
    if status != 0 {
        return None;
    }
    let text = std::ffi::CStr::from_bytes_until_nul(&buffer[..size]).ok()?;
    Some(text.to_string_lossy().into_owned())
}

fn sysctl_name(name: &str) -> Option<std::ffi::CString> {
    std::ffi::CString::new(name).ok()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentWork {
    pub agents: Vec<(u32, String)>,
    pub members: Vec<u32>,
    pub starters: BTreeSet<u32>,
    pub everything: Vec<u32>,
}

pub fn agent_work(config: &Config) -> AgentWork {
    let user_id = unsafe { libc::getuid() };
    let own_pid = std::process::id();
    let table: HashMap<u32, ProcessEntry> =
        process_table().into_iter().filter(|(pid, entry)| entry.user_id == user_id && *pid != own_pid).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, entry) in &table {
        children.entry(entry.parent).or_default().push(*pid);
    }
    let agents: Vec<(u32, String)> = table
        .keys()
        .map(|pid| (*pid, program_label(*pid)))
        .filter(|(_, name)| config.is_agent_program(name))
        .collect();
    let members: Vec<u32> = agents.iter().flat_map(|(pid, _)| descendants(*pid, &children)).collect();
    let started_by_agents =
        table.keys().filter(|pid| is_marked_as_agent_work(**pid, config) && !made_by_apple(**pid, &table));
    let everything: Vec<u32> =
        members.iter().chain(started_by_agents).copied().collect::<BTreeSet<u32>>().into_iter().collect();
    let starters = everything
        .iter()
        .filter(|pid| children.contains_key(pid) || has_reaped_children(**pid))
        .copied()
        .chain(agents.iter().map(|(pid, _)| *pid))
        .collect();
    AgentWork { agents, members, starters, everything }
}

fn has_reaped_children(pid: u32) -> bool {
    let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let status = unsafe { libc::proc_pid_rusage(pid as libc::c_int, libc::RUSAGE_INFO_V2, (&raw mut usage).cast()) };
    status == 0 && usage.ri_child_elapsed_abstime > 0
}

fn is_marked_as_agent_work(pid: u32, config: &Config) -> bool {
    config.agent_nice > 0 && niceness(pid).is_some_and(|nice| nice >= config.agent_nice)
}

pub fn is_background(pid: u32) -> bool {
    const PROC_FLAG_EXT_DARWINBG: u32 = 0x10000;
    bsd_info(pid).is_some_and(|info| info.pbi_flags & PROC_FLAG_EXT_DARWINBG != 0)
}

pub fn set_background(pid: u32, background: bool) -> std::io::Result<()> {
    let priority = if background { libc::PRIO_DARWIN_BG } else { 0 };
    let status = unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t, priority) };
    if status == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

pub fn is_held(pid: u32) -> bool {
    bsd_info(pid).is_some_and(|info| info.pbi_status == libc::SSTOP)
}

pub fn hold(pid: u32) -> std::io::Result<()> {
    send_signal(pid, libc::SIGSTOP)
}

pub fn resume(pid: u32) -> std::io::Result<()> {
    send_signal(pid, libc::SIGCONT)
}

pub fn started_by_an_interactive_shell(pid: u32) -> bool {
    let Some(parent) = bsd_info(pid).map(|info| info.pbi_ppid) else {
        return true;
    };
    SHELLS.contains(&process_name(parent).as_str()) && command_line(parent).is_none_or(|line| runs_interactively(&line))
}

pub fn held_agent_work(config: &Config) -> Vec<u32> {
    agent_work(config).everything.into_iter().filter(|pid| is_held(*pid)).collect()
}

fn send_signal(pid: u32, signal: libc::c_int) -> std::io::Result<()> {
    let status = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if status == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn runs_interactively(command_line: &[String]) -> bool {
    let mut arguments = command_line.iter().skip(1).peekable();
    let mut flags = String::new();
    while let Some(option) = arguments.next_if(|argument| argument.starts_with(['-', '+'])) {
        let long = option.starts_with("--");
        if !long && option.ends_with('o') {
            arguments.next();
        }
        if !long && option.starts_with('-') {
            flags.push_str(option);
        }
    }
    let runs_a_script = arguments.peek().is_some();
    flags.contains('i') || !(flags.contains('c') || runs_a_script)
}

fn command_line(pid: u32) -> Option<Vec<String>> {
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size = sysctl_number(c"kern.argmax").and_then(|bytes| usize::try_from(bytes).ok())?;
    let mut buffer = vec![0u8; size];
    let status = unsafe {
        libc::sysctl(name.as_mut_ptr(), name.len() as u32, buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0)
    };
    if status != 0 || size < std::mem::size_of::<i32>() {
        return None;
    }
    let (count, rest) = buffer[..size].split_at(std::mem::size_of::<i32>());
    let count = usize::try_from(i32::from_ne_bytes(count.try_into().ok()?)).ok()?;
    let after_program_path = rest.iter().position(|byte| *byte == 0)?;
    let arguments = rest[after_program_path..].iter().position(|byte| *byte != 0)? + after_program_path;
    let line: Vec<String> = rest[arguments..]
        .split(|byte| *byte == 0)
        .take(count)
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect();
    (line.len() == count).then_some(line)
}

fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then_some(info)
}

fn descendants(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut found = vec![root];
    let mut next = 0;
    while let Some(pid) = found.get(next).copied() {
        found.extend(children.get(&pid).into_iter().flatten().copied());
        next += 1;
    }
    found
}

pub fn niceness(pid: u32) -> Option<i32> {
    unsafe { *libc::__error() = 0 };
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t) };
    (unsafe { *libc::__error() } == 0).then_some(nice)
}

pub fn set_niceness(pid: u32, nice: i32) -> std::io::Result<()> {
    let status = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice) };
    if status == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectMemory {
    pub directory: PathBuf,
    pub bytes: u64,
    pub programs: Vec<(String, u64)>,
}

pub fn memory_by_project() -> Vec<ProjectMemory> {
    let user_id = unsafe { libc::getuid() };
    let mut roots: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut projects: HashMap<PathBuf, ProjectMemory> = HashMap::new();
    for pid in all_pids() {
        if process_entry(pid).is_none_or(|entry| entry.user_id != user_id) {
            continue;
        }
        let Some(root) = working_directory(pid).and_then(|directory| project_root(directory, &mut roots)) else {
            continue;
        };
        let Some(bytes) = memory_footprint(pid) else {
            continue;
        };
        projects
            .entry(root.clone())
            .or_insert_with(|| ProjectMemory { directory: root, bytes: 0, programs: Vec::new() })
            .add(program_label(pid), bytes);
    }
    let mut projects: Vec<ProjectMemory> = projects.into_values().collect();
    projects.sort_by(|left, right| right.bytes.cmp(&left.bytes));
    projects
}

impl ProjectMemory {
    fn add(&mut self, program: String, bytes: u64) {
        self.bytes += bytes;
        match self.programs.iter_mut().find(|(name, _)| *name == program) {
            Some((_, total)) => *total += bytes,
            None => self.programs.push((program, bytes)),
        }
        self.programs.sort_by(|left, right| right.1.cmp(&left.1));
    }
}

impl fmt::Display for ProjectMemory {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        const PROGRAMS_SHOWN: usize = 3;
        const SMALLEST_SHOWN: u64 = 100 << 20;
        let home = crate::config::home();
        let directory = match self.directory.strip_prefix(&home) {
            Ok(relative) => format!("~/{}", relative.display()),
            Err(_) => self.directory.display().to_string(),
        };
        let programs: Vec<String> = self
            .programs
            .iter()
            .filter(|(_, bytes)| *bytes >= SMALLEST_SHOWN)
            .take(PROGRAMS_SHOWN)
            .map(|(name, bytes)| format!("{name} {}", gigabytes(*bytes)))
            .collect();
        write!(formatter, "{directory}: {} ({})", gigabytes(self.bytes), programs.join(", "))
    }
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1u32 << 30))
}

fn program_label(pid: u32) -> String {
    let path = process_path(pid);
    if path.parent().is_some_and(|folder| folder.ends_with("claude/versions")) {
        return "claude".to_string();
    }
    process_name(pid)
}

fn working_directory(pid: u32) -> Option<PathBuf> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let written =
        unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast(), size) };
    if written != size {
        return None;
    }
    let path = unsafe { std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    Some(PathBuf::from(path.to_string_lossy().into_owned()))
}

fn project_root(directory: PathBuf, cache: &mut HashMap<PathBuf, Option<PathBuf>>) -> Option<PathBuf> {
    if let Some(root) = cache.get(&directory) {
        return root.clone();
    }
    let root = directory.ancestors().find(|ancestor| ancestor.join(".git").exists()).map(Path::to_path_buf);
    cache.insert(directory, root.clone());
    root
}

pub fn memory_footprint(pid: u32) -> Option<u64> {
    let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    let status = unsafe { libc::proc_pid_rusage(pid as libc::c_int, libc::RUSAGE_INFO_V0, (&raw mut usage).cast()) };
    (status == 0).then_some(usage.ri_phys_footprint)
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
    fn page_ins_count_toward_the_app_responsible_for_them_and_never_toward_agent_work() {
        let chrome = 100;
        let chrome_renderer = 101;
        let agent_launched_by_an_app = 102;
        let launchd_daemon = 103;
        let readings = [
            (chrome, chrome, 5.0),
            (chrome_renderer, chrome, 20.0),
            (agent_launched_by_an_app, chrome, 900.0),
            (launchd_daemon, 1, 50.0),
        ];
        let by_app = page_ins_by_app(readings.into_iter(), &HashSet::from([chrome]), &[agent_launched_by_an_app]);
        assert_eq!(by_app, HashMap::from([(chrome, 25.0)]));
    }

    #[test]
    fn cpu_time_is_reported_in_real_time_units() {
        burn_cpu(Duration::from_millis(200));
        let probe = Probe::new();
        let measured = probe.ticks_to_duration(resource_usage(std::process::id()).unwrap().ticks);
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
    fn per_cpu_ticks_count_up_with_work() {
        let probe = Probe::new();
        let before = per_cpu_ticks(probe.host);
        burn_cpu(Duration::from_millis(300));
        let after = per_cpu_ticks(probe.host);
        let sum = |ticks: &[CpuTicks]| ticks.iter().fold((0, 0), |(busy, total), cpu| (busy + cpu.busy, total + cpu.total));
        let ((busy_before, total_before), (busy_after, total_after)) = (sum(&before), sum(&after));
        assert!(busy_after > busy_before && total_after > total_before);
        assert!(after.iter().all(|cpu| cpu.busy <= cpu.total));
    }

    #[test]
    fn core_clusters_cover_every_cpu_named_by_macos() {
        let probe = Probe::new();
        let clusters = core_clusters();
        let cpus: usize = clusters.iter().map(|cluster| cluster.cpus).sum();
        assert_eq!(cpus, per_cpu_ticks(probe.host).len(), "{clusters:?}");
        if sysctl_number(c"hw.nperflevels") == Some(2) {
            let names: Vec<&str> = clusters.iter().map(|cluster| cluster.name.as_str()).collect();
            assert_eq!(names, ["efficiency", "performance"]);
        }
    }

    fn m4_clusters() -> Vec<Cluster> {
        vec![Cluster { name: "efficiency".to_string(), cpus: 6 }, Cluster { name: "performance".to_string(), cpus: 4 }]
    }

    fn ticks_after(busy_per_cpu: &[u64]) -> Vec<CpuTicks> {
        busy_per_cpu.iter().map(|busy| CpuTicks { busy: *busy, total: 100 }).collect()
    }

    #[test]
    fn a_full_efficiency_cluster_is_saturated_while_the_performance_cores_idle() {
        let before = [CpuTicks { busy: 0, total: 0 }; 10];
        let after = ticks_after(&[100, 100, 100, 100, 100, 99, 25, 25, 25, 25]);
        assert_eq!(saturated(&cluster_busy_percent(&m4_clusters(), &before, &after), 90.0), vec!["efficiency"]);
    }

    #[test]
    fn clusters_below_the_bar_are_not_saturated() {
        let before = [CpuTicks { busy: 0, total: 0 }; 10];
        let after = ticks_after(&[85, 85, 85, 85, 85, 85, 95, 95, 95, 95]);
        assert_eq!(saturated(&cluster_busy_percent(&m4_clusters(), &before, &after), 90.0), vec!["performance"]);
        assert!(saturated(&cluster_busy_percent(&m4_clusters(), &before, &ticks_after(&[50; 10])), 90.0).is_empty());
    }

    #[test]
    fn no_cluster_is_saturated_without_two_readings_of_every_cpu() {
        let full = ticks_after(&[100; 10]);
        assert!(saturated(&cluster_busy_percent(&m4_clusters(), &[], &full), 90.0).is_empty());
        assert!(saturated(&cluster_busy_percent(&m4_clusters(), &[CpuTicks { busy: 0, total: 0 }; 8], &full), 90.0).is_empty());
    }

    #[test]
    fn only_programs_apple_ships_and_starts_count_as_made_by_apple() {
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let child_pid = child.id();
        let table = process_table();
        let verdicts = (is_platform_binary(child_pid), is_platform_binary(std::process::id()), made_by_apple(1, &table));
        let started_by_third_party = made_by_apple(child_pid, &table);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(verdicts, (true, false, true), "(/bin/sleep, this test binary, launchd)");
        assert!(!started_by_third_party, "/bin/sleep started by a third-party program");
    }

    #[test]
    fn names_come_from_the_executable_path() {
        let name = process_name(std::process::id());
        assert!(name.starts_with("pressure_guard"), "{name}");
    }

    #[test]
    fn memory_is_added_up_per_project_from_each_process_working_directory() {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let projects = memory_by_project();
        let own = projects.iter().find(|project| project.directory == crate_root).expect("this test's own project");
        let test_binary = own.programs.iter().find(|(name, _)| name.starts_with("pressure_guard"));
        assert!(test_binary.is_some_and(|(_, bytes)| *bytes > 0), "{own}");
        assert!(projects.windows(2).all(|pair| pair[0].bytes >= pair[1].bytes));
    }

    #[test]
    fn an_agent_and_everything_it_starts_are_its_work() {
        let mut agent = std::process::Command::new("/bin/zsh").args(["-c", "/bin/sleep 30; true"]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let config = Config { agent_programs: vec!["zsh".to_string()], ..Config::default() };
        let work = agent_work(&config);
        let member_names: Vec<String> = work.members.iter().map(|pid| process_name(*pid)).collect();
        let sleep = process_table().into_iter().find(|(_, entry)| entry.parent == agent.id()).map(|(pid, _)| pid);
        let _ = std::process::Command::new("/usr/bin/pkill").args(["-P", &agent.id().to_string()]).status();
        agent.kill().unwrap();
        agent.wait().unwrap();
        assert!(work.agents.iter().any(|(pid, name)| *pid == agent.id() && name == "zsh"));
        assert!(work.members.contains(&agent.id()), "{:?}", work.members);
        assert!(work.starters.contains(&agent.id()), "{:?}", work.starters);
        assert!(sleep.is_some_and(|sleep| work.members.contains(&sleep) && !work.starters.contains(&sleep)), "{sleep:?}");
        assert!(member_names.iter().any(|name| name == "sleep"), "{member_names:?}");
    }

    #[test]
    fn a_process_whose_short_commands_already_finished_counts_as_starting_work() {
        let python = |script: &str| std::process::Command::new("python3").args(["-c", script]).spawn().unwrap();
        let mut watcher = python("import subprocess, time; subprocess.run(['/usr/bin/true']); time.sleep(30)");
        let mut quiet = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let started = Instant::now();
        while !has_reaped_children(watcher.id()) && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let verdicts = (has_reaped_children(watcher.id()), has_reaped_children(quiet.id()));
        for child in [&mut watcher, &mut quiet] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(verdicts, (true, false), "(ran a finished command, never started one)");
    }

    #[test]
    fn a_sleeping_process_counts_as_idle_and_a_spinning_one_does_not() {
        let mut sleeping = std::process::Command::new("/bin/zsh").args(["-c", "/bin/sleep 10; true"]).spawn().unwrap();
        let mut spinning = std::process::Command::new("/bin/zsh").args(["-c", "/usr/bin/yes >/dev/null; true"]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let config = Config { agent_idle_seconds: 0, ..Config::default() };
        let mut probe = Probe::new();
        probe.sample(&config, &BTreeSet::new());
        std::thread::sleep(Duration::from_millis(500));
        probe.sample(&config, &BTreeSet::new());
        let idle = probe.idle_pids(&config);
        let table = process_table();
        let child_of = |shell: u32| table.iter().find(|(_, entry)| entry.parent == shell).map(|(pid, _)| *pid).unwrap();
        let (sleep, yes) = (child_of(sleeping.id()), child_of(spinning.id()));
        for shell in [&mut sleeping, &mut spinning] {
            let _ = std::process::Command::new("/usr/bin/pkill").args(["-P", &shell.id().to_string()]).status();
            shell.kill().unwrap();
            shell.wait().unwrap();
        }
        assert!(idle.contains(&sleep), "sleep should be idle");
        assert!(!idle.contains(&yes), "yes should be busy");
    }

    #[test]
    fn low_cpu_work_that_keeps_the_disk_busy_is_not_idle() {
        let config = Config::default();
        let now = Instant::now();
        let reading_files = Streak::default().extend(1.0, true, now, &config);
        let waiting = Streak::default().extend(1.0, false, now, &config);
        assert_eq!(reading_files.idle_for(now), None);
        assert_eq!(waiting.idle_for(now), Some(Duration::ZERO));
    }

    #[test]
    fn memory_counts_as_tight_once_free_memory_drops_to_the_early_level_before_macos_says_so() {
        assert_eq!(memory_stress(Some(1), Some(48), 30, false), Stress::Calm);
        assert_eq!(memory_stress(Some(1), Some(30), 30, false), Stress::Elevated);
        assert_eq!(memory_stress(Some(1), Some(12), 0, false), Stress::Calm);
        assert_eq!(memory_stress(Some(2), Some(60), 30, false), Stress::Elevated);
        assert_eq!(memory_stress(Some(4), Some(5), 30, false), Stress::Critical);
        assert_eq!(memory_stress(None, None, 30, false), Stress::Calm);
    }

    fn compressor_after(seconds: u64, compressed_pages: u64, decompressed_pages: u64) -> (CompressorReading, CompressorReading) {
        let start = Instant::now();
        let before = CompressorReading { compressed_pages: 0, decompressed_pages: 0, read_at: start };
        let after = CompressorReading { compressed_pages, decompressed_pages, read_at: start + Duration::from_secs(seconds) };
        (before, after)
    }

    #[test]
    fn memory_packed_and_unpacked_again_is_measured_in_megabytes_a_second() {
        let (before, after) = compressor_after(10, 1_164_523, 1_029_708);
        let slowdown_of_2026_11_18 = megabytes_repacked_per_second(before, after, 16384);
        assert!((1608.0..1610.0).contains(&slowdown_of_2026_11_18), "{slowdown_of_2026_11_18}");
    }

    #[test]
    fn memory_that_is_packed_away_and_stays_there_is_not_repacking() {
        let (before, after) = compressor_after(10, 500_000, 0);
        assert_eq!(megabytes_repacked_per_second(before, after, 16384), 0.0);
        assert_eq!(megabytes_repacked_per_second(before, before, 16384), 0.0);
    }

    #[test]
    fn the_kernel_reports_how_many_pages_the_compressor_has_packed_and_unpacked() {
        let probe = Probe::new();
        let first = compressor_reading(probe.host, Instant::now()).expect("compressor counters");
        let second = compressor_reading(probe.host, Instant::now()).expect("compressor counters");
        assert!(second.compressed_pages >= first.compressed_pages, "{first:?} {second:?}");
        assert!(second.decompressed_pages >= first.decompressed_pages, "{first:?} {second:?}");
        assert!(probe.page_bytes >= 4096, "{}", probe.page_bytes);
    }

    #[test]
    fn thrashing_memory_counts_as_tight_while_macos_reports_normal_and_45_percent_free() {
        assert_eq!(memory_stress(Some(1), Some(45), 30, true), Stress::Elevated);
        assert_eq!(memory_stress(Some(4), Some(45), 30, true), Stress::Critical);
    }

    #[test]
    fn free_memory_is_read_as_a_percentage() {
        assert!(free_memory_percent().is_some_and(|free| (0..=100).contains(&free)));
    }

    #[test]
    fn a_held_process_stops_running_until_it_is_resumed() {
        let mut child = std::process::Command::new("/bin/sleep").arg("10").spawn().unwrap();
        let pid = child.id();
        let settle = || std::thread::sleep(Duration::from_millis(100));
        let before = is_held(pid);
        hold(pid).unwrap();
        settle();
        let during = is_held(pid);
        resume(pid).unwrap();
        settle();
        let after = is_held(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!((before, during, after), (false, true, false));
    }

    #[test]
    fn an_agent_can_still_end_a_held_process_when_its_own_time_limit_runs_out() {
        use std::os::unix::process::ExitStatusExt;
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let mut child = std::process::Command::new("/bin/sleep").arg("10").spawn().unwrap();
            hold(child.id()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            send_signal(child.id(), signal).unwrap();
            let asked = Instant::now();
            while child.try_wait().unwrap().is_none() && asked.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let ended_by = child.try_wait().unwrap().and_then(|status| status.signal());
            let _ = child.kill();
            assert_eq!(ended_by, Some(signal), "a held process, never resumed");
        }
    }

    fn command_line_of(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn a_shell_given_a_command_or_a_script_is_not_interactive() {
        for words in [
            &["/bin/zsh", "-c", "tsc --noEmit"][..],
            &["bash", "-lc", "bun test"],
            &["bash", "--login", "-c", "make"],
            &["sh", "-eo", "pipefail", "-c", "make"],
            &["zsh", "build.sh", "--release"],
        ] {
            assert!(!runs_interactively(&command_line_of(words)), "{words:?}");
        }
    }

    #[test]
    fn a_shell_asked_to_be_interactive_or_given_nothing_to_run_is_interactive() {
        for words in [
            &["/bin/zsh", "-ic", "tsc --noEmit"][..],
            &["bash", "-i", "-c", "make"],
            &["-zsh"],
            &["zsh", "-l"],
            &["/bin/zsh", "-o", "nopromptsp"],
        ] {
            assert!(runs_interactively(&command_line_of(words)), "{words:?}");
        }
    }

    #[test]
    fn work_started_by_an_interactive_shell_is_told_apart_from_work_started_by_a_shell_command() {
        let shell = |flags: &str| std::process::Command::new("/bin/zsh").args([flags, "/bin/sleep 10; true"]).spawn().unwrap();
        let mut shells = [shell("-c"), shell("-ic")];
        std::thread::sleep(Duration::from_millis(1500));
        let table = process_table();
        let child_of = |shell: u32| table.iter().find(|(_, entry)| entry.parent == shell).map(|(pid, _)| *pid);
        let children = shells.each_ref().map(|shell| child_of(shell.id()));
        let verdicts = children.map(|child| child.map(started_by_an_interactive_shell));
        let seen = command_line(shells[0].id());
        for (shell, child) in shells.iter_mut().zip(children) {
            if let Some(child) = child {
                let _ = send_signal(child, libc::SIGKILL);
            }
            shell.kill().unwrap();
            shell.wait().unwrap();
        }
        assert_eq!(verdicts, [Some(false), Some(true)], "(zsh -c, zsh -ic)");
        assert_eq!(seen, Some(command_line_of(&["/bin/zsh", "-c", "/bin/sleep 10; true"])));
    }

    #[test]
    fn background_mode_is_set_seen_and_cleared_on_a_running_process() {
        let mut child = std::process::Command::new("/bin/sleep").arg("10").spawn().unwrap();
        let pid = child.id();
        set_background(pid, false).unwrap();
        let before = is_background(pid);
        set_background(pid, true).unwrap();
        let during = is_background(pid);
        set_background(pid, false).unwrap();
        let after = is_background(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!((before, during, after), (false, true, false));
    }
}
