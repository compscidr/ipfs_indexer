/*!
 * Pauses the crawl while the gateway keeps failing (#284). It keeps the outcomes of the last
 * WINDOW fetches; when TRIP of them are gateway errors it opens for a pause, then lets one test
 * fetch through. A test that works closes it; one that fails doubles the pause, up to a cap.
 */
use log::{info, warn};
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

// the last this many fetches are kept, and this many gateway errors among them pause the crawl
const WINDOW: usize = 20;
const TRIP: usize = 16;
const MIN_PAUSE: Duration = Duration::from_secs(60);
const MAX_PAUSE: Duration = Duration::from_secs(15 * 60);

/// What a worker may do before taking a page
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Permit {
    /// crawl as normal, and record() the outcome
    Go,
    /// take nothing: the crawl is paused
    Wait,
    /// fetch one page as the test and report it with tested(), or hand_back() if none was fetched
    Test,
}

pub struct Breaker {
    // the root this one watches, for the log (#286); None for the whole crawl
    name: Option<String>,
    min: Duration,
    max: Duration,
    state: Mutex<State>,
}

struct State {
    // true for a gateway error, oldest first
    recent: VecDeque<bool>,
    // the length of the current pause, or the next one
    pause: Duration,
    // open until then; None while crawling
    until: Option<Instant>,
    // a worker holds the test
    testing: bool,
    // when it opened: failures among how many recorded, and the last error, for status()
    failed: usize,
    of: usize,
    last_error: String,
}

impl Breaker {
    pub fn new() -> Breaker {
        Breaker::with(MIN_PAUSE, MAX_PAUSE)
    }

    pub fn with(min: Duration, max: Duration) -> Breaker {
        Breaker::build(None, min, max)
    }

    /// One root's breaker (#286): the log says "root <name>" where it would say "the crawl"
    pub fn named(name: &str, min: Duration, max: Duration) -> Breaker {
        Breaker::build(Some(name.to_string()), min, max)
    }

    fn build(name: Option<String>, min: Duration, max: Duration) -> Breaker {
        Breaker {
            name,
            min,
            max,
            state: Mutex::new(State {
                recent: VecDeque::with_capacity(WINDOW),
                pause: min,
                until: None,
                testing: false,
                failed: 0,
                of: 0,
                last_error: String::new(),
            }),
        }
    }

    pub fn permit(&self) -> Permit {
        self.permit_at(Instant::now())
    }

    /// A Go fetch's outcome: Some(error) for a gateway error
    pub fn record(&self, failure: Option<&str>) {
        self.record_at(Instant::now(), failure)
    }

    /// The test fetch's outcome: Some(error) for a gateway error
    pub fn tested(&self, failure: Option<&str>) {
        self.tested_at(Instant::now(), failure)
    }

    /// Gives the test back unused, for the next worker that asks
    pub fn hand_back(&self) {
        self.state().testing = false;
    }

    /// True when permit() would say Wait: paused with time left, or a test out. Changes nothing
    pub fn blocks(&self) -> bool {
        self.blocks_at(Instant::now())
    }

    /// True while paused or testing
    pub fn is_open(&self) -> bool {
        self.state().until.is_some()
    }

    /// The last gateway error seen; empty before any
    pub fn last_error(&self) -> String {
        self.state().last_error.clone()
    }

    /// e.g. "paused 4m: 16 of the last 20 fetches failed; last: <error>"; None while crawling
    pub fn status(&self) -> Option<String> {
        self.status_at(Instant::now())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn blocks_at(&self, now: Instant) -> bool {
        let s = self.state();
        matches!(s.until, Some(until) if s.testing || now < until)
    }

    // "the crawl", or "it" for a root (named in the same line)
    // " of root <name>" for a root's breaker, else nothing
    fn of_root(&self) -> String {
        match &self.name {
            Some(name) => format!(" of root {}", name),
            None => String::new(),
        }
    }

    fn object(&self) -> &'static str {
        if self.name.is_some() {
            "it"
        } else {
            "the crawl"
        }
    }

