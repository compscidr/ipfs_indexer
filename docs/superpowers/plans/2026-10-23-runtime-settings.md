# Runtime Settings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every tunable indexer setting, the seeds and the local denylist can be seen and changed
from darksearch's admin, with admin override > env (iac) > default.

**Architecture:**
- A new `src/settings.rs` holds one registry of settings: name, env var, range, default, and live
  or restart. It resolves each value from an admin override stored in `index.sqlite`, then the
  env, then the default.
- Consumers read their value on each use, so a change is live. The few restart settings stop the
  actix server cleanly and Docker's `unless-stopped` policy starts it again.
- New token-protected `/admin/settings`, `/admin/seeds` and `/admin/denylist` endpoints return
  JSON built with `serde_json::json!`. darksearch gets a Settings page on top of them.

**Tech Stack:** Rust (actix-web 4, rusqlite bundled, serde_json, reqwest blocking); PHP for
darksearch.

**Spec:** `docs/superpowers/specs/2026-10-23-runtime-settings-design.md`

## Global Constraints
- Precedence: admin override, then env, then built-in default. Reset removes the override.
- `ETH_RPC_URL`'s value is never returned, logged, or shown. Logs say only `ETH RPC URL changed
  by <actor>`.
- Not settings: `DATA_DIR`, `BIND_ADDR`, `SEEDS_FILE`/`LOCAL_DENYLIST` paths, `ADMIN_TOKEN`,
  index internals, search scoring, the badbits URL.
- Ranges include every default and every value iac sets today (`CRAWL_WORKERS=60`,
  `ETH_LOGS_MAX_BLOCKS=10`, `ETH_REQUEST_SECS=6`), and the 0s meaning "no limit" or "off".
- A bad env value is fatal at startup, including `INDEX_MAX_AGE_DAYS`, which falls back to 30
  today. A stored override that fails validation at startup is logged, ignored, and reported as
  `ignored: <reason>`; it never stops startup.
- Every set or reset is logged: `Setting <name> set to <value> by <actor>` or
  `Setting <name> reset by <actor>`.
- No change-history table.
- Without `DATA_DIR`: set, reset and seed edits return 400 `settings need DATA_DIR`. Reads still
  work.
- The actor comes from the `X-Admin-Actor` header. It's required on writes: missing or empty
  gets 400.
- No WAL and no backups. New tables go in `SCHEMA` (`CREATE TABLE IF NOT EXISTS`), which is also
  the migration for existing stores.
- Clippy: no new warnings (main has existing ones). Tests run with `cargo test <filter>`; the
  crate is bin-only, so there's no `--lib`.
- Two PRs: the indexer (Tasks 1–8, version 0.6.0), then darksearch (Task 9). The indexer PR keeps
  the existing admin endpoints unchanged.

## The registry (used by every task)
`name` is the API id; Env is the variable; the range is inclusive.

| name | Env | Default | Range | Applies |
|---|---|---|---|---|
| `crawl_workers` | `CRAWL_WORKERS` | 10 | 1–500 | live |
| `gateway_requests_per_min` | `GATEWAY_REQUESTS_PER_MIN` | 0 own node, 100 public gateway | 0–100000 | live |
| `discovery_per_hour` | `DISCOVERY_PER_HOUR` | 500 | 0–100000 | live |
| `health_min_rate` | `HEALTH_MIN_RATE` | 500 | 0–1000000 | live |
| `health_grace_secs` | `HEALTH_GRACE_SECS` | 900 | 0–86400 | live |
| `index_max_age_days` | `INDEX_MAX_AGE_DAYS` | 30 | 1–3650 | live |
| `index_cache_mb` | `INDEX_CACHE_MB` | 1024 | 0–65536 | live (re-applied as a pragma) |
| `pause_window` | `PAUSE_WINDOW` | 20 | 1–1000 | live |
| `pause_failures` | `PAUSE_FAILURES` | 16 | 1–1000, ≤ `pause_window` | live |
| `pause_min_secs` | `PAUSE_MIN_SECS` | 60 | 1–86400 | live |
| `pause_max_secs` | `PAUSE_MAX_SECS` | 900 | 1–86400, ≥ `pause_min_secs` | live |
| `page_attempts` | `PAGE_ATTEMPTS` | 3 | 1–100 | live |
| `page_timeout_secs` | `PAGE_TIMEOUT_SECS` | 90 | 5–3600 | live |
| `page_max_bytes` | `PAGE_MAX_BYTES` | 2097152 | 65536–67108864 | live |
| `name_timeout_secs` | `NAME_TIMEOUT_SECS` | 30 | 5–3600 | live |
| `eth_logs_max_blocks` | `ETH_LOGS_MAX_BLOCKS` | 10000 | 1–100000 | restart |
| `eth_request_secs` | `ETH_REQUEST_SECS` | 1 | 0–3600 | restart |
| `eth_rpc_url` | `ETH_RPC_URL` | none | secret: an `http(s)://` URL with a host (`ens::host_of`) | restart |

