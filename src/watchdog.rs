use crate::config::home;
use crate::{enforcer, log};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::parent_id;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

pub const BEAT_INTERVAL: Duration = Duration::from_secs(5);
const FROZEN_AFTER: Duration = Duration::from_secs(30);

pub struct Watchdog {
    child: Option<Child>,
}

impl Watchdog {
    pub fn spawn() -> Watchdog {
        Watchdog { child: start_child() }
    }

    pub fn beat(&mut self) {
        if let Some(exit) = self.exit_status() {
            log(&format!("watchdog {exit}, starting a new one"));
            self.child = start_child();
        }
        let Some(pipe) = self.child.as_mut().and_then(|child| child.stdin.as_mut()) else {
            return;
        };
        let _ = pipe.write(&[0]);
    }

    fn exit_status(&mut self) -> Option<String> {
        let Some(child) = &mut self.child else {
            return Some("is not running".to_string());
        };
        match child.try_wait() {
            Ok(None) => None,
            Ok(Some(status)) => Some(format!("exited ({status})")),
            Err(error) => Some(format!("could not be checked ({error})")),
        }
    }
}

pub fn run() -> ExitCode {
    if !is_pipe(libc::STDIN_FILENO) {
        eprintln!("pressure-guard starts its own watchdog; it is not run by hand");
        return ExitCode::FAILURE;
    }
    let agent = parent_id();
    if watch(libc::STDIN_FILENO, FROZEN_AFTER) == Verdict::Frozen {
        rescue(agent);
    }
    ExitCode::SUCCESS
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Frozen,
    AgentExited,
}

enum Beat {
    Received,
    Missed,
    Closed,
}

fn watch(input: RawFd, frozen_after: Duration) -> Verdict {
    let mut last_beat = Instant::now();
    loop {
        let silent_for = last_beat.elapsed();
        if silent_for >= frozen_after {
            return Verdict::Frozen;
        }
        match next_beat(input, frozen_after - silent_for) {
            Beat::Received => last_beat = Instant::now(),
            Beat::Missed => {}
            Beat::Closed => return Verdict::AgentExited,
        }
    }
}

fn next_beat(input: RawFd, timeout: Duration) -> Beat {
    let mut request = libc::pollfd { fd: input, events: libc::POLLIN, revents: 0 };
    let milliseconds = libc::c_int::try_from(timeout.as_millis() + 1).unwrap_or(libc::c_int::MAX);
    let ready = unsafe { libc::poll(&mut request, 1, milliseconds) };
    if ready == 0 || (ready < 0 && interrupted()) {
        return Beat::Missed;
    }
    if ready < 0 {
        return Beat::Closed;
    }
    let mut buffer = [0u8; 64];
    let read = unsafe { libc::read(input, buffer.as_mut_ptr().cast(), buffer.len()) };
    match read {
        1.. => Beat::Received,
        _ if interrupted() => Beat::Missed,
        _ => Beat::Closed,
    }
}

fn interrupted() -> bool {
    io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
}

fn rescue(agent: u32) {
    log(&format!("watchdog: pressure-guard ({agent}) has not responded for {}s", FROZEN_AFTER.as_secs()));
    let restored = enforcer::restore_saved().unwrap_or(0);
    log(&format!("watchdog: restored {restored} processes it had slowed"));
    if let Some(report) = record_stacks(agent) {
        log(&format!("watchdog: saved where it was stuck to {report}"));
    }
    if parent_id() != agent {
        log("watchdog: it exited on its own");
        return;
    }
    unsafe { libc::kill(agent as libc::pid_t, libc::SIGKILL) };
    log("watchdog: stopped it so launchd starts a fresh one");
}

fn record_stacks(agent: u32) -> Option<String> {
    let stamp = jiff::ZonedDateTime::now().strftime("%Y%m%d-%H%M%S");
    let report = home().join(format!("Library/Logs/pressure-guard-freeze-{stamp}.txt"));
    let sampled = Command::new("/usr/bin/sample")
        .args([&agent.to_string(), "1", "-mayDie", "-file"])
        .arg(&report)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    sampled.ok().filter(|status| status.success()).map(|_| report.display().to_string())
}

fn start_child() -> Option<Child> {
    let spawned = std::env::current_exe()
        .and_then(|program| Command::new(program).arg("watchdog").stdin(Stdio::piped()).spawn());
    match spawned {
        Ok(child) => {
            if let Some(pipe) = &child.stdin {
                set_nonblocking(pipe.as_raw_fd());
            }
            Some(child)
        }
        Err(error) => {
            log(&format!("could not start the watchdog: {error}"));
            None
        }
    }
}

fn set_nonblocking(fd: RawFd) {
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
}

fn is_pipe(fd: RawFd) -> bool {
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    let found = unsafe { libc::fstat(fd, &mut status) } == 0;
    found && status.st_mode & libc::S_IFMT == libc::S_IFIFO
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::thread;

    fn pipe() -> (OwnedFd, std::fs::File) {
        let (reader, writer) = io::pipe().unwrap();
        (reader.into(), std::fs::File::from(OwnedFd::from(writer)))
    }

    #[test]
    fn silence_past_the_limit_means_the_agent_froze() {
        let (reader, _writer) = pipe();
        let started = Instant::now();
        assert_eq!(watch(reader.as_raw_fd(), Duration::from_millis(200)), Verdict::Frozen);
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn steady_beats_keep_the_agent_alive_until_it_exits() {
        let (reader, mut writer) = pipe();
        let beating = thread::spawn(move || {
            for _ in 0..10 {
                writer.write_all(&[0]).unwrap();
                thread::sleep(Duration::from_millis(50));
            }
        });
        let started = Instant::now();
        assert_eq!(watch(reader.as_raw_fd(), Duration::from_millis(200)), Verdict::AgentExited);
        assert!(started.elapsed() >= Duration::from_millis(450));
        beating.join().unwrap();
    }

    #[test]
    fn a_pipe_is_required_so_a_shell_is_never_mistaken_for_the_agent() {
        let (reader, _writer) = pipe();
        assert!(is_pipe(reader.as_raw_fd()));
        let file = std::fs::File::open("Cargo.toml").unwrap();
        assert!(!is_pipe(file.as_raw_fd()));
    }
}
