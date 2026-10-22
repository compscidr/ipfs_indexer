# Pause per root (#286) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A root whose fetches keep failing at the gateway pauses only its own pages; the crawl as a whole pauses only when every root with pages queued is paused.

**Architecture:**
- **Store:** the queue records each item's root in a new indexed column. A new pop takes the oldest item outside a set of roots to skip.
- **`Pauses`:** a new type (`src/pauses.rs`) holds one #284 `Breaker` per root.
- **`IndexQueue`:** asks `Pauses` which roots to skip, pops, then takes that root's permit. A page lost to a race goes back in its old place.
- **`/status` and `/roots`:** each gains a field.

**Tech Stack:** Rust, rusqlite, actix-web; tests run with `cargo test`.

**Spec:** `docs/superpowers/specs/2026-10-22-pause-per-root-design.md`

## Global Constraints

- Rule per root, unchanged from #284: `WINDOW = 20`, `TRIP = 16`, `MIN_PAUSE = 60 s`, `MAX_PAUSE = 15 min`, one test at a time per root.
- Unchanged: what counts as a gateway error (`is_gateway_error`, `Fetched::GatewayError`), `MAX_ATTEMPTS = 3`, and dropping missing, skipped or blocked pages.
- With no root paused, pop order is exactly today's: lowest rowid first.
- Pop cost must not grow with how many pages a skipped root holds.
- `/roots` line: `cid\tindexed\tqueued\ttitle\tpause`, where pause is the root's status or `-`. Tabs and newlines in it become spaces.
- `/status`: `… Discovered: N Paused roots: N Gateway: <gw> (<health>) Version: …`.
- When the whole crawl is paused, the gateway health text starts with `paused`, because darksearch #80 matches on that.
- Release: version `0.5.2` in `Cargo.toml` and `Cargo.lock`; README documents the change.
- Commits end with `Co-Authored-By: <your model> <noreply@anthropic.com>`.

## Review Focus

1. **A paused root whose last queued page was just popped:** the crawl must not count as paused. Its breaker must also not be pruned while open. Test in Task 2: `prune_keeps_open_breakers`.
2. **A huge paused root at the head of the queue:** the pop stays fast. Test in Task 1: `taking_skips_a_root_without_scanning_its_pages`, a query-plan check.
3. **The migration on an index from v0.5.1, whose queue items have paths:** each row's root is the part before the first `/`, and a bare CID is its own root. Test in Task 1: `a_queue_without_root_gains_it_on_open`.
4. **Every root failing, as when kubo is down:** health reads paused, and the resolver waits. Tests in Task 3: `every_root_failing_pauses_the_crawl`, and `the_resolver_waits_while_the_crawl_is_paused` rewritten.
5. **A test turn whose popped page is blocked, or a test that panics:** the test is handed back to that root, or counted as that root's failure. Tests in Task 3: the two #284 tests rewritten per root.

---

### Task 1: The queue records each item's root

**Files:**
- Modify: `src/store.rs`: the `queue` schema in `SCHEMA` (~line 108), `init` migrations (~lines 197–211), `queue_add`, `queue_retry`, `queue_pop` (~lines 629–726), and tests.

**Interfaces:**
- Produces:
  - `pub struct Queued { pub item: String, pub attempts: u32, pub rowid: i64 }`.
  - `Store::queue_take(&self, skip: &[String]) -> rusqlite::Result<Option<Queued>>`: removes and returns the oldest item whose root isn't in `skip`, keeping `roots` and the queued counter in step as `queue_pop` does.
  - `Store::queue_put_back(&self, taken: &Queued) -> rusqlite::Result<()>`: reinserts the item with its old rowid and attempts. If the item was queued again meanwhile, that copy wins and nothing is inserted.
  - `Store::queued_roots(&self) -> rusqlite::Result<Vec<String>>`: the roots with `roots.queued > 0`.
  - `Store::queue_pop(&self)` stays, as `queue_take(&[])` mapped to `(item, attempts)`. Existing callers and tests are unchanged.