    fn permit_at(&self, now: Instant) -> Permit {
        let mut s = self.state();
        match s.until {
            None => Permit::Go,
            Some(until) if s.testing || now < until => Permit::Wait,
            Some(_) => {
                s.testing = true;
                Permit::Test
            }
        }
    }

    fn record_at(&self, now: Instant, failure: Option<&str>) {
        let mut s = self.state();
        // only the test's outcome changes an open breaker
        if s.until.is_some() {
            return;
        }
        if s.recent.len() == WINDOW {
            s.recent.pop_front();
        }
        s.recent.push_back(failure.is_some());
        if let Some(err) = failure {
            s.last_error = err.to_string();
        }
        let failed = s.recent.iter().filter(|f| **f).count();
        if failed >= TRIP {
            s.failed = failed;
            s.of = s.recent.len();
            s.until = Some(now + s.pause);
            let of_what = self.of_root();
            warn!(
                "{} of the last {} fetches{} failed at the gateway: pausing {} for {:?} (last: {})",
                s.failed,
                s.of,
                of_what,
                self.object(),
                s.pause,
                s.last_error
            );
        }
    }

    fn tested_at(&self, now: Instant, failure: Option<&str>) {
        let mut s = self.state();
        s.testing = false;
        match failure {
            None => {
                s.until = None;
                s.recent.clear();
                s.pause = self.min;
                match &self.name {
                    Some(name) => info!("Root {} answered the test fetch: crawling it again", name),
                    None => info!("The gateway answered the test fetch: crawling again"),
                }
            }
            Some(err) => {
                s.last_error = err.to_string();
                s.pause = (s.pause * 2).min(self.max);
                s.until = Some(now + s.pause);
                warn!(
                    "The test fetch{} failed at the gateway: pausing {} for {:?} (last: {})",
                    self.of_root(),
                    self.object(),
                    s.pause,
                    err
                );
            }
        }
    }

    fn status_at(&self, now: Instant) -> Option<String> {
        let s = self.state();
        let until = s.until?;
        let why = format!(
            "{} of the last {} fetches failed; last: {}",
            s.failed, s.of, s.last_error
        );
        if s.testing {
            return Some(format!("testing after {}", why));
        }
        let left = until.saturating_duration_since(now).as_secs();
        let left = if left >= 60 {
            format!("{}m", left / 60)
        } else {
            format!("{}s", left)
        };
        Some(format!("paused {}: {}", left, why))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // opens it at `at` with TRIP failures
    fn trip(breaker: &Breaker, at: Instant) {
        for _ in 0..TRIP {
            breaker.record_at(at, Some("504"));
        }
    }

    #[test]
    fn fifteen_failures_in_twenty_keep_crawling_and_sixteen_pause() {
        let breaker = Breaker::with(ms(100), ms(400));
        let t = Instant::now();
        for _ in 0..5 {
            breaker.record_at(t, None);
        }
        for _ in 0..15 {
            breaker.record_at(t, Some("504"));
        }
        assert_eq!(breaker.permit_at(t), Permit::Go);
        // the oldest success falls out: 16 of the last 20
        breaker.record_at(t, Some("504"));
        assert_eq!(breaker.permit_at(t), Permit::Wait);
    }

    #[test]
    fn a_pause_gives_one_test_when_it_ends() {
        let breaker = Breaker::with(ms(100), ms(400));
        let t = Instant::now();
        trip(&breaker, t);
        assert_eq!(breaker.permit_at(t + ms(99)), Permit::Wait);
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
        assert_eq!(
            breaker.permit_at(t + ms(101)),
            Permit::Wait,
            "one test at a time"
        );
    }

    #[test]
    fn a_test_that_works_resumes_and_resets_the_pause() {
        let breaker = Breaker::with(ms(100), ms(400));
        let t = Instant::now();
        trip(&breaker, t);
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
        breaker.tested_at(t + ms(100), Some("504")); // the next pause is 200ms
        assert_eq!(breaker.permit_at(t + ms(300)), Permit::Test);
        breaker.tested_at(t + ms(300), None);
        assert_eq!(breaker.permit_at(t + ms(300)), Permit::Go);
        assert_eq!(breaker.status_at(t + ms(300)), None);
        // the record was cleared, and the next pause is the shortest again
        for _ in 0..TRIP - 1 {
            breaker.record_at(t + ms(300), Some("504"));
        }
        assert_eq!(breaker.permit_at(t + ms(300)), Permit::Go);
        breaker.record_at(t + ms(300), Some("504"));
        assert_eq!(breaker.permit_at(t + ms(399)), Permit::Wait);
        assert_eq!(breaker.permit_at(t + ms(400)), Permit::Test);
    }

    #[test]
    fn a_failed_test_doubles_the_pause_up_to_the_cap() {
        let breaker = Breaker::with(ms(100), ms(400));
        let t = Instant::now();
        trip(&breaker, t);
        let mut at = t + ms(100);
        for pause in [200, 400, 400] {
            assert_eq!(breaker.permit_at(at), Permit::Test);
            breaker.tested_at(at, Some("504"));
            assert_eq!(breaker.permit_at(at + ms(pause - 1)), Permit::Wait);
            at += ms(pause);
        }
        assert_eq!(breaker.permit_at(at), Permit::Test);
    }

    #[test]
    fn a_handed_back_test_goes_to_the_next_caller() {
        let breaker = Breaker::with(ms(100), ms(400));
        let t = Instant::now();
        trip(&breaker, t);
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
        breaker.hand_back();
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
    }

    #[test]
    fn fetches_finishing_during_a_pause_change_nothing() {
        let breaker = Breaker::with(ms(100), ms(400));
        let t = Instant::now();
        trip(&breaker, t);
        for _ in 0..WINDOW {
            breaker.record_at(t + ms(10), None);
        }
        assert_eq!(breaker.permit_at(t + ms(50)), Permit::Wait, "not closed");
        for _ in 0..WINDOW {
            breaker.record_at(t + ms(60), Some("504"));
        }
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test, "not extended");
    }

