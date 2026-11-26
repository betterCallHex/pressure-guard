use crate::config::home;
use crate::policy::Throttle;
use crate::system::set_background;
use crate::friendly_duration;
use crate::journal::log;
use serde_json::{json, Value};
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

pub struct Enforcer {
    dry_run: bool,
    throttled: BTreeMap<u32, Held>,
}

#[derive(Debug, Clone, PartialEq)]
struct Held {
    name: String,
    why: String,
    since: Timestamp,
}

impl Enforcer {
    pub fn new(dry_run: bool) -> Enforcer {
        Enforcer { dry_run, throttled: BTreeMap::new() }
    }

    pub fn recover_from_previous_run(&self) {
        let Some(restored) = restore_saved() else {
            return;
        };
        log(
            "services_recovered",
            &format!("restored {restored} processes left slowed by a previous run"),
            json!({ "restored": restored }),
        );
        let _ = std::fs::remove_file(state_path());
    }

    pub fn throttled(&self) -> BTreeSet<u32> {
        self.throttled.keys().copied().collect()
    }

    pub fn apply(&mut self, plan: &BTreeMap<u32, Throttle>) {
        let released: Vec<u32> = self.throttled.keys().filter(|pid| !plan.contains_key(pid)).copied().collect();
        for pid in released {
            let Some(held) = self.throttled.remove(&pid) else { continue };
            let slowed_for = (Timestamp::now() - held.since).unsigned_abs();
            self.act(
                "service_restored",
                &format!("restore normal priority for {} ({pid}) after {}", held.name, friendly_duration(slowed_for)),
                json!({ "pid": pid, "name": held.name, "slowed_seconds": slowed_for.as_secs() }),
                || set_background(pid, false),
            );
        }
        for (pid, throttle) in plan {
            if self.throttled.contains_key(pid) {
                continue;
            }
            self.act(
                "service_slowed",
                &format!("slow down {} ({pid}) {throttle}", throttle.name),
                json!({
                    "pid": pid,
                    "name": throttle.name,
                    "cpu_percent": throttle.cpu_percent.round(),
                    "reason": throttle.reason.kind(),
                    "because": throttle.reason.to_string(),
                }),
                || set_background(*pid, true),
            );
            let held = Held { name: throttle.name.clone(), why: throttle.to_string(), since: Timestamp::now() };
            self.throttled.insert(*pid, held);
        }
        self.save();
    }

    pub fn release_all(&mut self) {
        self.apply(&BTreeMap::new());
    }

    fn act(&self, event: &str, description: &str, fields: Value, action: impl FnOnce() -> io::Result<()>) {
        if self.dry_run {
            log(event, &format!("[dry run] would {description}"), fields);
            return;
        }
        match action() {
            Ok(()) => log(event, description, fields),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => log(
                "action_failed",
                &format!("could not {description}: {error}"),
                with(with(fields, "action", json!(event)), "error", json!(error.to_string())),
            ),
        }
    }

    fn save(&self) {
        if self.dry_run {
            return;
        }
        if self.throttled.is_empty() {
            let _ = std::fs::remove_file(state_path());
            return;
        }
        let path = state_path();
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, encode(&self.throttled));
    }
}

fn with(mut fields: Value, key: &str, value: Value) -> Value {
    if let Value::Object(map) = &mut fields {
        map.insert(key.to_string(), value);
    }
    fields
}

pub fn status_report() -> String {
    let held = std::fs::read_to_string(state_path()).map(|text| decode(&text)).unwrap_or_default();
    if held.is_empty() {
        return "No background services are slowed right now.".to_string();
    }
    let now = Timestamp::now();
    let lines: Vec<String> = held
        .iter()
        .map(|(pid, held)| {
            let slowed_for = friendly_duration((now - held.since).unsigned_abs());
            format!("  {} ({pid}), slowed for {slowed_for}, {}", held.name, held.why)
        })
        .collect();
    let noun = if held.len() == 1 { "service" } else { "services" };
    format!("{} background {noun} slowed:\n{}", held.len(), lines.join("\n"))
}

pub fn restore_saved() -> Option<usize> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    let pids = pids_to_restore(&text);
    for pid in &pids {
        let _ = set_background(*pid, false);
    }
    Some(pids.len())
}

fn pids_to_restore(text: &str) -> Vec<u32> {
    text.lines().filter_map(|line| line.split('\t').next()?.parse().ok()).collect()
}

fn encode(throttled: &BTreeMap<u32, Held>) -> String {
    let field = |text: &str| text.replace(['\t', '\n'], " ");
    throttled
        .iter()
        .map(|(pid, held)| format!("{pid}\t{}\t{}\t{}\n", held.since.as_second(), field(&held.name), field(&held.why)))
        .collect()
}

fn decode(text: &str) -> BTreeMap<u32, Held> {
    text.lines().filter_map(decode_line).collect()
}

fn decode_line(line: &str) -> Option<(u32, Held)> {
    let mut fields = line.splitn(4, '\t');
    let pid = fields.next()?.parse().ok()?;
    let since = Timestamp::from_second(fields.next()?.parse().ok()?).ok()?;
    let name = fields.next()?.to_string();
    let why = fields.next()?.to_string();
    Some((pid, Held { name, why, since }))
}

fn state_path() -> PathBuf {
    home().join("Library/Application Support/pressure-guard/held-pids")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_processes_survive_a_round_trip_through_the_state_file() {
        let since = Timestamp::from_second(1_800_000_000).unwrap();
        let held = Held { name: "media\tanalysisd".to_string(), why: "at 95% CPU, memory pressure elevated".to_string(), since };
        let decoded = decode(&encode(&BTreeMap::from([(42, held)])));
        assert_eq!(decoded[&42].name, "media analysisd");
        assert_eq!(decoded[&42].why, "at 95% CPU, memory pressure elevated");
        assert_eq!(decoded[&42].since, since);
    }

    #[test]
    fn state_file_from_an_older_version_still_lists_pids_to_restore() {
        assert_eq!(pids_to_restore("17\n42\n"), vec![17, 42]);
    }
}
