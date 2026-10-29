/*!
 * Pauses the crawl while the gateway keeps failing (#284). It keeps the outcomes of the last
 * `window` fetches; when `failures` of them are gateway errors it opens for a pause, then lets
 * one test fetch through. A test that works closes it; one that fails doubles the pause, up to
 * a cap. The rule is passed in with each outcome, so a change to it applies from the next one
 * (#304).
 */
use log::{info, warn};
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// When to pause and for how long: the pause_* settings (#304)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PauseRule {
    /// the last this many fetches are kept
    pub window: usize,
    /// this many gateway errors among them pause the crawl
    pub failures: usize,
    /// the first pause
    pub min: Duration,
    /// the longest pause
    pub max: Duration,
}

impl PauseRule {
    // the pause_* settings' defaults; the crawl itself reads the settings (IndexQueue::pause_rule)
    #[cfg(test)]
    pub const DEFAULT: PauseRule = PauseRule {
        window: 20,
        failures: 16,
        min: Duration::from_secs(60),
        max: Duration::from_secs(15 * 60),
    };
}

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
    state: Mutex<State>,
}

struct State {
    // true for a gateway error, oldest first
    recent: VecDeque<bool>,
    // the length of the current pause; None while crawling, so the next one is the rule's min
    pause: Option<Duration>,
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
    #[cfg(test)]
    pub fn new() -> Breaker {
        Breaker::build(None)
    }

    /// One root's breaker (#286): the log says "root <name>" where it would say "the crawl"
    pub fn named(name: &str) -> Breaker {
        Breaker::build(Some(name.to_string()))
    }

