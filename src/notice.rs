use crate::config::{Config, home};
use crate::system::{self, ProjectMemory, Stress};
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PROJECTS_SHOWN: usize = 3;

#[derive(Deserialize, Default)]
#[serde(default)]
struct HookInput {
    session_id: String,
    cwd: Option<PathBuf>,
    hook_event_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LastNotice {
    level: Stress,
    ago: Duration,
}

pub fn run() -> ExitCode {
    let config = Config::load().unwrap_or_default();
    let pressure = system::memory_pressure();
    if pressure == Stress::Calm {
        return ExitCode::SUCCESS;
    }
    let mut text = String::new();
    let _ = std::io::stdin().read_to_string(&mut text);
    let input: HookInput = serde_json::from_str(&text).unwrap_or_default();
    let record = notice_record(&input.session_id);
    if !is_due(pressure, read_last_notice(&record), config.notice_repeat()) {
        return ExitCode::SUCCESS;
    }
    write_notice(&record, pressure);
    let agents = system::agent_work(&config).agents.len();
    let message = advice(pressure, agents, input.cwd.as_deref(), &system::memory_by_project());
    let event = if input.hook_event_name.is_empty() { "PostToolUse" } else { input.hook_event_name.as_str() };
    let output = serde_json::json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": message } });
    println!("{output}");
    ExitCode::SUCCESS
}

fn is_due(pressure: Stress, last: Option<LastNotice>, repeat: Duration) -> bool {
    last.is_none_or(|last| pressure > last.level || last.ago >= repeat)
}

fn notice_record(session_id: &str) -> PathBuf {
    let safe: String = session_id.chars().filter(|character| character.is_ascii_alphanumeric() || *character == '-').collect();
    home().join("Library/Caches/pressure-guard/notices").join(if safe.is_empty() { "unknown".to_string() } else { safe })
}

fn read_last_notice(record: &Path) -> Option<LastNotice> {
    let text = std::fs::read_to_string(record).ok()?;
    let (seconds, level) = text.trim().split_once(' ')?;
    let at = UNIX_EPOCH + Duration::from_secs(seconds.parse().ok()?);
    let level = if level == Stress::Critical.label() { Stress::Critical } else { Stress::Elevated };
    Some(LastNotice { level, ago: SystemTime::now().duration_since(at).unwrap_or_default() })
}

fn write_notice(record: &Path, pressure: Stress) {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    if let Some(directory) = record.parent() {
        let _ = std::fs::create_dir_all(directory);
    }
    let _ = std::fs::write(record, format!("{seconds} {}", pressure.label()));
}

fn advice(pressure: Stress, agents: usize, cwd: Option<&Path>, projects: &[ProjectMemory]) -> String {
    let own_root = cwd.and_then(|cwd| cwd.ancestors().find(|ancestor| ancestor.join(".git").exists()));
    let own = projects.iter().find(|project| Some(project.directory.as_path()) == own_root);
    let yours = own.map_or_else(String::new, |project| format!("\nYour project now uses {project}."));
    let biggest: Vec<String> = projects.iter().take(PROJECTS_SHOWN).map(|project| format!("- {project}")).collect();
    format!(
        "pressure-guard: the Mac is under heavy usage. Memory pressure is {}, with {agents} coding agents sharing its RAM; \
         if memory runs out, the whole Mac freezes. Please use less RAM while you keep working on your task:\n\
         - stop dev servers, watchers and browser sessions (`agent-browser close`) you no longer need, and reuse one that is already running instead of starting another;\n\
         - run installs, builds and test suites one at a time, never several in parallel;\n\
         - prefer targeted commands (a single test file, a filtered build) over whole-repo runs.{yours}\n\
         Biggest memory users right now:\n{}",
        pressure.label(),
        biggest.join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPEAT: Duration = Duration::from_secs(120);

    #[test]
    fn the_first_sign_of_pressure_is_always_announced() {
        assert!(is_due(Stress::Elevated, None, REPEAT));
    }

    #[test]
    fn the_same_pressure_is_repeated_only_after_the_repeat_interval() {
        let recent = LastNotice { level: Stress::Elevated, ago: Duration::from_secs(30) };
        let old = LastNotice { level: Stress::Elevated, ago: REPEAT };
        assert!(!is_due(Stress::Elevated, Some(recent), REPEAT));
        assert!(is_due(Stress::Elevated, Some(old), REPEAT));
    }

    #[test]
    fn worsening_pressure_is_announced_at_once() {
        let recent = LastNotice { level: Stress::Elevated, ago: Duration::from_secs(5) };
        assert!(is_due(Stress::Critical, Some(recent), REPEAT));
        let recent_critical = LastNotice { level: Stress::Critical, ago: Duration::from_secs(5) };
        assert!(!is_due(Stress::Elevated, Some(recent_critical), REPEAT));
    }

    #[test]
    fn a_notice_survives_a_round_trip_through_its_record() {
        let record = std::env::temp_dir().join(format!("pressure-guard-notice-{}", std::process::id()));
        write_notice(&record, Stress::Critical);
        let last = read_last_notice(&record).unwrap();
        let _ = std::fs::remove_file(&record);
        assert_eq!(last.level, Stress::Critical);
        assert!(last.ago < Duration::from_secs(5));
    }

    #[test]
    fn advice_asks_for_less_ram_and_shows_the_agents_own_project() {
        let own = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let gigabyte = 1 << 30;
        let projects = vec![
            ProjectMemory { directory: PathBuf::from("/work/shop"), bytes: 3 * gigabyte, programs: vec![("node".into(), 3 * gigabyte)] },
            ProjectMemory { directory: own.clone(), bytes: gigabyte, programs: vec![("vite".into(), gigabyte)] },
        ];
        let text = advice(Stress::Critical, 7, Some(&own.join("src")), &projects);
        assert!(text.contains("Memory pressure is critical, with 7 coding agents"), "{text}");
        assert!(text.contains("Please use less RAM"), "{text}");
        assert!(text.contains("Your project now uses ") && text.contains("1.0 GB (vite 1.0 GB)."), "{text}");
        assert!(text.contains("- /work/shop: 3.0 GB (node 3.0 GB)"), "{text}");
    }
}