    #[test]
    fn only_one_of_many_workers_gets_the_test() {
        let breaker = Arc::new(Breaker::with(ms(100), ms(400)));
        let t = Instant::now();
        trip(&breaker, t);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let breaker = Arc::clone(&breaker);
                std::thread::spawn(move || breaker.permit_at(t + ms(100)))
            })
            .collect();
        let permits: Vec<Permit> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(permits.iter().filter(|p| **p == Permit::Test).count(), 1);
        assert_eq!(permits.iter().filter(|p| **p == Permit::Wait).count(), 7);
    }

    #[test]
    fn status_says_why_the_crawl_is_paused() {
        let breaker = Breaker::with(Duration::from_secs(240), Duration::from_secs(900));
        let t = Instant::now();
        assert_eq!(breaker.status_at(t), None);
        for _ in 0..4 {
            breaker.record_at(t, None);
        }
        for _ in 0..TRIP {
            breaker.record_at(t, Some("504 Gateway Timeout"));
        }
        let why = "16 of the last 20 fetches failed; last: 504 Gateway Timeout";
        assert_eq!(breaker.status_at(t), Some(format!("paused 4m: {}", why)));
        assert_eq!(
            breaker.status_at(t + Duration::from_secs(200)),
            Some(format!("paused 40s: {}", why))
        );
        assert_eq!(
            breaker.permit_at(t + Duration::from_secs(240)),
            Permit::Test
        );
        assert_eq!(
            breaker.status_at(t + Duration::from_secs(240)),
            Some(format!("testing after {}", why))
        );
    }

    #[test]
    fn blocks_matches_permit_without_taking_the_test() {
        let breaker = Breaker::with(ms(100), ms(400));
        let t = Instant::now();
        assert!(!breaker.blocks_at(t));
        trip(&breaker, t);
        assert!(breaker.blocks_at(t));
        assert!(!breaker.blocks_at(t + ms(100)));
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
        assert!(breaker.blocks_at(t + ms(100)));
    }
}