- [ ] **Step 1: Write the failing tests** in `store.rs`'s test module.
  - `a_queue_without_root_gains_it_on_open`:
    - Build a v0.5.1-shaped index: open a store, `queue_add` `["a/x/y", "b", "a/z"]`, drop it.
    - Using a raw `Connection`, run `CREATE TABLE q2 (item TEXT PRIMARY KEY, attempts INTEGER NOT NULL DEFAULT 0); INSERT INTO q2 SELECT item, attempts FROM queue ORDER BY rowid; DROP TABLE queue; ALTER TABLE q2 RENAME TO queue;`.
    - Reopen. Assert `SELECT root FROM queue ORDER BY rowid` is `["a", "b", "a"]`.
    - Assert three `queue_pop()` calls return `a/x/y`, `b`, `a/z` in that order.
  - `queue_add_and_retry_set_the_root`: after `queue_add(["c/1"])` and `queue_retry("d/2", 1)`, the `root` column reads `c` and `d`.
  - `taking_skips_the_given_roots`:
    - Queue `["a/1", "b/1", "a/2", "c/1"]`.
    - `queue_take(&["a".into()])` returns `b/1`, then `c/1`, then `None`.
    - `queue_len()` is then 2, and `roots` still shows `a` with queued 2.
  - `taking_with_nothing_skipped_keeps_rowid_order`: same queue; `queue_take(&[])` gives `a/1`, `b/1`, `a/2`, `c/1`.
  - `a_put_back_item_keeps_its_place_and_attempts`:
    - Queue `["a/1", "a/2"]`; `queue_retry("a/0", 2)` puts it last.
    - Take `a/1` and put it back.
    - `queue_take(&[])` again returns `a/1` with its original attempts. `queue_len()` and the `roots` counts are back where they were before the take.
  - `putting_back_an_item_queued_again_keeps_the_new_copy`: take `a/1`, `queue_add(["a/1"])`, then put back the taken copy. Exactly one `a/1` is queued, at the end, with attempts 0.
  - `taking_skips_a_root_without_scanning_its_pages`:
    - `EXPLAIN QUERY PLAN` on the per-root lookup SQL that `queue_take` uses must mention the `(root, rowid)` index, and must not say `SCAN queue`.
    - Expose the SQL as a `const` so the test and the code share it.
  - The existing `CREATE VIEW queue` test (~line 2545) must still pass unchanged. The view has no `root` column, so the migration has to run after `fill_roots`, as the `attempts` migration does.

- [ ] **Step 2: Run them and see them fail**
  - Run: `cargo test --lib store::`
  - Expected: the new tests fail to compile (no `queue_take`, `Queued`, `queue_put_back`) or fail.

- [ ] **Step 3: Implement**
  - **Schema:** `queue` gains `root TEXT NOT NULL DEFAULT ''`, and `CREATE INDEX IF NOT EXISTS queue_root ON queue (root, rowid)` joins the index batch in `init`.
  - **Migration:** if `pragma_table_info('queue')` has no `root`, then in one transaction:
    1. `ALTER TABLE queue ADD COLUMN root TEXT NOT NULL DEFAULT ''`;
    2. set `root` to the part of `item` before its first `/`, or the whole item, for every row;
    3. create the index.
    - Place it after the `attempts` migration.
  - **Writes:** `queue_add` and `queue_retry` write `root_of(item)`.
  - **`queue_take`:**
    - With `skip` empty, keep today's `min(rowid)` statement.
    - Otherwise, read `queued_roots`, drop the skipped ones, and look up each remaining root's `min(rowid)` through the index. Take the smallest, then `DELETE … WHERE rowid = ? RETURNING item, attempts`.
    - All of this happens in one transaction under the connection lock.
  - **`queue_put_back`:** `INSERT OR IGNORE INTO queue (rowid, item, attempts, root)`. Bump `roots` and the counter only if a row was inserted. If the old rowid has been reused, fall back to an ordinary insert at the end. SQLite's `AUTOINCREMENT`-less rowid can reuse the max rowid after a delete; a test isn't required for that case, but the code must not error on it.

- [ ] **Step 4: Run the store tests and the whole suite**
  - Run: `cargo test --lib store::`, then `cargo test`.
  - Expected: all pass.

