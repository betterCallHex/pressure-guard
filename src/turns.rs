use crate::config::Config;
use crate::system::{self, AgentWork, Snapshot, Stress};
use crate::friendly_duration;
use crate::journal::log;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

pub const QUIET_RUNNER_LOSES_ITS_TURN_AFTER: Duration = Duration::from_secs(3);
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const SETTLES_AFTER_HOLDING: Duration = Duration::from_secs(3);
const TURNS_TO_SETTLE_AFTER_RESUMING: u32 = 3;

pub struct Turns {
    dry_run: bool,
    rota: Option<Rota>,
    held: BTreeSet<u32>,
    short: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Rota {
    queue: Vec<u32>,
    running_at_once: usize,
    since: Instant,
    turn_started: Instant,
    settled_from: Instant,
    roomy_since: Option<Instant>,
}

#[derive(Debug, Default)]
struct Jobs {
    alive: BTreeSet<u32>,
    heavy: Vec<u32>,
    held: BTreeSet<u32>,
    gone_quiet: HashSet<u32>,
}

pub struct Activity {
    pub active: HashSet<u32>,
    pub gone_quiet: HashSet<u32>,
}

impl Turns {
    pub fn new(dry_run: bool) -> Turns {
        Turns { dry_run, rota: None, held: BTreeSet::new(), short: false }
    }

    pub fn recover_from_previous_run(&self, config: &Config) {
        if self.dry_run {
            return;
        }
        let resumed = resume_work_left_held(config);
        if resumed > 0 {
            log(
                "turns_recovered",
                &format!("resumed {resumed} agent jobs left waiting their turn by a previous run"),
                json!({ "resumed": resumed }),
            );
        }
    }

    pub fn tend(&mut self, work: &AgentWork, snapshot: &Snapshot, activity: Activity, config: &Config) {
        self.short = config.turn().is_some() && is_short(snapshot, config);
        let Some(turn) = config.turn().filter(|_| self.short || self.rota.is_some()) else {
            self.stop();
            return;
        };
        let now = Instant::now();
        let jobs = Jobs {
            alive: work.everything.iter().copied().collect(),
            heavy: heavy_busy_jobs(work, &activity.active, &self.held, config),
            held: self.held.clone(),
            gone_quiet: activity.gone_quiet,
        };
        let before = self.rota.take();
        let after = next_rota(before.clone(), &jobs, self.short, turn, now);
        announce(before.as_ref(), after.as_ref(), snapshot, turn, now);
        self.hold_only(after.as_ref().map(|rota| rota.waiting().iter().copied().collect()).unwrap_or_default());
        self.rota = after;
    }

    pub fn stop(&mut self) {
        self.hold_only(BTreeSet::new());
        self.rota = None;
    }

    pub fn taking_their_turn(&self) -> BTreeSet<u32> {
        self.rota.as_ref().map(|rota| rota.running().iter().copied().collect()).unwrap_or_default()
    }

    pub fn waiting_their_turn(&self) -> BTreeSet<u32> {
        self.held.clone()
    }

    pub fn scan_interval(&self) -> Option<Duration> {
        (self.short || self.rota.is_some()).then_some(SCAN_INTERVAL)
    }

    fn hold_only(&mut self, waiting: BTreeSet<u32>) {
        if self.dry_run {
            self.held = waiting;
            return;
        }
        for pid in self.held.difference(&waiting) {
            let _ = system::resume(*pid);
        }
        self.held = waiting.into_iter().filter(|pid| system::is_held(*pid) || system::hold(*pid).is_ok()).collect();
    }
}

pub fn resume_work_left_held(config: &Config) -> usize {
    let held = system::held_agent_work(config);
    for pid in &held {
        let _ = system::resume(*pid);
    }
    held.len()
}

pub fn status_report(config: &Config) -> Option<String> {
    let held = system::held_agent_work(config);
    let noun = if held.len() == 1 { "job waits" } else { "jobs wait" };
    let names: Vec<String> = held.iter().map(|pid| format!("{} ({pid})", system::process_name(*pid))).collect();
    (!held.is_empty()).then(|| {
        format!("{} memory-heavy agent {noun} for a turn while memory is short: {}", held.len(), names.join(", "))
    })
}

fn is_short(snapshot: &Snapshot, config: &Config) -> bool {
    snapshot.memory == Stress::Critical || config.is_thrashing(snapshot.repacked_megabytes_per_second)
}

fn heavy_busy_jobs(work: &AgentWork, active: &HashSet<u32>, held: &BTreeSet<u32>, config: &Config) -> Vec<u32> {
    let agents: BTreeSet<u32> = work.agents.iter().map(|(pid, _)| *pid).collect();
    let mut jobs: Vec<(u64, u32)> = work
        .everything
        .iter()
        .filter(|pid| !agents.contains(pid) && active.contains(pid) && !held.contains(pid))
        .filter_map(|pid| Some((system::memory_footprint(*pid)?, *pid)))
        .filter(|(footprint, pid)| config.takes_turns_from(*footprint) && !system::started_by_an_interactive_shell(*pid))
        .collect();
    jobs.sort_by(|left, right| right.cmp(left));
    jobs.into_iter().map(|(_, pid)| pid).collect()
}

fn next_rota(rota: Option<Rota>, jobs: &Jobs, short: bool, turn: Duration, now: Instant) -> Option<Rota> {
    let Some(mut rota) = rota else {
        return (short && jobs.heavy.len() > 1).then(|| Rota::starting(&jobs.heavy, now));
    };
    rota.queue = rota.queue_with(jobs);
    match (short, now >= rota.settled_from) {
        (true, true) => rota.narrow(now),
        (true, false) => {}
        (false, _) => rota.widen(turn, now),
    }
    if !short && rota.waiting().is_empty() {
        return None;
    }
    rota.pass_the_turn(jobs, turn, now);
    Some(rota)
}

impl Rota {
    fn starting(heaviest_first: &[u32], now: Instant) -> Rota {
        Rota {
            queue: heaviest_first.to_vec(),
            running_at_once: half(heaviest_first.len()),
            since: now,
            turn_started: now,
            settled_from: now + SETTLES_AFTER_HOLDING,
            roomy_since: None,
        }
    }