Each row also has a one-line `label` for the page, for example "Crawl workers: pages fetched at
once". The builder writes the labels.

## Review Focus
1. **The secret leaks through a side channel.** Error messages for a bad `eth_rpc_url`, the
   set-response body, and the `ignored:` text must not echo the value. Task 1 and Task 5 tests pin
   this.
2. **A cross-field change made in two steps.** Raising `pause_failures` above the current window
   is refused even when the next request would raise the window. The fix is to change the window
   first; the error says which bound applies. Task 1 tests pin this.
3. **Shrinking workers mid-crawl.** A worker that's no longer needed finishes its page and
   stops: no page is lost and none is fetched twice. Workers then grow back to the new target.
   Task 4 pins this.
4. **The restart response arrives before the exit.** darksearch must get its 200 before the
   process stops. Task 5 pins that the stop is scheduled after the response, not before.
5. **An admin-removed seed that's also in the file stays forgotten across the hourly re-read and
   restarts.** Task 6 pins this.

---

### Task 1: The settings registry, overrides, and env parsing

**Files:**
- Create: `src/settings.rs`
- Modify: `src/store.rs` (SCHEMA plus override functions), `src/main.rs` (replace
  `crawl_workers`, `health_min_rate`, `discovery_per_hour`, `eth_logs_max_blocks`,
  `eth_request_secs`, the `INDEX_MAX_AGE_DAYS`/`INDEX_CACHE_MB`/`GATEWAY_REQUESTS_PER_MIN` parsing,
  and their tests, with `Settings::from_env`), `src/index_queue.rs` (hold the settings)

**Interfaces:**
- Produces:
  - `pub enum Key { CrawlWorkers, GatewayRequestsPerMin, DiscoveryPerHour, HealthMinRate, HealthGraceSecs, IndexMaxAgeDays, IndexCacheMb, PauseWindow, PauseFailures, PauseMinSecs, PauseMaxSecs, PageAttempts, PageTimeoutSecs, PageMaxBytes, NameTimeoutSecs, EthLogsMaxBlocks, EthRequestSecs, EthRpcUrl }`.
    Also `Key::ALL: [Key; 18]` in table order, `Key::name(self) -> &'static str` and
    `Key::from_name(&str) -> Option<Key>`.
  - `pub enum Applies { Live, Restart }`.
  - `pub enum Source { Admin, Env, Default }`.
  - `pub struct Settings`, which is Send + Sync:
    - `pub fn from_env(env: impl Fn(&str) -> Option<String>, public_gateway: bool) -> Result<Settings, String>`.
      Err is the fatal message, for example `CRAWL_WORKERS must be a whole number from 1 to 500, got "0"`.
    - `pub fn defaults() -> Settings`: no env and own node. `IndexQueue::new()` uses it.
    - `pub fn get(&self, key: Key) -> u64` panics for `EthRpcUrl`.
    - `pub fn secret(&self, key: Key) -> Option<String>`, for `EthRpcUrl` only.
    - `pub fn load(&self, store: &Store)` reads the overrides. Invalid ones are logged and kept as
      ignored.
    - `pub fn set(&self, store: &Store, key: Key, raw: &str, actor: &str) -> Result<Applies, String>`
      validates, including the cross-field rules, writes, and logs.
    - `pub fn reset(&self, store: &Store, key: Key, actor: &str) -> Result<Applies, String>`.
    - `pub fn report(&self) -> serde_json::Value`: an array with one object per key, in table
      order, with the fields `name`, `label`, `env_var`, `value` (number, or `"set"`/`"not set"`
      for the secret), `source` (`"admin"|"iac"|"default"`), `iac` (number, `"set"`, or null),
      `default`, `min`, `max`, `applies` (`"live"|"restart"`), `actor`, `set_at` (unix secs or
      null), and `ignored` (string or null).
  - `Store`:
    - `settings_overrides() -> rusqlite::Result<Vec<(String, String, String, i64)>>` returns
      (name, value, actor, set_at).
    - `set_setting(name, value, actor, at) -> rusqlite::Result<()>`.
    - `clear_setting(name) -> rusqlite::Result<bool>`.
    - Table: `settings (name TEXT PRIMARY KEY, value TEXT NOT NULL, actor TEXT NOT NULL, set_at INTEGER NOT NULL)`.
  - `IndexQueue::settings(&self) -> &Settings` (a `OnceLock`, `get_or_init(Settings::defaults)`)
    and `IndexQueue::set_settings(&self, Settings)`.
  - `attach_store` calls `settings().load(store)`.

