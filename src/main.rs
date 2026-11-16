mod agent;
mod config;
mod enforcer;
mod events;
mod policy;
mod system;

use agent::Agent;
use config::Config;
use enforcer::Enforcer;
use objc2_foundation::NSRunLoop;
use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use system::Probe;

fn main() -> ExitCode {
    let dry_run = std::env::args().any(|argument| argument == "--dry-run");
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("invalid config {error}");
            return ExitCode::FAILURE;
        }
    };
    let probe = match Probe::new() {
        Ok(probe) => probe,
        Err(error) => {
            eprintln!("cannot read processes: {error}");
            return ExitCode::FAILURE;
        }
    };
    log(&format!("started{}, config {}", if dry_run { " in dry run" } else { "" }, Config::path().display()));
    let agent = Rc::new(RefCell::new(Agent::new(config, probe, Enforcer::new(dry_run))));
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
