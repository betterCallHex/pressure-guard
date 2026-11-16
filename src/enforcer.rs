use crate::config::home;
use crate::log;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

pub struct Enforcer {
    dry_run: bool,
    throttled: BTreeMap<u32, String>,
}

impl Enforcer {
    pub fn new(dry_run: bool) -> Enforcer {
        Enforcer { dry_run, throttled: BTreeMap::new() }
    }

    pub fn recover_from_previous_run(&self) {
        let Ok(text) = std::fs::read_to_string(state_path()) else {
            return;
        };
        let pids: Vec<u32> = text.lines().filter_map(|line| line.parse().ok()).collect();
        for pid in &pids {
            let _ = set_background(*pid, false);
        }
        log(&format!("restored {} processes left slowed by a previous run", pids.len()));
        let _ = std::fs::remove_file(state_path());
    }

    pub fn throttled(&self) -> BTreeSet<u32> {
        self.throttled.keys().copied().collect()
    }

    pub fn apply(&mut self, plan: &BTreeMap<u32, String>) {
        let released: Vec<u32> = self.throttled.keys().filter(|pid| !plan.contains_key(pid)).copied().collect();
        for pid in released {
            let name = self.throttled.remove(&pid).unwrap_or_default();
            self.act(&format!("restore normal priority for {name} ({pid})"), || set_background(pid, false));
        }
        for (pid, name) in plan {
            if self.throttled.contains_key(pid) {
                continue;
            }
            self.act(&format!("slow down {name} ({pid})"), || set_background(*pid, true));
            self.throttled.insert(*pid, name.clone());
        }
        self.save();
    }

    pub fn release_all(&mut self) {
        self.apply(&BTreeMap::new());
    }

    fn act(&self, description: &str, action: impl FnOnce() -> io::Result<()>) {
        if self.dry_run {
            log(&format!("[dry run] would {description}"));
            return;
        }
        match action() {
            Ok(()) => log(description),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => log(&format!("could not {description}: {error}")),
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
        let text: String = self.throttled.keys().map(|pid| format!("{pid}\n")).collect();
        let path = state_path();
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, text);
    }
}

fn state_path() -> PathBuf {
    home().join("Library/Application Support/pressure-guard/held-pids")
}

fn set_background(pid: u32, background: bool) -> io::Result<()> {
    let priority = if background { libc::PRIO_DARWIN_BG } else { 0 };
    let status = unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t, priority) };
    if status == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}
