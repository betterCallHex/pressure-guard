use serde_json::{Map, Value};
use std::io::Write;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

const ROTATE_AT_BYTES: u64 = 64 << 20;

static DRY_RUN: AtomicBool = AtomicBool::new(false);

pub fn mark_dry_run() {
    DRY_RUN.store(true, Ordering::Relaxed);
}

pub fn log(event: &str, message: &str, fields: Value) {
    let now = jiff::ZonedDateTime::now();
    println!("{} {message}", now.strftime("%Y-%m-%d %H:%M:%S"));
    write(&now, event, Some(message), fields);
}

pub fn record(event: &str, fields: Value) {
    write(&jiff::ZonedDateTime::now(), event, None, fields);
}

fn write(now: &jiff::ZonedDateTime, event: &str, message: Option<&str>, fields: Value) {
    let Some(path) = journal_path() else {
        return;
    };
    let time = now.strftime("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string();
    append(path, &entry(&time, event, message, DRY_RUN.load(Ordering::Relaxed), fields));
}

pub fn journal_path() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| beside(&regular_file_path(libc::STDOUT_FILENO)?)).as_deref()
}

fn regular_file_path(fd: RawFd) -> Option<PathBuf> {
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    let regular = unsafe { libc::fstat(fd, &mut status) } == 0 && status.st_mode & libc::S_IFMT == libc::S_IFREG;
    if !regular {
        return None;
    }
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    let found = unsafe { libc::fcntl(fd, libc::F_GETPATH, buffer.as_mut_ptr()) } == 0;
    let path = std::ffi::CStr::from_bytes_until_nul(&buffer).ok().filter(|_| found)?;
    Some(PathBuf::from(path.to_string_lossy().into_owned()))
}

fn beside(log: &Path) -> Option<PathBuf> {
    Some(log.with_file_name(format!("{}.jsonl", log.file_stem()?.to_string_lossy())))
}

fn entry(time: &str, event: &str, message: Option<&str>, dry_run: bool, fields: Value) -> String {
    let mut line = Map::new();
    line.insert("time".to_string(), Value::from(time));
    line.insert("event".to_string(), Value::from(event));
    if let Some(message) = message {
        line.insert("message".to_string(), Value::from(message));
    }
    if dry_run {
        line.insert("dry_run".to_string(), Value::from(true));
    }
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    Value::Object(line).to_string()
}

fn append(path: &Path, line: &str) {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() >= ROTATE_AT_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("jsonl.1"));
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_entry_is_one_json_object_with_its_fields_beside_the_time_event_and_message() {
        let line = entry("2026-11-26T09:17:11.000+01:00", "service_slowed", Some("slow down cloudd"), false, json!({"pid": 42}));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["event"], "service_slowed");
        assert_eq!(parsed["message"], "slow down cloudd");
        assert_eq!(parsed["pid"], 42);
        assert!(!line.contains('\n'));
        assert!(parsed.get("dry_run").is_none());
    }

    #[test]
    fn every_entry_of_a_dry_run_says_so_so_no_reader_mistakes_it_for_an_action_taken() {
        let line = entry("2026-11-26T09:36:08.887+01:00", "turns_started", Some("memory is short"), true, json!({}));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["dry_run"], true);
    }

    #[test]
    fn the_journal_sits_beside_the_log_launchd_writes() {
        let log = Path::new("/opt/homebrew/var/log/pressure-guard.log");
        assert_eq!(beside(log).unwrap(), Path::new("/opt/homebrew/var/log/pressure-guard.jsonl"));
    }
}
