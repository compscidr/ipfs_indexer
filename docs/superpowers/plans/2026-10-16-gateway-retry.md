# Retry gateway errors and pause when the gateway keeps failing (#284): implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A page that fails with a gateway error goes back on the queue, up to 3 attempts in all. While most recent fetches fail, every worker pauses, and one test fetch decides when crawling resumes.

**Architecture:**
- The `queue` table gains an `attempts` column. `queue_pop` returns the item with its attempts, and a new `queue_retry` puts an item back at the end.
- A new `src/breaker.rs` holds the pause: the last 20 outcomes and a pause that doubles. It hands out `Go`, `Wait` and `Test` permits.
- `IndexQueue::process_next` asks for a permit before popping, reports each fetch's outcome, and retries pages on gateway errors.
- `gateway_health` shows the pause.

**Tech Stack:** Rust 2021, rusqlite, reqwest (blocking), log. Tests use `cargo test`, with local one-shot HTTP servers in `src/index_queue.rs` tests.

**Spec:** `docs/superpowers/specs/2026-10-16-gateway-retry-design.md`

## Global Constraints

- At most 3 attempts in all: a page that has failed `attempts` times so far is retried while `attempts + 1 < 3`, and dropped otherwise.
- Gateway error = `is_gateway_error`: 5xx, 429, or no status (timeouts, connection errors). Everything else, 404 included, is not a gateway error and counts as a success for the pause.
- Pause: 16 or more failures in the last 20 fetches opens it. The first pause is 1 min; each failed test doubles it, capped at 15 min; a test that works resets it to 1 min and clears the record.
- Only the test's outcome changes an open breaker. Outcomes recorded while it's open are ignored.
- Thresholds are constants, not settings.
- `/status` texts: `paused 4m: 16 of the last 20 fetches failed; last: <error>` and `testing after 16 of the last 20 fetches failed; last: <error>`. Otherwise unchanged.
- Logs:
  - a warning when a pause starts and an info line when crawling resumes;
  - debug per retried page;
  - a warning per page dropped after 3 attempts.
  - Gateway errors no longer log the two per-page warnings.
- No format change beyond the added column. API unchanged. Release v0.4.5.
- `cargo fmt` clean, all tests pass. No new dependencies.
- Commit messages end with `Co-Authored-By: <the model that wrote the commit> <noreply@anthropic.com>`.

## Review Focus

1. **Many workers asking at the moment a pause ends.** Exactly one gets `Test`; the rest wait. Test: Task 2, `only_one_of_many_workers_gets_the_test`.
2. **A v0.4.4 index on disk** (queue without `attempts`). It opens with the column added, the order kept and rows at 0, and a retried page keeps its count across a restart. Test: Task 1, `a_queue_from_before_attempts_gains_the_column`.
3. **A page queued again by a link while its retry is pending.** One copy, and the one already queued wins. Test: Task 1, `queue_retry_goes_to_the_back_and_keeps_counts`.
4. **A pause ending when there's nothing to fetch.** The test turn isn't stuck: it goes to the next worker. Test: Task 3, `a_test_turn_with_nothing_to_fetch_is_handed_back`.
5. **Lots of 404s** (a mirror with many dead links). These never pause the crawl. Test: Task 3, `missing_pages_never_pause_the_crawl`.

---

### Task 1: Attempts in the queue (`src/store.rs`)

**Files:**
- Modify: `src/store.rs`: the `SCHEMA` queue table (about line 107), `Store::init` (after `conn.execute_batch(SCHEMA)?;`, about line 182), `queue_pop` (about line 655), plus a new `queue_retry` after `queue_pop`. Tests are in the `tests` module of the same file.
- Modify: `src/index_queue.rs`: the callers of `queue_pop`, at about lines 924, 1876 and 2155.

**Interfaces:**
- Produces:
  - `Store::queue_pop(&self) -> rusqlite::Result<Option<(String, u32)>>`: the item and its failed attempts so far.
  - `Store::queue_retry(&self, item: &str, attempts: u32) -> rusqlite::Result<bool>`: true if it was added, false if the item is already queued.

- [ ] **Step 1: Write the failing tests** (in `src/store.rs` `mod tests`, next to `queue_round_trips_in_order_and_ignores_duplicates`)