- [ ] **Step 5: Commit**
  - `git commit -am "Store: record each queued item's root; take the oldest item outside a set of roots, and put one back in its place (#286)"`

### Task 2: `Pauses`, one breaker per root

**Files:**
- Modify: `src/breaker.rs`: a name in the log lines, and the new helpers below.
- Create: `src/pauses.rs`, and add `mod pauses;` in `src/main.rs`, next to `mod breaker;`.

**Interfaces:**
- Consumes: `Breaker`, `Permit` from `src/breaker.rs`.
- Produces, in `breaker.rs`:
  - `Breaker::named(name: &str, min: Duration, max: Duration) -> Breaker`. Log lines say `root <name>` where they now say `the crawl`, for example `16 of the last 20 fetches of root <name> failed at the gateway: pausing it for 60s (last: …)` and `Root <name> answered the test fetch: crawling it again`. `Breaker::with` stays and names nothing, keeping today's wording.
  - `Breaker::blocks(&self) -> bool`: true when `permit()` would say `Wait` (open with time left, or a test out). It changes nothing.
  - `Breaker::is_open(&self) -> bool`: true while paused or testing.
  - `Breaker::last_error(&self) -> String`.
- Produces, in `pauses.rs`, `pub struct Pauses` with:
  - `Pauses::new()`: the production durations. `Pauses::with(min: Duration, max: Duration)`: for tests.
  - `skip(&self) -> Vec<String>`: the roots whose breaker `blocks()`.
  - `permit(&self, root: &str) -> Permit`: `Go` for a root with no breaker.
  - `record(&self, root: &str, failure: Option<&str>)`: creates the root's breaker on its first record.
  - `tested(&self, root: &str, failure: Option<&str>)` and `hand_back(&self, root: &str)`.
  - `status(&self, root: &str) -> Option<String>`: the breaker's `status()`.
  - `paused_count(&self) -> usize`: open breakers.
  - `prune(&self, queued: &[String])`: drops closed breakers whose root isn't in `queued`. Open ones always stay.
  - `crawl_paused(&self, queued: &[String]) -> Option<String>`: `Some` only when `queued` is non-empty and every root in it has an open breaker. The text is `format!("paused: all {} roots with pages queued; last: {}", queued.len(), last_error)`, where `last_error` is the most recent failure recorded or tested on any root.

- [ ] **Step 1: Write the failing tests** in `pauses.rs`, using `Pauses::with(ms(50), ms(200))` and a `trip(&pauses, root)` helper that records 16 failures.
  - `a_failing_root_pauses_only_itself`: trip `a`; `skip()` is `["a"]`; `permit("b")` is `Go`; `permit("a")` is `Wait`.
  - `a_root_due_for_its_test_is_not_skipped`: trip `a`, sleep 55 ms. `skip()` is empty. `permit("a")` is `Test`, and after that `skip()` is `["a"]`.
  - `crawl_paused_only_when_every_queued_root_is_paused`:
    - Trip `a`.
    - `crawl_paused(&["a","b"])` is `None`. `crawl_paused(&["a"])` starts with `"paused: all 1 roots with pages queued; last: 504"`. `crawl_paused(&[])` is `None`.
  - `prune_drops_closed_idle_roots`: record one success for `a` and `b`; `prune(&["b"])` leaves `paused_count() == 0`, and `a`'s breaker is gone. Check this through a test-only `len()`.
  - `prune_keeps_open_breakers`: trip `a`; `prune(&[])`; `permit("a")` is still `Wait`, and `crawl_paused(&["a"])` is `Some`.
  - In `breaker.rs`: `blocks_matches_permit_without_taking_the_test`. Trip a breaker: `blocks()` is true. After the pause ends, `blocks()` is false and `permit()` is `Test`. Then `blocks()` is true.

- [ ] **Step 2: Run them and see them fail**
  - Run: `cargo test --lib pauses:: breaker::`
  - Expected: compile errors.

- [ ] **Step 3: Implement**
  - `Pauses` holds a `Mutex<HashMap<String, Breaker>>`, plus a `Mutex<String>` for the last error.
  - Keep each lock short. Workers call `skip()` and `permit()` on every page.