    fn running(&self) -> &[u32] {
        &self.queue[..self.running_at_once.min(self.queue.len())]
    }

    fn waiting(&self) -> &[u32] {
        &self.queue[self.running().len()..]
    }

    fn queue_with(&self, jobs: &Jobs) -> Vec<u32> {
        let staying = self
            .queue
            .iter()
            .enumerate()
            .filter(|(place, pid)| {
                let keeps_its_turn = *place < self.running_at_once && !jobs.gone_quiet.contains(pid);
                jobs.alive.contains(pid) && (keeps_its_turn || jobs.held.contains(pid) || jobs.heavy.contains(pid))
            })
            .map(|(_, pid)| pid);
        let arriving = jobs.heavy.iter().filter(|pid| !self.queue.contains(pid));
        staying.chain(arriving).copied().collect()
    }

    fn narrow(&mut self, now: Instant) {
        self.roomy_since = None;
        self.running_at_once = half(self.running().len());
        self.settled_from = now + SETTLES_AFTER_HOLDING;
    }

    fn widen(&mut self, turn: Duration, now: Instant) {
        let roomy_since = *self.roomy_since.get_or_insert(now);
        if now - roomy_since < turn {
            return;
        }
        self.running_at_once += 1;
        self.roomy_since = Some(now);
        self.settled_from = now + turn / TURNS_TO_SETTLE_AFTER_RESUMING;
    }