```rust
    #[test]
    fn queue_retry_goes_to_the_back_and_keeps_counts() {
        let dir = temp_dir("queue-retry");
        let store = Store::open(&dir, 0).unwrap();
        store
            .queue_add(&["a/1".into(), "a/2".into(), "b/1".into()])
            .unwrap();
        assert_eq!(store.queue_pop().unwrap(), Some(("a/1".to_string(), 0)));
        assert!(store.queue_retry("a/1", 1).unwrap());
        assert_eq!(store.queue_len(), 3);
        assert_eq!(store.queue_load().unwrap(), vec!["a/2", "b/1", "a/1"]);
        assert_eq!(roots_table(&store), roots_by_scan(&store));
        // a copy already queued (a link found it again) wins, attempts and all
        assert!(!store.queue_retry("a/2", 2).unwrap());
        assert_eq!(store.queue_len(), 3);
        assert_eq!(roots_table(&store), roots_by_scan(&store));
        assert_eq!(store.queue_pop().unwrap(), Some(("a/2".to_string(), 0)));
    }

    #[test]
    fn a_queue_from_before_attempts_gains_the_column() {
        let dir = temp_dir("queue-attempts");
        let store = Store::open(&dir, 0).unwrap();
        store.queue_add(&["a/1".into(), "a/2".into()]).unwrap();
        drop(store);
        {
            // v0.4.4's queue: no attempts column
            let conn = Connection::open(dir.join("index.sqlite")).unwrap();
            conn.execute_batch(
                "ALTER TABLE queue RENAME TO queue_old;
                 CREATE TABLE queue (item TEXT PRIMARY KEY);
                 INSERT INTO queue (item) SELECT item FROM queue_old ORDER BY rowid;
                 DROP TABLE queue_old;",
            )
            .unwrap();
        }
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(store.queue_pop().unwrap(), Some(("a/1".to_string(), 0)));
        store.queue_retry("a/1", 1).unwrap();
        drop(store);
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(store.queue_pop().unwrap(), Some(("a/2".to_string(), 0)));
        assert_eq!(
            store.queue_pop().unwrap(),
            Some(("a/1".to_string(), 1)),
            "kept across a restart"
        );
    }
```

And update the existing `queue_round_trips_in_order_and_ignores_duplicates` (about line 1501), whose last pops become:

```rust
        assert_eq!(store.queue_pop().unwrap(), Some(("b/1".to_string(), 0)));
        assert_eq!(store.queue_pop().unwrap(), Some(("c/3".to_string(), 0)));
        assert_eq!(store.queue_pop().unwrap(), None);
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --quiet queue_ 2>&1 | tail -20`
Expected: compile errors. `queue_retry` doesn't exist yet, and `queue_pop` returns `Option<String>`.

- [ ] **Step 3: Implement**

In `SCHEMA`, replace the queue table with:

```rust
    -- the crawl frontier (#224): items waiting to be fetched, in insertion order
    CREATE TABLE IF NOT EXISTS queue (
        item     TEXT PRIMARY KEY,
        -- failed fetches so far (#284)
        attempts INTEGER NOT NULL DEFAULT 0
    );";
```

In `Store::init`, right after `conn.execute_batch(SCHEMA)?;`:

```rust
        // v0.4.4 and before had no attempts (#284); adding the column rewrites no rows
        let has_attempts: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('queue') WHERE name = 'attempts'")?
            .exists([])?;
        if !has_attempts {
            conn.execute_batch("ALTER TABLE queue ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0")?;
        }
```

Replace `queue_pop` with this, then add `queue_retry` after it:

```rust
    /// Takes the oldest item off the frontier with its failed attempts so far, or None if it's
    /// empty
    pub fn queue_pop(&self) -> rusqlite::Result<Option<(String, u32)>> {
        let start = Instant::now();
        let mut conn = self.conn.lock().unwrap();
        let waited = start.elapsed();
        let tx = conn.transaction()?;
        let next: Option<(String, u32)> = tx
            .prepare_cached(
                "DELETE FROM queue WHERE rowid = (SELECT min(rowid) FROM queue)
                 RETURNING item, attempts",
            )?
            .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((item, _)) = &next {
            bump_root(&tx, item, 0, -1)?;
        }
        tx.commit()?;
        // not on empty polls: idle workers poll every 200ms
        if next.is_some() {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            debug!(
                "Store queue_pop: waited {:?}, took {:?}",
                waited,
                start.elapsed() - waited
            );
        }
        Ok(next)
    }

    /// Puts an item back at the end of the frontier with its failed attempts (#284). False if
    /// it's queued already: that copy, and its count, win.
    pub fn queue_retry(&self, item: &str, attempts: u32) -> rusqlite::Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let n = tx
            .prepare_cached("INSERT OR IGNORE INTO queue (item, attempts) VALUES (?1, ?2)")?
            .execute(params![item, attempts])?;
        bump_root(&tx, item, 0, n as i64)?;
        tx.commit()?;
        self.queued.fetch_add(n, Ordering::Relaxed);
        Ok(n == 1)
    }
```

In `src/index_queue.rs`:
- `process_next` (about line 924): change `Ok(Some(item)) => item,` to `Ok(Some((item, _attempts))) => item,`. Task 3 uses the attempts.
- The test at about line 1876: change `queue.store().queue_pop().unwrap().unwrap(),` to `queue.store().queue_pop().unwrap().unwrap().0,`.
- The `crawl` test helper at about line 2155: change it to `std::iter::from_fn(|| queue.store().queue_pop().unwrap().map(|(item, _)| item)).collect()`.

- [ ] **Step 4: Run all tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | tail -5`
Expected: all pass. That's the 197 already there, plus the 2 new ones.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs src/index_queue.rs
git commit -m "Queue attempts: queue_pop returns them, queue_retry puts an item back (#284)"
```

---

### Task 2: The breaker (`src/breaker.rs`)

**Files:**
- Create: `src/breaker.rs`
- Modify: `src/main.rs`: add `mod breaker;` in the module list (about line 14, kept alphabetical, so after `mod admin;`).

**Interfaces:**
- Produces:
  - `pub enum Permit { Go, Wait, Test }`, with `Debug, PartialEq, Clone, Copy`.
  - `Breaker::new() -> Breaker`, which uses 1 min and 15 min.
  - `Breaker::with(min: Duration, max: Duration) -> Breaker`.
  - `Breaker::permit(&self) -> Permit`.
  - `Breaker::record(&self, failure: Option<&str>)`, for a `Go` fetch.
  - `Breaker::tested(&self, failure: Option<&str>)`, for a `Test` fetch.
  - `Breaker::hand_back(&self)`.
  - `Breaker::status(&self) -> Option<String>`.

- [ ] **Step 1: Write the module with its tests.** The behavior is all in one small file, so the tests ship with it. Run them in Step 2 against a deliberately broken `TRIP` to see them fail first.

```rust
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
        Breaker {
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

    /// e.g. "paused 4m: 16 of the last 20 fetches failed; last: <error>"; None while crawling
    pub fn status(&self) -> Option<String> {
        self.status_at(Instant::now())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
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
            warn!(
                "{} of the last {} fetches failed at the gateway: pausing the crawl for {:?} (last: {})",
                s.failed, s.of, s.pause, s.last_error
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
                info!("The gateway answered the test fetch: crawling again");
            }
            Some(err) => {
                s.last_error = err.to_string();
                s.pause = (s.pause * 2).min(self.max);
                s.until = Some(now + s.pause);
                warn!(
                    "The test fetch failed at the gateway: pausing the crawl for {:?} (last: {})",
                    s.pause, err
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
        assert_eq!(breaker.permit_at(t + ms(101)), Permit::Wait, "one test at a time");
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
}
```

In `src/main.rs`, add `mod breaker;` after `mod admin;`.

- [ ] **Step 2: See the tests fail, then pass**

Temporarily change `const TRIP: usize = 16;` to `17` and run `cargo test --quiet breaker 2>&1 | tail -15`. Expected: `fifteen_failures_in_twenty_keep_crawling_and_sixteen_pause` and `status_says_why_the_crawl_is_paused` fail, among others. Set it back to `16`.

Run: `cargo fmt && cargo test --quiet breaker 2>&1 | tail -5`
Expected: 8 passed. The module isn't used yet, so expect dead-code warnings for `Breaker`'s public API until Task 3. Leave them; don't add `#[allow]`.

- [ ] **Step 3: Commit**