- [ ] **Step 1: Write the failing tests in `src/settings.rs`**
  - `defaults_match_the_table`: every `get` against the registry's Default column, with
    `gateway_requests_per_min` 0 for an own node and 100 for a public gateway.
  - `env_values_are_used_and_bad_ones_are_fatal`:
    - `CRAWL_WORKERS=60` gives 60.
    - `CRAWL_WORKERS=0` errors with `CRAWL_WORKERS must be a whole number from 1 to 500, got "0"`.
    - `INDEX_MAX_AGE_DAYS=x` errors (it used to fall back to 30).
    - `ETH_LOGS_MAX_BLOCKS=10` and `ETH_REQUEST_SECS=6` are accepted (iac's values).
  - `env_cross_field_rules_are_fatal`: `PAUSE_FAILURES=21` with the default window errors, and so
    does `PAUSE_MIN_SECS=1000` with the default max.
  - `an_override_beats_env_and_reset_falls_back`: with env `CRAWL_WORKERS=60`, `set` to 80 gives
    `get` 80 with source admin, actor `a@b` and a set_at. `reset` gives 60 with source iac.
    `load` on a fresh `Settings` over the same store sees the override.
  - `set_refuses_out_of_range_and_cross_field_values`:
    - `set(PauseFailures, "21")` errors, and the error mentions `pause_window`.
    - After `set(PauseWindow, "30")`, `set(PauseFailures, "21")` is Ok.
    - `set(CrawlWorkers, "abc")` errors and changes nothing in the store.
  - `the_secret_is_never_reported_or_echoed`:
    - `set(EthRpcUrl, "https://eth.example/v2/KEY123")` returns `Applies::Restart`.
    - `report().to_string()` doesn't contain `KEY123`.
    - `set(EthRpcUrl, "not a url KEY123")` errors, and the message doesn't contain `KEY123`.
  - `an_invalid_stored_override_is_ignored_not_fatal`: write `crawl_workers = "9999"` straight
    into the store. `load` keeps `get` at the env/default, and the report's `ignored` is set.
  - `restart_settings_say_so`: `set(EthLogsMaxBlocks, "50")` returns `Restart`, and
    `set(CrawlWorkers, "5")` returns `Live`.
  - Tests use `Store::open_in_memory()` and an `env` closure over a `HashMap`. They don't use the
    real environment.

- [ ] **Step 2: Run `cargo test settings` and confirm they fail to compile or fail.**

- [ ] **Step 3: Implement `settings.rs`, the store table and functions, and the `IndexQueue` field**
  - Overrides go in a `RwLock<HashMap<Key, Override>>`.
  - `set` validates the merged view: the new value against the current effective values of the
    other keys.
  - The time comes from `now_secs()`.

- [ ] **Step 4: Switch `main.rs` over**
  - Build `Settings::from_env(|k| std::env::var(k).ok(), args.len() < 2)` once, at the top. On
    Err, `error!` and exit 1.
  - Pass it to `index_queue.set_settings`.
  - Read `n_workers`, `min_rate`, `per_hour`, `max_age_secs`, `cache_mb` and the ENS limits from
    `index_queue.settings().get(..)`. The ETH URL comes from `secret(Key::EthRpcUrl)`.
  - Delete the old parse functions and move their tests' cases into the Step 1 tests.
  - Behaviour is unchanged: the later tasks make values live.

- [ ] **Step 5: Run `cargo test` and `cargo clippy`.** Expected: everything passes, with no new warnings.

- [ ] **Step 6: Commit.** Message: `Settings registry: admin override > env > default, stored in index.sqlite (#304)`.

### Task 2: Make the simple settings live

**Files:** `src/index_queue.rs` (throttle, expiry), `src/discovery.rs` (`HourlyCap`, `resolve`),
`src/main.rs` (health, expiry loop), `src/store.rs` (`set_cache_mb`)

**Interfaces:**
- Consumes `IndexQueue::settings()` and `Key` from Task 1.
- Produces:
  - `Throttle::wait(&self, per_min: u32)`: the interval comes from the argument on every call. The
    `OnceLock<Throttle>` becomes a plain `Throttle`, and `set_gateway_rate` goes.
  - `HourlyCap::new()` with no argument, and `HourlyCap::take(&self, now: i64, per_hour: u32) -> bool`.
  - `discovery::resolve(gateway: &str, name: &str, timeout: Duration)`.
  - `Store::set_cache_mb(&self, mb: u32) -> rusqlite::Result<()>` sets `cache_size` on the writer
    connection; 0 restores SQLite's default of -2000.
  - `health(..)` takes `grace: Duration` in place of the `HEALTH_GRACE` const, which goes.
  - `expire_and_reseed(queue)` reads `index_max_age_days` itself.
  - `Settings::set` and `reset` call a hook when `IndexCacheMb` changes. Do this as
    `IndexQueue::apply_setting(key)` called from the admin handler, and have `IndexQueue` own
    `settings_set(key, raw, actor)` and `settings_reset(key, actor)` wrappers. They call
    `Settings::set` and `reset` on `self.store()` and then `apply_setting`. Task 5 uses these.

- [ ] **Step 1: Write failing tests**
  - `throttle_follows_the_rate_it_is_given`: `wait(0)` doesn't block. `wait(600)` twice in a row
    takes about 100 ms or more.
  - `the_hourly_cap_uses_the_limit_it_is_given`: adapt the existing
    `the_hourly_cap_counts_per_hour_and_zero_means_none` to pass `per_hour` on each `take`.
    Lowering it mid-hour stops takes at once.
  - `health_uses_the_grace_it_is_given`: adapt the existing health tests to pass a grace.
  - `a_name_lookup_uses_the_timeout_it_is_given`: a gateway that never answers, with a 1 s
    timeout, errors within 3 s. The test may pass a timeout below the 5 s floor; the range only
    applies to settings.
  - `the_cache_size_is_reapplied`: after `set_cache_mb(64)`, `PRAGMA cache_size` on the writer
    reads `-65536`.

- [ ] **Step 2: Run them (`cargo test throttle hourly_cap health name_lookup cache_size`) and confirm they fail.**

- [ ] **Step 3: Implement**
  - `wait_for_gateway` passes `get(GatewayRequestsPerMin)`.
  - The resolver passes `get(DiscoveryPerHour)` to `take` and `get(NameTimeoutSecs)` to
    `resolve`.
  - `health_handler` reads `HealthMinRate` and `HealthGraceSecs`, so the `Health` struct keeps
    only `started`.
  - The daily refresh and startup expiry read `IndexMaxAgeDays`.
  - `attach_store` applies `set_cache_mb(get(IndexCacheMb))` after `load`, because an override
    may differ from the value used at open.

- [ ] **Step 4: Run `cargo test` and confirm everything passes.**

- [ ] **Step 5: Commit.** Message: `Gateway rate, discovery cap, health, max age, cache and name timeout read their setting on each use (#304)`.

### Task 3: Live pause rule and page fetch limits

**Files:** `src/breaker.rs`, `src/pauses.rs`, `src/index_queue.rs` (`fetch_html`, requeue,
`process_next`)

**Interfaces:**
- Consumes Task 1's `Settings` and `Key`.
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq)] pub struct PauseRule { pub window: usize, pub failures: usize, pub min: Duration, pub max: Duration }`
    in `breaker.rs`. `PauseRule::DEFAULT` is today's 20/16/60 s/15 min. `WINDOW`, `TRIP`,
    `MIN_PAUSE` and `MAX_PAUSE` go.
  - `Breaker::named(name: &str)`. `record(failure, rule: PauseRule)` and
    `tested(failure, rule: PauseRule)` use the rule passed in:
    - `recent` is trimmed to `rule.window`;
    - it opens when `failures` are reached;
    - the first pause is `rule.min`;
    - a failed test doubles the pause, capped at `rule.max`.
  - `Pauses::new()`, `record(root, failure, rule)` and `tested(root, failure, rule)`.
    `Pauses::with` goes, and tests pass a rule.
  - `IndexQueue::pause_rule(&self) -> PauseRule`, built from the settings.
  - `fetch_html(url: &str, timeout: Duration, max_bytes: u64)`. `MAX_BODY_BYTES` goes.
  - The requeue compares against `get(PageAttempts)`. `MAX_ATTEMPTS` goes.

- [ ] **Step 1: Write failing tests**
  - `a_smaller_window_trips_sooner`: rule window 4, failures 3. Three failures open the breaker.
  - `a_rule_change_applies_on_the_next_record`: record 15 failures under the default rule, then 1
    under a rule with failures 16 → open. Separately, record 2 failures, then record under window
    2 / failures 2 → open.
  - `the_pause_doubles_up_to_the_rules_max`: min 10 s, max 25 s. Successive failed tests give
    pauses of 10, 20, 25, 25 s.
  - `a_page_over_the_limit_is_skipped`: adapt `fetch_html_skips_oversized_body` to pass
    `max_bytes` 100 and a 101-byte body.
  - `a_page_is_dropped_after_the_configured_attempts`: with `page_attempts` set to 1, one
    gateway error drops the page; the existing 3-attempt test keeps passing at the default.
  - Adapt the existing breaker and pauses tests to pass `PauseRule::DEFAULT`.

- [ ] **Step 2: Run `cargo test breaker pauses fetch_html attempts` and confirm the new tests fail.**

- [ ] **Step 3: Implement.** `process_next` passes `self.pause_rule()` to every record and tested
  call, and `get(PageTimeoutSecs)` and `get(PageMaxBytes)` to `fetch_html`.

- [ ] **Step 4: Run `cargo test` and confirm everything passes.**

- [ ] **Step 5: Commit.** Message: `Pause rule, page attempts, timeout and size limit are settings (#304)`.

### Task 4: Resize the worker pool live

**Files:** Create `src/workers.rs`. Modify `src/main.rs`, where it replaces the `ThreadPool` loop.
Drop `threadpool` from `Cargo.toml` if nothing else uses it.

**Interfaces:**
- Consumes `IndexQueue::process_next(&str) -> bool` and `settings().get(Key::CrawlWorkers)`.
- Produces `pub fn supervise(queue: Arc<IndexQueue>, gateway: String, target: impl Fn() -> usize + Send + 'static, tick: Duration)`.
  It runs forever on its own thread (main spawns it with `target` reading the setting and a 1 s
  tick).
  - **Workers:** each has an id. Every tick, the supervisor starts a thread for each id in
    `0..target()` that isn't running.
  - **Shrinking:** a worker checks `id < target()` before taking each page, and exits when it
    isn't. It never exits mid-page.
  - **Bookkeeping:** running ids live in an `Arc<Mutex<BTreeSet<usize>>>`. An id is removed when
    its thread exits, including by panic: use a guard whose `Drop` removes it.
  - **Idle:** the existing 200 ms sleep when `process_next` returns false.
  - Also expose `pub fn running(set: &Mutex<BTreeSet<usize>>) -> usize` or similar for tests.
    The builder decides the shape.

- [ ] **Step 1: Write failing tests in `src/workers.rs`**
  - `workers_grow_and_shrink_to_the_target`: a fake queue setup where `process_next` sleeps 50 ms
    per page over many queued items, and a target in an `AtomicUsize`:
    - target 3 → 3 running within 1 s;
    - target 1 → 1 running within 1 s;
    - target 4 → 4 running.
    - Use a 20 ms tick.
  - `no_page_is_lost_when_shrinking`: queue 50 items and shrink from 5 to 1 while running. All
    50 are processed exactly once (the count of indexed items is 50 and the queue is empty).
  - `a_panicking_worker_is_replaced`. If faking the queue is impractical, test through
    `IndexQueue` with `index_for_test`-style items and a local `serve_once` gateway as the
    existing `index_queue` tests do.

- [ ] **Step 2: Run `cargo test workers` and confirm it fails.**

- [ ] **Step 3: Implement, and replace the pool in `main.rs`.** Log `Crawl workers: N` at start,
  and `Crawl workers: N → M` when the supervisor sees the target change.

- [ ] **Step 4: Run `cargo test` and confirm everything passes.**

- [ ] **Step 5: Commit.** Message: `Crawl workers grow and shrink to the setting without a restart (#304)`.

### Task 5: Admin settings endpoints and self-restart

**Files:** `src/admin.rs`, `src/main.rs` (server handle)

**Interfaces:**
- Consumes `IndexQueue::settings_set`, `settings_reset` and `settings().report()` (Tasks 1–2).
- Produces the routes, each behind `authorized` and registered in `configure`:
  - `GET /admin/settings` returns 200 with JSON `{"settings": <report>, "persistent": bool}`.
    `persistent` is false without `DATA_DIR`.
  - `POST /admin/settings/{name}`: the body is the raw value (text/plain), with `X-Admin-Actor`.
    It returns 200 `{"applies":"live"}` or `{"applies":"restart"}`.
    - An unknown name gets 404.
    - A bad value gets 400 with the reason as the body text, never echoing a secret.
    - Without `DATA_DIR` it gets 400 `settings need DATA_DIR`.
    - A missing actor gets 400 `X-Admin-Actor is required`.
  - `DELETE /admin/settings/{name}` resets, with the same codes.
  - `Admin` gains `restart: web::Data<Restart>`, a struct wrapping `OnceLock<actix_web::dev::ServerHandle>`.
    `main` builds the server, sets the handle, and awaits it.
  - On `applies: restart`, the handler returns its 200 first. It then spawns an actix task that
    sleeps 1 s and calls `handle.stop(true)`. `main` already flushes after the server returns,
    and returns `Ok(())`. Docker restarts the container.
  - Log `Restarting to apply <name>`.
  - `IndexQueue` needs to know whether a store is attached: add `has_persistent_store(&self) -> bool`.

- [ ] **Step 1: Write failing tests in `admin.rs`'s test module, using its `call` helper**
  - `settings_read_lists_every_key_with_sources`: 18 entries, `crawl_workers` with source
    `default`, and `persistent`.
  - `setting_a_value_needs_an_actor_and_a_valid_value`:
    - no `X-Admin-Actor` gets 400;
    - `abc` gets 400;
    - `25` gets 200 `live`, and the next GET shows 25, source `admin` and the actor.
  - `resetting_falls_back`: DELETE, then GET shows the default.
  - `an_unknown_setting_is_404`.
  - `the_secret_never_comes_back`: POST `eth_rpc_url` `https://x.example/v2/SECRETKEY`. The
    response body and the GET body don't contain `SECRETKEY`, and the GET shows `"set"`.
  - `a_restart_setting_answers_before_stopping`: with a `Restart` whose handle is unset (test
    mode), POST `eth_logs_max_blocks` `50` returns 200 `restart` without panicking. Unit-test
    the scheduling function separately: given a handle-like trait or closure, it runs only after
    the delay. The builder decides the shape, provided the response isn't delayed by the sleep.
  - `without_data_dir_writes_are_refused`: an in-memory store gets 400 `settings need DATA_DIR`.
  - `without_the_token_settings_routes_are_401` (and 404 without `ADMIN_TOKEN`, as the existing
    test checks).

- [ ] **Step 2: Run `cargo test admin` and confirm the new tests fail.**

- [ ] **Step 3: Implement.** Write logging through Task 1's `set`/`reset`. The handlers add none,
  so the secret has one code path.

- [ ] **Step 4: Run `cargo test` and confirm everything passes.**

- [ ] **Step 5: Commit.** Message: `Admin endpoints to read, set and reset settings; restart settings restart the indexer (#304)`.

### Task 6: Admin seed changes on top of iac's seeds file

**Files:** `src/store.rs`, `src/discovery.rs`, `src/admin.rs`, `src/main.rs`

**Interfaces:**
- Produces:
  - Table `seed_changes (entry TEXT PRIMARY KEY, change TEXT NOT NULL CHECK (change IN ('add','remove')), actor TEXT NOT NULL, set_at INTEGER NOT NULL)`.
  - `Store::seed_changes() -> rusqlite::Result<Vec<SeedChange>>`, `set_seed_change(&SeedChange)`
    and `clear_seed_change(entry) -> rusqlite::Result<bool>`.
  - In `discovery.rs`:
    - `pub struct SeedChange { pub entry: String, pub add: bool, pub actor: String, pub set_at: i64 }`.
    - `pub enum SeedMark { Iac, Added, Removed }`.
    - `pub fn effective_seeds(file: &[Entry], changes: &[SeedChange]) -> Vec<(String, SeedMark, Option<(String, i64)>)>`.
      It returns iac entries in file order, each `Iac` or `Removed`, then admin additions not in
      the file, in `set_at` order, as `Added`. An `add` change for an entry the file already has
      shows as `Iac`. A `Removed` entry still appears in the list, but it isn't seeded.
    - `run_seeds(queue, path: Option<&Path>)` waits on `IndexQueue::wake_seeds()` / a condvar,
      or for an hour. Each round it reads the file (none means empty), keeps the parsed file
      entries in `IndexQueue` for the view, then calls `add_seeds` and `forget_seeds` with the
      effective non-removed entries.
  - `main` always starts the seeds thread when a store is persistent, and also when only
    `SEEDS_FILE` is set (today's behaviour).
  - Routes:
    - `GET /admin/seeds` returns 200 with JSON `{"seeds":[{"entry","mark":"iac|added|removed","actor","set_at"}], "file": "<path>"|null}`.
    - `POST /admin/seeds/add` and `POST /admin/seeds/remove` take the entry as the body, plus
      `X-Admin-Actor`. The entry is validated with `entry::parse`; invalid gets 400. A blocked
      CID gets 400, as `discovery::check` refuses one.
    - `POST /admin/seeds/reset` clears that entry's change.
    - Each write calls `wake_seeds()` and returns 200 with a one-line message.

- [ ] **Step 1: Write failing tests**
  - `effective_seeds_layers_admin_changes_on_the_file`: file [a, b], changes add c and remove b
    → [(a, Iac), (b, Removed), (c, Added)].
  - `a_removed_seed_stays_forgotten_after_a_reread`: seed [a, b] and remove b. Run one round:
    b's `discovered` row is forgotten. Run another round with the same file: b is still absent.
    Run one round from a fresh `IndexQueue` over the same store (a restart): still absent.
  - `admin_seeds_work_without_a_file`: path None, add x, one round → x is discovered with source
    `seeds`.
  - `seed_routes_validate_and_wake`: an invalid entry gets 400; add, then GET shows `added` with
    the actor; reset, then GET no longer lists it.
  - Leave the existing seeds tests passing, adapted to `Option<&Path>`.

- [ ] **Step 2: Run `cargo test seed` and confirm the new tests fail.**

- [ ] **Step 3: Implement.** Make the round function (`seeds_round(queue, path)`) separate from
  the loop so the tests can call it.

- [ ] **Step 4: Run `cargo test` and confirm everything passes.**

- [ ] **Step 5: Commit.** Message: `Admin seed additions and removals on top of the seeds file (#304)`.

### Task 7: Show the local denylist and badbits in the admin

**Files:** `src/admin.rs`, `src/index_queue.rs`, `src/denylist.rs`

**Interfaces:**
- Produces:
  - `GET /admin/denylist` returns 200 with JSON
    `{"local": [{"line": "<as written>", "item": "<canonical>"|null}], "local_file": "<path>"|null, "badbits": {"entries": N, "loaded_at": <unix secs>}}`.
    `item` is `entry_item(line)`: non-null means darksearch may offer undo via the existing
    `/admin/undeny`. Blank lines are left out.
  - The read takes the existing `file_lock()`.
  - Without `LOCAL_DENYLIST`: `local` is `[]` and `local_file` is null.
  - `IndexQueue::badbits_summary(&self) -> (usize, i64)`. `set_denylist` records `now_secs()` as
    `loaded_at`.

- [ ] **Step 1: Write failing tests**
  - `the_denylist_read_lists_lines_and_marks_undoable_ones`: the file is
    `"# note\n<GOOD>\n//abcd\n<GOOD>/wiki/X\n"`, giving four entries. The items are null, GOOD,
    null and `GOOD/wiki/X`.
  - `the_denylist_read_needs_the_token`: 401.
  - `badbits_summary_counts_and_dates_the_list`.

- [ ] **Step 2: Run `cargo test denylist` and confirm the new tests fail.**

- [ ] **Step 3: Implement.**

- [ ] **Step 4: Run `cargo test` and confirm everything passes.**

- [ ] **Step 5: Commit.** Message: `Admin endpoint listing the local denylist and the badbits count (#304)`.

### Task 8: README, version, and the indexer PR

**Files:** `README.md`, `Cargo.toml` (`version = "0.6.0"`), `Cargo.lock`

- [ ] **Step 1: Update the README**
  - Add a "Runtime settings" section: the registry table (name, env var, default, range,
    live/restart), the precedence, that admin overrides survive restarts and deploys, and the
    restart behaviour.
  - Document the new endpoints and the `X-Admin-Actor` header next to the existing admin
    endpoint docs.
  - Document the new env vars where the existing ones are listed.
- [ ] **Step 2: Bump to 0.6.0.** Run `cargo build`, `cargo test` and `cargo clippy`: all pass,
  with no new warnings.
- [ ] **Step 3: Commit, then run the final review per the execution skill, then open the PR.**
  The PR body ends with the Claude Code line and says the darksearch page follows.

### Task 9: darksearch Settings page (separate repo and PR)

**Files (in `compscidr/darksearch.xyz`):**
- Create: `src/admin/settings.php`.
- Modify: `src/admin/admin.php`: add `X-Admin-Actor` to `adminIndexer`'s headers when an actor
  is given (a new optional parameter), and add a JSON helper. Also modify the admin nav
  (wherever `index.php`/`reports.php` links live).
- Add tests where the repo has PHP tests (run with `composer.phar`, since Docker can't mount the
  scratchpad).

**Interfaces:**
- Consumes the Task 5–7 endpoints exactly as specified.
- Produces one page, guarded by `adminActor()`, with three sections:
  - **Settings:**
    - **Table:** one row per setting, showing the label, the value, and the source as a badge
      (admin/iac/default). The iac and default values sit beside them, and an admin override also
      shows who set it and when.
    - **Editing:** a number input with min/max and Save, plus a Reset when the source is admin.
      A restart setting is marked "restarts the indexer".
    - **The secret:** shows "set" or "not set" with an empty password-type input that's never
      pre-filled, and Replace/Reset.
    - **Ignored overrides:** an `ignored` value shows as a warning on its row.
    - **No `DATA_DIR`:** when `persistent` is false, the inputs are disabled with the note
      "settings need DATA_DIR".
  - **Seeds:** the list with a mark per seed, plus add, remove (for iac or added seeds) and
    reset (for an admin change).
  - **Takedowns:** the local lines, with Undo (POST `admin/undeny` with `item`) only where
    `item` is non-null, and the badbits count and load date.
  - Writes are POST forms with the existing CSRF/guard pattern the other admin pages use. After a
    restart save, show "Saved; the indexer is restarting" and offer a reload link. Indexer
    unreachable uses the existing message.

- [ ] **Step 1: Write failing tests for the PHP helpers** (parsing the settings JSON into rows,
  and making sure the secret field is never pre-filled), following the existing test layout.
- [ ] **Step 2: Implement the page and the nav link.**
- [ ] **Step 3: Run the tests, then load the page locally against a local indexer** (`ADMIN_TOKEN`
  set, `DATA_DIR` set). Change `crawl_workers`, check the indexer log shows
  `Crawl workers: 10 → N`, then reset it.
- [ ] **Step 4: Commit and open the darksearch PR** after the indexer PR merges and deploys.
