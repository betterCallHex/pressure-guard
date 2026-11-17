mod agent;
mod config;
mod enforcer;
mod events;
mod policy;
mod system;
mod watchdog;

use agent::Agent;
use config::Config;
use enforcer::Enforcer;
use objc2_foundation::NSRunLoop;
use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;
use system::Probe;
use watchdog::Watchdog;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => run(false),
        ["--dry-run"] => run(true),
        ["status"] => {
            println!("{}", enforcer::status_report());
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
    log(&format!("started{}, config {}", if dry_run { " in dry run" } else { "" }, Config::path().display()));
    let watchdog = RefCell::new(Watchdog::spawn());
    let _heartbeat = events::every(watchdog::BEAT_INTERVAL, move || watchdog.borrow_mut().beat());
    let agent = Rc::new(RefCell::new(Agent::new(config, Probe::new(), Enforcer::new(dry_run))));
    let _subscriptions = events::subscribe(&agent);
    agent.borrow_mut().start();
    NSRunLoop::mainRunLoop().run();
    eprintln!("event loop ended unexpectedly");
    agent.borrow_mut().shut_down();
    ExitCode::FAILURE
}

pub fn log(message: &str) {
    println!("{} {message}", jiff::ZonedDateTime::now().strftime("%Y-%m-%d %H:%M:%S"));
}

pub fn friendly_duration(duration: Duration) -> String {
    let whole_seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    format!("{:#}", jiff::SignedDuration::from_secs(whole_seconds))
}