```bash
git add src/breaker.rs src/main.rs
git commit -m "Breaker: pause the crawl after 16 gateway errors in 20, one test fetch to resume (#284)"
```

---

### Task 3: Retry and pause in the crawl (`src/index_queue.rs`)

**Files:**
- Modify: `src/index_queue.rs`:
  - the imports (top);
  - the `IndexQueue` struct and `new()` (about lines 280–325);
  - `gateway_health` (about line 384);
  - `process_next` (about line 921);
  - `retrieve_content` (about line 954);
  - tests (`mod tests`).

**Interfaces:**
- Consumes:
  - `Store::queue_pop() -> rusqlite::Result<Option<(String, u32)>>` and `Store::queue_retry(&str, u32) -> rusqlite::Result<bool>` (Task 1);
  - `Breaker`, `Permit` and their methods (Task 2).
- Produces:
  - `IndexQueue::process_next(&self, gateway: &str) -> bool`. It now returns false when it took nothing: the queue was empty, or the crawl is paused.
  - `retrieve_content(...) -> Fetched`.

- [ ] **Step 1: Write the failing tests** (in `mod tests` of `src/index_queue.rs`, after `a_missing_page_is_not_a_gateway_error`)

```rust
    fn gateway_of(url: String) -> String {
        url.trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string()
    }

    // a gateway that answers every request, 504 until `up` is set and then an html page naming
    // the path (so pages differ and none is a mirror); counts the requests
    fn serve_flaky() -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let up = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let (is_up, counted) = (Arc::clone(&up), Arc::clone(&hits));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                counted.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if is_up.load(Ordering::SeqCst) {
                    (
                        "200 OK",
                        format!("<title>{0}</title><body>words for {0}</body>", path),
                    )
                } else {
                    ("504 Gateway Timeout", "timeout".to_string())
                };
                let header = format!(
                    "HTTP/1.1 {}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status,
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        (host, up, hits)
    }

    #[test]
    fn a_gateway_error_sends_the_page_to_the_back_of_the_queue() {
        let queue = IndexQueue::new();
        let (a, b) = (format!("{}/wiki/A", GOOD), format!("{}/wiki/B", GOOD));
        queue.enqueue_many(vec![a.clone(), b.clone()]);
        let gateway = gateway_of(serve_status_once(
            "504 Gateway Timeout",
            "text/plain",
            b"x".to_vec(),
        ));
        assert!(queue.process_next(&gateway));
        assert_eq!(queue.store().queue_pop().unwrap(), Some((b, 0)));
        assert_eq!(queue.store().queue_pop().unwrap(), Some((a, 1)));
    }

    #[test]
    fn a_page_is_dropped_after_its_third_gateway_error() {
        let queue = IndexQueue::new();
        let item = format!("{}/wiki/A", GOOD);
        assert!(queue.store().queue_retry(&item, 2).unwrap());
        let gateway = gateway_of(serve_status_once(
            "504 Gateway Timeout",
            "text/plain",
            b"x".to_vec(),
        ));
        assert!(queue.process_next(&gateway));
        assert_eq!(queue.queue_length(), 0);
    }

    #[test]
    fn a_missing_page_is_dropped_at_once() {
        let queue = IndexQueue::new();
        queue.enqueue(format!("{}/wiki/A", GOOD));
        let gateway = gateway_of(serve_status_once(
            "404 Not Found",
            "text/plain",
            b"no".to_vec(),
        ));
        assert!(queue.process_next(&gateway));
        assert_eq!(queue.queue_length(), 0);
    }

    #[test]
    fn a_retried_page_that_fetches_is_indexed() {
        let queue = IndexQueue::new();
        let item = format!("{}/wiki/A", GOOD);
        assert!(queue.store().queue_retry(&item, 1).unwrap());
        let gateway = gateway_of(serve_once(
            "text/html",
            b"<title>t</title><body>words here</body>".to_vec(),
        ));
        assert!(queue.process_next(&gateway));
        assert_eq!(queue.index_length(), 1);
        assert_eq!(queue.queue_length(), 0);
    }

    #[test]
    fn missing_pages_never_pause_the_crawl() {
        let queue = IndexQueue::new();
        for i in 0..20 {
            queue.enqueue(format!("{}/wiki/Gone{}", GOOD, i));
            let gateway = gateway_of(serve_status_once(
                "404 Not Found",
                "text/plain",
                b"no".to_vec(),
            ));
            assert!(queue.process_next(&gateway));
        }
        assert_eq!(queue.breaker().permit(), Permit::Go);
    }

    #[test]
    fn a_test_turn_with_nothing_to_fetch_is_handed_back() {
        let queue = IndexQueue::new();
        queue.set_breaker_for_test(Breaker::with(
            Duration::from_millis(10),
            Duration::from_millis(40),
        ));
        for _ in 0..16 {
            queue.breaker().record(Some("504"));
        }
        std::thread::sleep(Duration::from_millis(15));
        assert!(!queue.process_next("127.0.0.1:1"), "the queue is empty");
        assert_eq!(queue.breaker().permit(), Permit::Test, "the turn came back");
    }

    #[test]
    fn the_crawl_pauses_while_the_gateway_fails_and_loses_no_pages() {
        use std::sync::atomic::Ordering;
        let queue = IndexQueue::new();
        queue.set_breaker_for_test(Breaker::with(
            Duration::from_millis(50),
            Duration::from_millis(200),
        ));
        let items: Vec<String> = (0..20).map(|i| format!("{}/wiki/P{}", GOOD, i)).collect();
        queue.enqueue_many(items);
        let (gateway, up, hits) = serve_flaky();
        for _ in 0..16 {
            assert!(queue.process_next(&gateway));
        }
        assert_eq!(hits.load(Ordering::SeqCst), 16);
        assert_eq!(queue.queue_length(), 20, "every failed page went back");
        assert!(
            queue.gateway_health().starts_with("paused "),
            "{}",
            queue.gateway_health()
        );
        // paused: nothing taken, nothing fetched
        assert!(!queue.process_next(&gateway));
        assert_eq!(hits.load(Ordering::SeqCst), 16);
        assert_eq!(queue.queue_length(), 20);
        up.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        while queue.process_next(&gateway) {}
        assert_eq!(queue.index_length(), 20, "the retried pages were indexed");
        assert_eq!(queue.queue_length(), 0);
        assert!(
            queue.gateway_health().starts_with("ok "),
            "{}",
            queue.gateway_health()
        );
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --quiet gateway 2>&1 | tail -20`
Expected: compile errors. `breaker()`, `set_breaker_for_test`, `Breaker` and `Permit` aren't there yet.

