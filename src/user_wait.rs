use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAKE_EVERY: Duration = Duration::from_millis(50);
const ONE_FRAME: Duration = Duration::from_millis(16);
const FELT: Duration = Duration::from_millis(100);

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct UserWait {
    pub app_page_ins_per_second: f64,
    pub paging_apps: Vec<PagingApp>,
    pub wake_up_lateness: Lateness,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PagingApp {
    pub pid: u32,
    pub name: String,
    pub page_ins_per_second: f64,
}

#[derive(Serialize, Debug, Clone, Copy, Default, PartialEq)]
pub struct Lateness {
    pub wake_ups: u32,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub over_a_frame: u32,
    pub over_100_ms: u32,
}

pub struct LatenessProbe {
    window: Arc<Mutex<Window>>,
}

#[derive(Default)]
struct Window {
    wake_ups: u32,
    total: Duration,
    max: Duration,
    over_a_frame: u32,
    over_felt: u32,
}

impl LatenessProbe {
    pub fn start() -> LatenessProbe {
        let window = Arc::new(Mutex::new(Window::default()));
        let shared = Arc::clone(&window);
        let started = std::thread::Builder::new().name("user-wait probe".to_string()).spawn(move || {
            unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) };
            loop {
                let due = Instant::now() + WAKE_EVERY;
                std::thread::sleep(WAKE_EVERY);
                let late = Instant::now().saturating_duration_since(due);
                if let Ok(mut window) = shared.lock() {
                    window.record(late);
                }
            }
        });
        if let Err(error) = started {
            eprintln!("could not start the user-wait probe: {error}");
        }
        LatenessProbe { window }
    }

    pub fn take(&self) -> Lateness {
        self.window.lock().map(|mut window| std::mem::take(&mut *window).summary()).unwrap_or_default()
    }
}

impl Window {
    fn record(&mut self, late: Duration) {
        self.wake_ups += 1;
        self.total += late;
        self.max = self.max.max(late);
        self.over_a_frame += u32::from(late > ONE_FRAME);
        self.over_felt += u32::from(late > FELT);
    }

    fn summary(self) -> Lateness {
        let milliseconds = |duration: Duration| (duration.as_secs_f64() * 10_000.0).round() / 10.0;
        Lateness {
            wake_ups: self.wake_ups,
            max_ms: milliseconds(self.max),
            mean_ms: milliseconds(self.total.checked_div(self.wake_ups).unwrap_or_default()),
            over_a_frame: self.over_a_frame,
            over_100_ms: self.over_felt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_counts_wake_ups_late_by_more_than_a_frame_and_more_than_a_felt_lag() {
        let mut window = Window::default();
        for late in [1, 3, 20, 250] {
            window.record(Duration::from_millis(late));
        }
        let lateness = window.summary();
        assert_eq!(lateness.wake_ups, 4);
        assert_eq!(lateness.max_ms, 250.0);
        assert_eq!(lateness.mean_ms, 68.5);
        assert_eq!(lateness.over_a_frame, 2);
        assert_eq!(lateness.over_100_ms, 1);
    }

    #[test]
    fn taking_the_readings_starts_a_fresh_window() {
        let probe = LatenessProbe::start();
        std::thread::sleep(WAKE_EVERY * 4);
        assert!(probe.take().wake_ups >= 2);
        assert!(probe.take().wake_ups <= 1);
    }
}
