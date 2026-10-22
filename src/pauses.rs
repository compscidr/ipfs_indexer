/*!
 * One breaker per root (#286): a root whose pages keep failing at the gateway is paused on its
 * own, and the others crawl on. A root's breaker is made on its first outcome and dropped
 * once it is closed and the root has nothing queued; an open one always stays.
 */
use crate::breaker::{Breaker, Permit, MAX_PAUSE, MIN_PAUSE};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

pub struct Pauses {
    min: Duration,
    max: Duration,
    breakers: Mutex<HashMap<String, Breaker>>,
    // the most recent failure recorded or tested on any root
    last_error: Mutex<String>,
}

impl Pauses {
    pub fn new() -> Pauses {
        Pauses::with(MIN_PAUSE, MAX_PAUSE)
    }

    pub fn with(min: Duration, max: Duration) -> Pauses {
        Pauses {
            min,
            max,
            breakers: Mutex::new(HashMap::new()),
            last_error: Mutex::new(String::new()),
        }
    }

    /// The roots to take no page from: paused, or with their test out
    pub fn skip(&self) -> Vec<String> {
        let breakers = self.breakers();
        breakers
            .iter()
            .filter(|(_, b)| b.blocks())
            .map(|(root, _)| root.clone())
            .collect()
    }

    /// Go for a root with no breaker
    pub fn permit(&self, root: &str) -> Permit {
        match self.breakers().get(root) {
            Some(b) => b.permit(),
            None => Permit::Go,
        }
    }

    /// A Go fetch's outcome: Some(error) for a gateway error
    pub fn record(&self, root: &str, failure: Option<&str>) {
        self.note(failure);
        self.breakers()
            .entry(root.to_string())
            .or_insert_with(|| Breaker::named(root, self.min, self.max))
            .record(failure);
    }

    /// The test fetch's outcome
    pub fn tested(&self, root: &str, failure: Option<&str>) {
        self.note(failure);
        if let Some(b) = self.breakers().get(root) {
            b.tested(failure);
        }
    }

    /// Gives the root's test back unused
    pub fn hand_back(&self, root: &str) {
        if let Some(b) = self.breakers().get(root) {
            b.hand_back();
        }
    }

    pub fn status(&self, root: &str) -> Option<String> {
        self.breakers().get(root)?.status()
    }

    /// How many roots are paused or testing
    pub fn paused_count(&self) -> usize {
        self.breakers().values().filter(|b| b.is_open()).count()
    }

    /// Drops the closed breakers of roots not in `queued`
    pub fn prune(&self, queued: &[String]) {
        self.breakers()
            .retain(|root, b| b.is_open() || queued.contains(root));
    }

    /// Some only when `queued` is not empty and every root in it is paused or testing
    pub fn crawl_paused(&self, queued: &[String]) -> Option<String> {
        if queued.is_empty() {
            return None;
        }
        let all_open = {
            let breakers = self.breakers();
            queued
                .iter()
                .all(|root| breakers.get(root).is_some_and(|b| b.is_open()))
        };
        if !all_open {
            return None;
        }
        let last = self.last_error.lock().unwrap_or_else(|e| e.into_inner());
        Some(format!(
            "paused: all {} roots with pages queued; last: {}",
            queued.len(),
            last
        ))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.breakers().len()
    }

    fn note(&self, failure: Option<&str>) {
        if let Some(err) = failure {
            *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = err.to_string();
        }
    }

    fn breakers(&self) -> MutexGuard<'_, HashMap<String, Breaker>> {
        self.breakers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::Permit;
    use std::time::Duration;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn trip(pauses: &Pauses, root: &str) {
        for _ in 0..16 {
            pauses.record(root, Some("504"));
        }
    }

    fn roots(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_failing_root_pauses_only_itself() {
        let pauses = Pauses::with(ms(50), ms(200));
        trip(&pauses, "a");
        assert_eq!(pauses.skip(), roots(&["a"]));
        assert_eq!(pauses.permit("b"), Permit::Go);
        assert_eq!(pauses.permit("a"), Permit::Wait);
    }

    #[test]
    fn a_root_due_for_its_test_is_not_skipped() {
        let pauses = Pauses::with(ms(50), ms(200));
        trip(&pauses, "a");
        std::thread::sleep(ms(55));
        assert!(pauses.skip().is_empty());
        assert_eq!(pauses.permit("a"), Permit::Test);
        assert_eq!(pauses.skip(), roots(&["a"]));
    }

    #[test]
    fn crawl_paused_only_when_every_queued_root_is_paused() {
        let pauses = Pauses::with(ms(50), ms(200));
        trip(&pauses, "a");
        assert_eq!(pauses.crawl_paused(&roots(&["a", "b"])), None);
        let text = pauses.crawl_paused(&roots(&["a"])).unwrap();
        assert!(
            text.starts_with("paused: all 1 roots with pages queued; last: 504"),
            "{}",
            text
        );
        assert_eq!(pauses.crawl_paused(&[]), None);
    }

    #[test]
    fn prune_drops_closed_idle_roots() {
        let pauses = Pauses::with(ms(50), ms(200));
        pauses.record("a", None);
        pauses.record("b", None);
        pauses.prune(&roots(&["b"]));
        assert_eq!(pauses.paused_count(), 0);
        assert_eq!(pauses.len(), 1);
    }

    #[test]
    fn prune_keeps_open_breakers() {
        let pauses = Pauses::with(ms(50), ms(200));
        trip(&pauses, "a");
        pauses.prune(&[]);
        assert_eq!(pauses.permit("a"), Permit::Wait);
        assert!(pauses.crawl_paused(&roots(&["a"])).is_some());
    }
}