- [ ] **Step 4: Run them, and the existing breaker tests**
  - Run: `cargo test --lib pauses:: breaker::`
  - Expected: all pass, with the existing breaker tests unchanged.

- [ ] **Step 5: Commit**
  - `git commit -am "Pauses: one #284 breaker per root (#286)"`

### Task 3: The crawl pauses per root

**Files:**
- Modify: `src/index_queue.rs`: replace the `breaker` field and its accessors (~lines 379–472); `TestTurn` (~lines 313–345); `process_next` (~line 1039); `gateway_health` (~line 498); the #284 tests (~lines 2320–2460).
- Modify: `src/discovery.rs`: the test `the_resolver_waits_while_the_crawl_is_paused` (~line 610).
- Modify: `src/main.rs`: the per-minute log loop (~line 583) calls `queue.prune_pauses()`.

**Interfaces:**
- Consumes:
  - `Pauses` (Task 2).
  - `Store::queue_take`, `queue_put_back`, `queued_roots`, and `Queued` (Task 1).
- Produces:
  - `IndexQueue::pauses(&self) -> &Pauses` (`pub(crate)`), and `set_pauses_for_test(&self, Pauses)`. These replace `breaker()` and `set_breaker_for_test`.
  - `IndexQueue::crawl_paused(&self) -> Option<String>`: `pauses.crawl_paused(&store.queued_roots())`. A store error means `None`, logged and noted.
  - `IndexQueue::paused_roots(&self) -> usize`.
  - `IndexQueue::root_pause(&self, root: &str) -> Option<String>`.
  - `IndexQueue::prune_pauses(&self)`.
  - `fn take_next_skipping(&self, skip: &[String]) -> Option<(Queued, Option<TestTurn<'_>>)>` (private, test-visible). It pops outside `skip`, then takes the root's permit:
    - `Go`: returns the page with no turn.
    - `Test`: returns it with a `TestTurn` for that root.
    - `Wait`: puts it back and returns `None`.
  - `TestTurn` gains the root and calls `pauses.tested(root, …)` and `pauses.hand_back(root)`.

- [ ] **Step 1: Rewrite and add the tests** in `index_queue.rs`.
  - **Rewrite the #284 tests per root,** with the same assertions: `missing_pages_never_pause_the_crawl`, `a_test_turn_with_nothing_to_fetch_is_handed_back`, `a_test_fetch_that_panics_pauses_again_instead_of_stalling_the_crawl`, `the_crawl_pauses_while_the_gateway_fails_and_loses_no_pages`.
    - Use `set_pauses_for_test(Pauses::with(..))`.
    - Use `pauses().record(GOOD, …)` and `pauses().permit(GOOD)` in place of the single breaker.
    - The "handed back" test needs a page of GOOD that is blocked rather than an empty queue, since an empty queue now never takes the test. Use a GOOD page blocked through `set_local_denylist`, or another way the test helpers allow. Assert `pauses().permit(GOOD) == Permit::Test` afterwards.
    - The whole-gateway test (16 failures, every page back, `gateway_health()` starts with `"paused "`) stays true with one root, GOOD.
  - **New `one_failing_root_does_not_stop_the_others`:**
    - Queue 20 pages of root `GOOD` (`/wiki/P{i}`), then 5 pages of root `QM`.
    - Use a test gateway that answers 504 for paths containing `GOOD` and 200 HTML otherwise; adapt `serve_flaky` into a `serve_failing_root(root)`.
    - Run `process_next` until it returns false. Assert:
      - all 5 `QM` pages are indexed;
      - `paused_roots() == 1` and `root_pause(GOOD)` starts with `"paused "`;
      - `crawl_paused()` is `None` before the `QM` pages are done;
      - `queue_length()` equals the `GOOD` pages still queued, and none were lost: indexed plus queued plus dropped after 3 attempts equals 20.
  - **New `every_root_failing_pauses_the_crawl`:** with every page answering 504 across two roots, after both trip:
    - `crawl_paused()` starts with `"paused: all 2 roots with pages queued"`;
    - `gateway_health()` starts with `"paused"`;
    - `process_next` returns false.
  - **New `a_page_whose_test_was_taken_goes_back_in_place`:**
    - Trip GOOD with a 10 ms pause and sleep 15 ms.
    - Call `pauses().permit(GOOD)` to take the test, as another worker would.
    - `take_next_skipping(&[])` returns `None`.
    - `store().queue_pop()` then returns the same GOOD page with its attempts.
  - **New `a_resumed_root_crawls_again`:** trip GOOD (10 ms pause), sleep, gateway up; one `process_next` passes the test, then `pauses().permit(GOOD) == Permit::Go` and `paused_roots() == 0`.
  - **In `discovery.rs`, `the_resolver_waits_while_the_crawl_is_paused`:** queue one page of a root, trip that root through `queue.pauses()`, and assert the same as now.