    fn pass_the_turn(&mut self, jobs: &Jobs, turn: Duration, now: Instant) {
        if self.waiting().is_empty() {
            self.turn_started = now;
            return;
        }
        if now - self.turn_started < turn {
            return;
        }
        let had_their_turn: Vec<u32> = self.queue.drain(..self.running_at_once).collect();
        self.queue.extend(had_their_turn.into_iter().filter(|pid| !jobs.gone_quiet.contains(pid)));
        self.turn_started = now;
        self.settled_from = now + turn / TURNS_TO_SETTLE_AFTER_RESUMING;
    }
}

fn half(jobs: usize) -> usize {
    (jobs / 2).max(1)
}

fn announce(before: Option<&Rota>, after: Option<&Rota>, snapshot: &Snapshot, turn: Duration, now: Instant) {
    match (before, after) {
        (None, Some(rota)) => log(
            "turns_started",
            &format!(
                "memory is short ({}): memory-heavy agent jobs take turns, {} of {} at a time for {} each, so the \
                 Mac stops swapping; every job still runs to the end",
                shortage(snapshot),
                rota.running_at_once.min(rota.queue.len()),
                rota.queue.len(),
                friendly_duration(turn),
            ),
            json!({
                "memory_pressure": snapshot.memory.label(),
                "repacked_mb_per_second": snapshot.repacked_megabytes_per_second.round(),
                "turn_seconds": turn.as_secs(),
                "running": describe_jobs(rota.running()),
                "waiting": describe_jobs(rota.waiting()),
            }),
        ),
        (Some(earlier), Some(rota)) if earlier.running_at_once != rota.running_at_once => log(
            "turns_resized",
            &format!(
                "memory-heavy agent jobs running at a time: {}, waiting their turn: {}",
                rota.running().len(),
                rota.waiting().len(),
            ),
            json!({
                "repacked_mb_per_second": snapshot.repacked_megabytes_per_second.round(),
                "running": describe_jobs(rota.running()),
                "waiting": describe_jobs(rota.waiting()),
            }),
        ),
        (Some(earlier), None) => log(
            "turns_ended",
            &format!(
                "memory has room again: agent jobs stopped taking turns after {}",
                friendly_duration(now - earlier.since),
            ),
            json!({ "after_seconds": (now - earlier.since).as_secs() }),
        ),
        _ => {}
    }
}

fn describe_jobs(pids: &[u32]) -> Vec<Value> {
    pids.iter()
        .map(|pid| {
            let megabytes = system::memory_footprint(*pid).map(|bytes| bytes >> 20);
            json!({ "pid": pid, "name": system::process_name(*pid), "footprint_mb": megabytes })
        })
        .collect()
}

fn shortage(snapshot: &Snapshot) -> String {
    if snapshot.memory == Stress::Critical {
        return "memory pressure critical".to_string();
    }
    format!("{:.0} MB repacked a second", snapshot.repacked_megabytes_per_second)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: Duration = Duration::from_secs(10);

    fn jobs(heavy: &[u32], held: &[u32]) -> Jobs {
        Jobs {
            alive: heavy.iter().chain(held).copied().collect(),
            heavy: heavy.to_vec(),
            held: held.iter().copied().collect(),
            gone_quiet: HashSet::new(),
        }
    }

    fn held_by(rota: &Rota) -> Vec<u32> {
        rota.waiting().to_vec()
    }

    #[test]
    fn nothing_takes_turns_while_memory_has_room() {
        assert_eq!(next_rota(None, &jobs(&[10, 11, 12, 13], &[]), false, TURN, Instant::now()), None);
    }

    #[test]
    fn a_shortage_lets_half_the_heavy_jobs_run_and_holds_the_lighter_half() {
        let rota = next_rota(None, &jobs(&[10, 11, 12, 13], &[]), true, TURN, Instant::now()).unwrap();
        assert_eq!((rota.running_at_once, held_by(&rota)), (2, vec![12, 13]));
    }

    #[test]
    fn a_lone_heavy_job_is_never_held() {
        let start = Instant::now();
        assert_eq!(next_rota(None, &jobs(&[10], &[]), true, TURN, start), None);
        let last_one_left = Rota { running_at_once: 1, ..Rota::starting(&[10], start) };
        let later = next_rota(Some(last_one_left), &jobs(&[10], &[]), true, TURN, start + Duration::from_secs(60)).unwrap();
        assert_eq!((later.running_at_once, held_by(&later)), (1, vec![]));
    }

    #[test]
    fn while_the_shortage_lasts_fewer_jobs_run_at_once_down_to_one() {
        let start = Instant::now();
        let all = [10, 11, 12, 13, 14, 15, 16, 17];
        let mut rota = next_rota(None, &jobs(&all, &[]), true, TURN, start).unwrap();
        let mut running = vec![rota.running_at_once];
        for second in 1..=9 {
            let held = held_by(&rota);
            let busy: Vec<u32> = all.iter().copied().filter(|pid| !held.contains(pid)).collect();
            rota = next_rota(Some(rota), &jobs(&busy, &held), true, TURN, start + Duration::from_secs(second)).unwrap();
            running.push(rota.running_at_once);
        }
        assert_eq!(running, [4, 4, 4, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn every_job_gets_a_turn_and_none_waits_longer_than_the_others_run() {
        let start = Instant::now();
        let all = [10, 11, 12];
        let mut rota = Rota { running_at_once: 1, ..Rota::starting(&all, start) };
        let mut ran = Vec::new();
        for second in 0..30 {
            let held = held_by(&rota);
            let busy: Vec<u32> = all.iter().copied().filter(|pid| !held.contains(pid)).collect();
            rota = next_rota(Some(rota), &jobs(&busy, &held), true, TURN, start + Duration::from_secs(second)).unwrap();
            ran.push(rota.queue[0]);
        }
        assert_eq!(ran[..10], [10; 10]);
        assert_eq!(ran[10..20], [11; 10]);
        assert_eq!(ran[20..], [12; 10]);
    }

    #[test]
    fn the_swapping_right_after_a_handover_is_not_taken_for_a_new_shortage() {
        let start = Instant::now();
        let all = [10, 11, 12, 13];
        let mut rota = Rota { running_at_once: 2, ..Rota::starting(&all, start) };
        let mut running = Vec::new();
        for second in 1..=14 {
            let held = held_by(&rota);
            let busy: Vec<u32> = all.iter().copied().filter(|pid| !held.contains(pid)).collect();
            let swapping = second > 10;
            rota = next_rota(Some(rota), &jobs(&busy, &held), swapping, TURN, start + Duration::from_secs(second)).unwrap();
            running.push((rota.queue[0], rota.running_at_once));
        }
        assert_eq!(running[8..], [(10, 2), (12, 2), (12, 2), (12, 2), (12, 2), (12, 1)]);
    }

    #[test]
    fn a_job_that_finishes_hands_its_place_to_the_next_in_line_at_once() {
        let start = Instant::now();
        let rota = Rota { running_at_once: 1, ..Rota::starting(&[10, 11, 12], start) };
        let next = next_rota(Some(rota), &jobs(&[], &[11, 12]), true, TURN, start + Duration::from_secs(1)).unwrap();
        assert_eq!((next.queue[0], held_by(&next)), (11, vec![12]));
    }

    #[test]
    fn a_job_that_grows_heavy_during_a_shortage_joins_the_end_of_the_line() {
        let start = Instant::now();
        let rota = Rota { running_at_once: 1, ..Rota::starting(&[10, 11], start) };
        let next = next_rota(Some(rota), &jobs(&[10, 20], &[11]), true, TURN, start + Duration::from_secs(1)).unwrap();
        assert_eq!(next.queue, [10, 11, 20]);
    }

    #[test]
    fn a_runner_that_goes_quiet_gives_its_turn_to_the_next_in_line_and_a_brief_pause_does_not() {
        let start = Instant::now();
        let later = start + Duration::from_secs(5);
        let rota = Rota { running_at_once: 1, ..Rota::starting(&[10, 11], start) };
        let pausing = Jobs { alive: BTreeSet::from([10, 11]), ..jobs(&[], &[11]) };
        let quiet = Jobs { alive: BTreeSet::from([10, 11]), gone_quiet: HashSet::from([10]), ..jobs(&[], &[11]) };
        assert_eq!(next_rota(Some(rota.clone()), &pausing, true, TURN, later).unwrap().queue, [10, 11]);
        assert_eq!(next_rota(Some(rota), &quiet, true, TURN, later).unwrap().queue, [11]);
    }

    #[test]
    fn once_memory_has_room_one_more_job_runs_after_each_roomy_turn() {
        let start = Instant::now();
        let all = [10, 11, 12, 13];
        let mut rota = Rota { running_at_once: 1, ..Rota::starting(&all, start) };
        let mut running = Vec::new();
        for second in [1, 9, 11, 20, 21, 31] {
            let held = held_by(&rota);
            let busy: Vec<u32> = all.iter().copied().filter(|pid| !held.contains(pid)).collect();
            let Some(next) = next_rota(Some(rota.clone()), &jobs(&busy, &held), false, TURN, start + Duration::from_secs(second)) else {
                running.push(usize::MAX);
                break;
            };
            rota = next;
            running.push(rota.running_at_once);
        }
        assert_eq!(running, [1, 1, 2, 2, 3, usize::MAX]);
    }

    #[test]
    fn turns_end_as_soon_as_nobody_waits_and_memory_has_room() {
        let start = Instant::now();
        let rota = Rota { running_at_once: 2, ..Rota::starting(&[10, 11, 12], start) };
        let one_finished = jobs(&[10], &[12]);
        assert_eq!(next_rota(Some(rota.clone()), &one_finished, false, TURN, start + Duration::from_secs(1)), None);
        assert!(next_rota(Some(rota), &one_finished, true, TURN, start + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn a_shortage_is_critical_pressure_or_thrashing_and_tight_memory_alone_is_not() {
        let snapshot = |memory, repacked_megabytes_per_second| Snapshot {
            memory,
            repacked_megabytes_per_second,
            thermal: Stress::Calm,
            saturated_clusters: Vec::new(),
            processes: Vec::new(),
        };
        let config = Config::default();
        assert!(is_short(&snapshot(Stress::Critical, 0.0), &config));
        assert!(is_short(&snapshot(Stress::Elevated, 930.0), &config));
        assert!(!is_short(&snapshot(Stress::Elevated, 124.0), &config));
    }

    #[test]
    fn a_dry_run_tracks_who_would_wait_without_holding_anyone() {
        let mut child = std::process::Command::new("/bin/sleep").arg("10").spawn().unwrap();
        let mut turns = Turns::new(true);
        turns.hold_only(BTreeSet::from([child.id()]));
        let verdict = (turns.held.contains(&child.id()), system::is_held(child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(verdict, (true, false));
    }

    #[test]
    fn held_jobs_stop_running_and_resume_where_they_left_off() {
        let mut child = std::process::Command::new("/bin/sleep").arg("10").spawn().unwrap();
        let pid = child.id();
        let mut turns = Turns::new(false);
        turns.hold_only(BTreeSet::from([pid]));
        std::thread::sleep(Duration::from_millis(100));
        let while_waiting = system::is_held(pid);
        turns.stop();
        std::thread::sleep(Duration::from_millis(100));
        let after = (system::is_held(pid), child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!((while_waiting, after), (true, (false, true)));
    }
}