- [ ] **Step 3: Implement**

Imports, at the top of `src/index_queue.rs`, after `use crate::denylist::...`:

```rust
use crate::breaker::{Breaker, Permit};
```

Add a constant near `MAX_BODY_BYTES`:

```rust
// a page the gateway fails on this many times in all is dropped (#284)
const MAX_ATTEMPTS: u32 = 3;
```

Add the fetch outcome type, just above `pub struct IndexQueue`:

```rust
// what a fetch came to (#284): a page to index, nothing to index (missing, skipped, a mirror
// redirect...), or a gateway error, which is worth retrying
enum Fetched {
    Page(IndexResult, Vec<String>),
    Nothing,
    GatewayError(String),
}
```

Add a field to `IndexQueue`, after `throttle`:

```rust
    // pauses the crawl while the gateway keeps failing (#284); set on first use, or by a test
    breaker: OnceLock<Breaker>,
```

In `new()`, add `breaker: OnceLock::new(),` after `throttle: OnceLock::new(),`.

Add these methods after `wait_for_gateway`:

```rust
    fn breaker(&self) -> &Breaker {
        self.breaker.get_or_init(Breaker::new)
    }

    #[cfg(test)]
    pub(crate) fn set_breaker_for_test(&self, breaker: Breaker) {
        let _ = self.breaker.set(breaker);
    }
```

In `gateway_health`, put this first in the body:

```rust
        if let Some(paused) = self.breaker().status() {
            return paused;
        }
```

Replace `process_next` with:

```rust
    /// Fetch and index the next queued item; false if it took none (the queue was empty, or the
    /// crawl is paused while the gateway keeps failing)
    pub fn process_next(&self, gateway: &str) -> bool {
        let start = Instant::now();
        let permit = self.breaker().permit();
        if permit == Permit::Wait {
            return false;
        }
        // a test turn that fetches nothing goes to the next worker
        let unused = || {
            if permit == Permit::Test {
                self.breaker().hand_back();
            }
        };
        // taken off disk now: a crash mid-fetch loses at most one item per worker
        let (item, attempts) = match self.store().queue_pop() {
            Ok(Some(next)) => next,
            Ok(None) => {
                unused();
                return false;
            }
            Err(err) => {
                warn!("Couldn't take the next queued item: {}", err);
                self.store().note_error(&err);
                unused();
                return false;
            }
        };
        if self.is_blocked(&item) {
            unused();
            return true;
        }
        debug!("Indexing {}", item);
        let fetched = self.retrieve_content(gateway.to_string(), item.clone());
        let failure = match &fetched {
            Fetched::GatewayError(err) => Some(err.as_str()),
            _ => None,
        };
        match permit {
            Permit::Test => self.breaker().tested(failure),
            _ => self.breaker().record(failure),
        }
        match fetched {
            // a mirror's links lead to more of the mirror: the original's were followed
            Fetched::Page(result, links) => {
                if self.persist(&item, &result) {
                    self.enqueue_many(links);
                }
            }
            Fetched::Nothing => warn!("Error retrieving CID {}", item),
            Fetched::GatewayError(err) => self.retry(&item, attempts, &err),
        }
        debug!("Done {} in {:?}", item, start.elapsed());
        true
    }

    // a page the gateway failed on goes to the back of the queue, until MAX_ATTEMPTS (#284)
    fn retry(&self, item: &str, attempts: u32, err: &str) {
        let attempts = attempts + 1;
        if attempts >= MAX_ATTEMPTS {
            warn!("Dropping {} after {} attempts: {}", item, attempts, err);
            return;
        }
        debug!("Retrying {} later ({} failed so far): {}", item, attempts, err);
        if let Err(err) = self.store().queue_retry(item, attempts) {
            warn!("Couldn't queue {} again: {}", item, err);
            self.store().note_error(&err);
        }
    }
```

In `retrieve_content`:
- Change the signature to `fn retrieve_content(&self, gateway: String, cid: String) -> Fetched`.
- Update its doc comment to: "Use the http client to obtain the page from the ipfs gateway: the page and its links, nothing to index, or a gateway error to retry (#284)".
- Replace the `let (html, mut base) = match fetched { ... };` block with:

```rust
        let (html, mut base) = match fetched {
            Ok(Some(page)) => page,
            Ok(None) => return Fetched::Nothing,
            Err(err) if is_gateway_error(&err) => return Fetched::GatewayError(err.to_string()),
            Err(err) => {
                warn!("Error: {}, not re-enqueue-ing cid", err);
                return Fetched::Nothing;
            }
        };
```

- Replace its final line, `self.process_content(&base, fullcid.clone(), document.clone())`, with:

```rust
        match self.process_content(&base, fullcid.clone(), document.clone()) {
            Some((result, links)) => Fetched::Page(result, links),
            None => Fetched::Nothing,
        }
```

Leave the `health` / `last_fetch` lines as they are. The existing tests that call `retrieve_content` and ignore its result still compile, since `Fetched` isn't `must_use`.

- [ ] **Step 4: Run all tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | tail -5`
Expected: all pass, with no new warnings. The breaker's dead-code warnings from Task 2 are gone.

- [ ] **Step 5: Commit**

```bash
git add src/index_queue.rs
git commit -m "Retry pages on gateway errors and pause the crawl while the gateway keeps failing (#284)"
```

---

### Task 4: Release v0.4.5

**Files:**
- Modify: `Cargo.toml` (`version = "0.4.4"` → `"0.4.5"`), `Cargo.lock` (updated by the build)
- Modify: `docs/superpowers/specs/2026-10-16-gateway-retry-design.md`: change `**Status:** agreed` to `**Status:** implemented (v0.4.5)`.

- [ ] **Step 1: Bump and build**

Run: `sed -i 's/^version = "0.4.4"/version = "0.4.5"/' Cargo.toml && cargo build --quiet && grep -A1 'name = "ipfs_indexer"' Cargo.lock`
Expected: `version = "0.4.5"`.

- [ ] **Step 2: Full test run**

Run: `cargo fmt --check && cargo test --quiet 2>&1 | tail -3`
Expected: all pass.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock docs/superpowers/specs/2026-10-16-gateway-retry-design.md
git commit -m "v0.4.5"
```
