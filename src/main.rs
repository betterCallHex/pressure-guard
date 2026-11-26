mod agent;
mod config;
mod enforcer;
mod events;
mod housekeeping;
mod journal;
mod policy;
mod system;
mod turns;
mod user_wait;
mod watchdog;

use agent::Agent;
use config::Config;
use enforcer::Enforcer;
use housekeeping::Housekeeper;
use journal::log;
use serde_json::json;
use objc2_foundation::NSRunLoop;
use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;
use system::Probe;
use turns::Turns;
use user_wait::LatenessProbe;
use watchdog::Watchdog;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => run(false),
        ["--dry-run"] => run(true),
        ["status"] => {
            println!("{}", enforcer::status_report());
            if let Some(waiting) = turns::status_report(&Config::load().unwrap_or_default()) {
                println!("{waiting}");
            }
            ExitCode::SUCCESS
        }
        ["watchdog"] => watchdog::run(),
        _ => {
            eprintln!("usage: pressure-guard [--dry-run | status]");
            ExitCode::FAILURE
        }
    }
}

fn run(dry_run: bool) -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("invalid config {error}");
            return ExitCode::FAILURE;
        }
    };
    log(
        "started",
        &format!("started{}, config {}", if dry_run { " in dry run" } else { "" }, Config::path().display()),
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "dry_run": dry_run,
            "config_path": Config::path(),
            "config": config,
        }),
    );
    let watchdog = RefCell::new(Watchdog::spawn());
    let _heartbeat = events::every(watchdog::BEAT_INTERVAL, move || watchdog.borrow_mut().beat());
    let agent = Agent::new(
        config,
        Probe::new(),
        LatenessProbe::start(),
        Enforcer::new(dry_run),
        Housekeeper::new(dry_run),
        Turns::new(dry_run),
    );
    let agent = Rc::new(RefCell::new(agent));
    let _subscriptions = events::subscribe(&agent);
    let _memory_watch = events::every(agent::MEMORY_WATCH_INTERVAL, {
        let agent = Rc::clone(&agent);
        move || agent.borrow_mut().watch_memory()
    });
    agent.borrow_mut().start();
    NSRunLoop::mainRunLoop().run();
    eprintln!("event loop ended unexpectedly");
    agent.borrow_mut().shut_down();
    ExitCode::FAILURE
}

pub fn friendly_duration(duration: Duration) -> String {
    let whole_seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    format!("{:#}", jiff::SignedDuration::from_secs(whole_seconds))
}