- [ ] **Step 2: Run them and see them fail**
  - Run: `cargo test --lib index_queue:: discovery::`
  - Expected: compile errors, since `pauses()` doesn't exist yet.

- [ ] **Step 3: Implement**
  - **`process_next`:** call `take_next_skipping(&self.pauses().skip())`. On `None`, return false. Otherwise:
    - a blocked item returns true, and its `TestTurn` drop hands the test back;
    - fetch;
    - report through the turn, or `pauses().record(root_of(item), failure)`;
    - then persist, drop or retry, as now.
  - **Pop errors:** a store error from `queue_take` is logged and noted, and returns false, as now.
  - **`gateway_health`:** `crawl_paused()` if `Some`, else today's text.
  - **`main.rs`:** call `queue.prune_pauses()` in the per-minute loop.
  - **Old API:** remove `breaker()`, `set_breaker_for_test` and the `breaker` field.

- [ ] **Step 4: Run the whole suite**
  - Run: `cargo test`, then `cargo clippy --all-targets -- -D warnings`.
  - Expected: all pass, and clippy is clean.

- [ ] **Step 5: Commit**
  - `git commit -am "Crawl: pause only the root that keeps failing; the crawl is paused when every root with pages queued is (#286)"`

### Task 4: `/status`, `/roots`, README and v0.5.2

**Files:**
- Modify: `src/main.rs`: `status` (~line 45), `roots` (~line 120), and their tests (~lines 900 and 1020).
- Modify: `README.md`: the `/status` and `/roots` descriptions, and the #284 pause section.
- Modify: `Cargo.toml` and `Cargo.lock`: version `0.5.2`.

**Interfaces:**
- Consumes: `IndexQueue::paused_roots()` and `IndexQueue::root_pause(root)` (Task 3).

- [ ] **Step 1: Write the failing tests** in `main.rs`.
  - Extend the `/status` test that checks `" Discovered: 1 Gateway: gw.example"` to expect `" Discovered: 1 Paused roots: 0 Gateway: gw.example"`.
  - New `roots_show_each_roots_pause`:
    - Index a page of GOOD, queue one of QM, and trip QM through `queue.pauses()`.
    - `/roots` has GOOD's line ending in `\t-` and QM's fifth field starting with `paused `.
    - Each line has exactly 5 tab-separated fields.

- [ ] **Step 2: Run them and see them fail**
  - Run: `cargo test --bin ipfs_indexer` (or `cargo test` filtered to these names).
  - Expected: FAIL on the missing field.

- [ ] **Step 3: Implement**
  - Add `Paused roots: {}` before `Gateway:` in `/status`.
  - In `/roots`, append `\t` and the root's pause, with tabs and newlines turned into spaces, or `-`.
  - Update the `/roots` doc comment to the 5-field format.
  - **README:**
    - document the 5th `/roots` field and `Paused roots:`;
    - change the #284 section to say the pause is per root, and the crawl counts as paused when every root with pages queued is.
  - Bump the version to `0.5.2`, and run `cargo build` so `Cargo.lock` follows.

- [ ] **Step 4: Run the whole suite and clippy**
  - Run: `cargo test && cargo clippy --all-targets -- -D warnings`
  - Expected: all pass.

- [ ] **Step 5: Commit**
  - `git commit -am "/status: Paused roots; /roots: each root's pause; README; v0.5.2 (#286)"`