    fn build(name: Option<String>) -> Breaker {
        Breaker {
            name,
            state: Mutex::new(State {
                recent: VecDeque::new(),
                pause: None,
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

    /// A Go fetch's outcome: Some(error) for a gateway error, judged by `rule`
    pub fn record(&self, failure: Option<&str>, rule: PauseRule) {
        self.record_at(Instant::now(), failure, rule)
    }

    /// The test fetch's outcome: Some(error) for a gateway error, judged by `rule`
    pub fn tested(&self, failure: Option<&str>, rule: PauseRule) {
        self.tested_at(Instant::now(), failure, rule)
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

    // " of root <name>" for a root's breaker, else nothing
    fn of_root(&self) -> String {
        match &self.name {
            Some(name) => format!(" of root {}", name),
            None => String::new(),
        }
    }

    // "the crawl", or "it" for a root (named in the same line)
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

    fn record_at(&self, now: Instant, failure: Option<&str>, rule: PauseRule) {
        let mut s = self.state();
        // only the test's outcome changes an open breaker
        if s.until.is_some() {
            return;
        }
        // room for this one within the window, which may have shrunk since the last
        while s.recent.len() >= rule.window.max(1) {
            s.recent.pop_front();
        }
        s.recent.push_back(failure.is_some());
        if let Some(err) = failure {
            s.last_error = err.to_string();
        }
        let failed = s.recent.iter().filter(|f| **f).count();
        if failed >= rule.failures {
            let pause = rule.min;
            s.failed = failed;
            s.of = s.recent.len();
            s.pause = Some(pause);
            s.until = Some(now + pause);
            let of_what = self.of_root();
            warn!(
                "{} of the last {} fetches{} failed at the gateway: pausing {} for {:?} (last: {})",
                s.failed,
                s.of,
                of_what,
                self.object(),
                pause,
                s.last_error
            );
        }
    }

    fn tested_at(&self, now: Instant, failure: Option<&str>, rule: PauseRule) {
        let mut s = self.state();
        s.testing = false;
        match failure {
            None => {
                s.until = None;
                s.recent.clear();
                s.pause = None;
                match &self.name {
                    Some(name) => info!("Root {} answered the test fetch: crawling it again", name),
                    None => info!("The gateway answered the test fetch: crawling again"),
                }
            }
            Some(err) => {
                s.last_error = err.to_string();
                // never shorter than the rule's first pause, which may have grown since
                let pause = match s.pause {
                    Some(pause) => (pause * 2).min(rule.max).max(rule.min),
                    None => rule.min,
                };
                s.pause = Some(pause);
                s.until = Some(now + pause);
                warn!(
                    "The test fetch{} failed at the gateway: pausing {} for {:?} (last: {})",
                    self.of_root(),
                    self.object(),
                    pause,
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

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    // the default rule with short pauses
    const RULE: PauseRule = PauseRule {
        min: Duration::from_millis(100),
        max: Duration::from_millis(400),
        ..PauseRule::DEFAULT
    };

    // opens it at `at` with RULE's failures
    fn trip(breaker: &Breaker, at: Instant) {
        for _ in 0..RULE.failures {
            breaker.record_at(at, Some("504"), RULE);
        }
    }

    #[test]
    fn fifteen_failures_in_twenty_keep_crawling_and_sixteen_pause() {
        let breaker = Breaker::new();
        let t = Instant::now();
        for _ in 0..5 {
            breaker.record_at(t, None, RULE);
        }
        for _ in 0..15 {
            breaker.record_at(t, Some("504"), RULE);
        }
        assert_eq!(breaker.permit_at(t), Permit::Go);
        // the oldest success falls out: 16 of the last 20
        breaker.record_at(t, Some("504"), RULE);
        assert_eq!(breaker.permit_at(t), Permit::Wait);
    }

    #[test]
    fn a_pause_gives_one_test_when_it_ends() {
        let breaker = Breaker::new();
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
        let breaker = Breaker::new();
        let t = Instant::now();
        trip(&breaker, t);
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
        breaker.tested_at(t + ms(100), Some("504"), RULE); // the next pause is 200ms
        assert_eq!(breaker.permit_at(t + ms(300)), Permit::Test);
        breaker.tested_at(t + ms(300), None, RULE);
        assert_eq!(breaker.permit_at(t + ms(300)), Permit::Go);
        assert_eq!(breaker.status_at(t + ms(300)), None);
        // the record was cleared, and the next pause is the shortest again
        for _ in 0..RULE.failures - 1 {
            breaker.record_at(t + ms(300), Some("504"), RULE);
        }
        assert_eq!(breaker.permit_at(t + ms(300)), Permit::Go);
        breaker.record_at(t + ms(300), Some("504"), RULE);
        assert_eq!(breaker.permit_at(t + ms(399)), Permit::Wait);
        assert_eq!(breaker.permit_at(t + ms(400)), Permit::Test);
    }

    #[test]
    fn a_failed_test_doubles_the_pause_up_to_the_cap() {
        let breaker = Breaker::new();
        let t = Instant::now();
        trip(&breaker, t);
        let mut at = t + ms(100);
        for pause in [200, 400, 400] {
            assert_eq!(breaker.permit_at(at), Permit::Test);
            breaker.tested_at(at, Some("504"), RULE);
            assert_eq!(breaker.permit_at(at + ms(pause - 1)), Permit::Wait);
            at += ms(pause);
        }
        assert_eq!(breaker.permit_at(at), Permit::Test);
    }

    #[test]
    fn a_handed_back_test_goes_to_the_next_caller() {
        let breaker = Breaker::new();
        let t = Instant::now();
        trip(&breaker, t);
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
        breaker.hand_back();
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
    }

    #[test]
    fn fetches_finishing_during_a_pause_change_nothing() {
        let breaker = Breaker::new();
        let t = Instant::now();
        trip(&breaker, t);
        for _ in 0..RULE.window {
            breaker.record_at(t + ms(10), None, RULE);
        }
        assert_eq!(breaker.permit_at(t + ms(50)), Permit::Wait, "not closed");
        for _ in 0..RULE.window {
            breaker.record_at(t + ms(60), Some("504"), RULE);
        }
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test, "not extended");
    }

    #[test]
    fn only_one_of_many_workers_gets_the_test() {
        let breaker = Arc::new(Breaker::new());
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
        let breaker = Breaker::new();
        let rule = PauseRule {
            min: secs(240),
            ..PauseRule::DEFAULT
        };
        let t = Instant::now();
        assert_eq!(breaker.status_at(t), None);
        for _ in 0..4 {
            breaker.record_at(t, None, rule);
        }
        for _ in 0..rule.failures {
            breaker.record_at(t, Some("504 Gateway Timeout"), rule);
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
        let breaker = Breaker::new();
        let t = Instant::now();
        assert!(!breaker.blocks_at(t));
        trip(&breaker, t);
        assert!(breaker.blocks_at(t));
        assert!(!breaker.blocks_at(t + ms(100)));
        assert_eq!(breaker.permit_at(t + ms(100)), Permit::Test);
        assert!(breaker.blocks_at(t + ms(100)));
    }

    #[test]
    fn a_smaller_window_trips_sooner() {
        let breaker = Breaker::new();
        let rule = PauseRule {
            window: 4,
            failures: 3,
            ..RULE
        };
        let t = Instant::now();
        breaker.record_at(t, Some("504"), rule);
        breaker.record_at(t, Some("504"), rule);
        assert_eq!(breaker.permit_at(t), Permit::Go);
        breaker.record_at(t, Some("504"), rule);
        assert_eq!(breaker.permit_at(t), Permit::Wait);
    }

    #[test]
    fn a_rule_change_applies_on_the_next_record() {
        let breaker = Breaker::new();
        let t = Instant::now();
        for _ in 0..14 {
            breaker.record_at(t, Some("504"), PauseRule::DEFAULT);
        }
        assert_eq!(breaker.permit_at(t), Permit::Go);
        // 15 failures would not trip the default of 16, so only the lowered rule can
        let rule = PauseRule {
            failures: 15,
            ..PauseRule::DEFAULT
        };
        breaker.record_at(t, Some("504"), rule);
        assert_eq!(breaker.permit_at(t), Permit::Wait);

        // a shrunk window drops the oldest outcomes before counting
        let breaker = Breaker::new();
        breaker.record_at(t, Some("504"), PauseRule::DEFAULT);
        breaker.record_at(t, Some("504"), PauseRule::DEFAULT);
        assert_eq!(breaker.permit_at(t), Permit::Go);
        let rule = PauseRule {
            window: 2,
            failures: 2,
            ..PauseRule::DEFAULT
        };
        breaker.record_at(t, Some("504"), rule);
        assert_eq!(breaker.permit_at(t), Permit::Wait);
        assert_eq!(
            breaker.status_at(t).unwrap(),
            "paused 1m: 2 of the last 2 fetches failed; last: 504"
        );
    }

    #[test]
    fn the_pause_doubles_up_to_the_rules_max() {
        let breaker = Breaker::new();
        let rule = PauseRule {
            min: secs(10),
            max: secs(25),
            ..PauseRule::DEFAULT
        };
        let t = Instant::now();
        for _ in 0..rule.failures {
            breaker.record_at(t, Some("504"), rule);
        }
        let mut at = t;
        for pause in [10, 20, 25, 25] {
            assert_eq!(breaker.permit_at(at + secs(pause) - ms(1)), Permit::Wait);
            at += secs(pause);
            assert_eq!(breaker.permit_at(at), Permit::Test);
            breaker.tested_at(at, Some("504"), rule);
        }
    }
}
