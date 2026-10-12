# Persist the index (#106) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store indexed pages in SQLite under `DATA_DIR` and reload them at startup. Takedowns, denylists and expiry delete rows from disk, and nothing is served before the startup re-check.

**Architecture:**
- A new `src/store.rs` wraps one `rusqlite::Connection` behind a `Mutex`, with `secure_delete` on and the rollback journal kept in memory.
- `IndexQueue` gets an optional store (`OnceLock<Store>`): it upserts on insert, deletes on purge and expiry, and has an `attach_store` that filters by the denylists and age, then reloads `map` and the keyword maps.
- `main.rs` loads both denylists synchronously, attaches the store, and only then starts the workers and the HTTP server.

**Tech Stack:** Rust 2021, rusqlite 0.40 (`bundled`), actix-web 4, dashmap 6, existing `cargo test` suite.

**Spec:** `docs/superpowers/specs/2026-10-12-persist-index-design.md`

> **Superseded where it differs from the spec.** The branch review changed three things after this plan was written: `journal_mode = MEMORY` (an on-disk DELETE journal writes plaintext page copies and only unlinks them), a `writes` lock serializing `persist()` against denylist swaps and `expire()`, and `reseed()` after the daily expiry. The spec and code are authoritative; the snippets below are updated for the journal mode only.

## Global Constraints

- `DATA_DIR` unset means no store: behavior identical to today. `DATA_DIR` set but unopenable means log an error and `std::process::exit(1)`.
- File: `$DATA_DIR/index.sqlite`. Pragmas: `secure_delete = ON`, `journal_mode = MEMORY` (never an on-disk journal).
- Schema exactly as in the spec: `pages(item TEXT PRIMARY KEY, cid TEXT NOT NULL, title TEXT NOT NULL, excerpt TEXT NOT NULL, keywords TEXT NOT NULL, indexed_at INTEGER NOT NULL)`.
- Keywords encoding: one `word count` per line, `\n`-separated. Words never contain whitespace.
- Expiry: `INDEX_MAX_AGE_DAYS`, default 30. Checked at attach and in the existing 24 h refresh thread.
- Store only today's fields. No new fields on `IndexResult`, and the `/search` output format is unchanged (darksearch.xyz parses it).
- Keyword entries keep today's key semantics: they point at `IndexResult.cid`.
- `cargo fmt --all -- --check` and `cargo test` must pass (CI runs both).

## Review Focus

- **A purge while a worker is mid-insert:** the worker's `upsert` could re-create a row right after `purge` deleted it. Before upserting, the worker re-checks `is_blocked(&item)` and `is_blocked(&result.cid)` against the current denylists. Pinned in Task 2.
- **A store write failure while crawling** (disk full, `/data` remounted read-only): log it and keep crawling in memory; never panic a worker. Pinned in Task 2 with a store whose table was dropped (`break_for_test`), so every write fails.
- **A corrupt or non-SQLite `index.sqlite`:** `Store::open` returns an error, so the process exits instead of serving an empty or half-loaded index. Pinned in Task 1.
- **Keywords containing a space or newline** (impossible today, since words come from `split_whitespace`): decoding skips malformed lines rather than panicking. Pinned in Task 1.
- **A `DATA_DIR` that's a file, not a directory:** `open` errors and the process exits. Pinned in Task 1.

---

### Task 1: `Store`, the SQLite file

**Files:**
- Create: `src/store.rs`
- Modify: `Cargo.toml` (`[dependencies]`), `src/main.rs:13-15` (add `mod store;`)
- Test: `src/store.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub struct Store`
  - `pub fn now_secs() -> i64`
  - `impl Store`:
    - `pub fn open(dir: &Path) -> rusqlite::Result<Store>`
    - `pub fn path(&self) -> &Path`
    - `pub fn upsert(&self, item: &str, result: &IndexResult, indexed_at: i64) -> rusqlite::Result<()>`
    - `pub fn delete(&self, items: &[String]) -> rusqlite::Result<usize>`
    - `pub fn load_all(&self) -> rusqlite::Result<Vec<(String, IndexResult, i64)>>`
    - `pub fn items_older_than(&self, cutoff: i64) -> rusqlite::Result<Vec<String>>`
    - `#[cfg(test)] pub(crate) fn break_for_test(&self)`: drops the table so the next write fails (used by Task 2's failure test)

- [ ] **Step 1: Add the dependency and module**

`Cargo.toml` `[dependencies]`, append:
```toml
rusqlite = { version = "0.40.2", features = ["bundled"] }
```
`src/main.rs`, after `mod index_result;`:
```rust
mod store;
```

- [ ] **Step 2: Write the failing tests** (`src/store.rs`, the whole file for now)

```rust
use crate::index_result::IndexResult;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // a fresh, unique directory under the system temp dir
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("ipfs_indexer_{}_{}_{}", name, std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn page(cid: &str, title: &str, words: &[(&str, u32)]) -> IndexResult {
        let keywords = words.iter().map(|(w, c)| (w.to_string(), *c)).collect();
        IndexResult::new(cid.to_string(), title.to_string(), "an excerpt".to_string(), keywords)
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    #[test]
    fn round_trips_pages() {
        let dir = temp_dir("roundtrip");
        let store = Store::open(&dir).unwrap();
        store.upsert("bafyA/x", &page("bafyA/x", "Title A", &[("alpha", 2), ("beta", 1)]), 100).unwrap();
        store.upsert("bafyA/x", &page("bafyA/x", "Title A2", &[("alpha", 3)]), 200).unwrap();
        drop(store);

        let rows = Store::open(&dir).unwrap().load_all().unwrap();
        assert_eq!(rows.len(), 1, "upsert replaces");
        let (item, result, at) = &rows[0];
        assert_eq!((item.as_str(), result.title.as_str(), *at), ("bafyA/x", "Title A2", 200));
        assert_eq!(result.keywords.get("alpha"), Some(&3));
        assert_eq!(result.excerpt, "an excerpt");
    }

    #[test]
    fn delete_erases_the_text_from_the_file() {
        let dir = temp_dir("erase");
        let store = Store::open(&dir).unwrap();
        store.upsert("bafyB", &page("bafyB", "ZZQX-takedown-title-77", &[]), 1).unwrap();
        assert!(contains(&std::fs::read(store.path()).unwrap(), "ZZQX-takedown-title-77"));

        assert_eq!(store.delete(&["bafyB".to_string()]).unwrap(), 1);
        assert!(store.load_all().unwrap().is_empty());
        assert!(!contains(&std::fs::read(store.path()).unwrap(), "ZZQX-takedown-title-77"),
            "secure_delete must zero the deleted row in the file");
    }

    #[test]
    fn without_secure_delete_the_text_would_remain() {
        // control: proves the assertion above can fail
        let dir = temp_dir("control");
        let store = Store::open(&dir).unwrap();
        store.conn.lock().unwrap().pragma_update(None, "secure_delete", false).unwrap();
        store.upsert("bafyC", &page("bafyC", "ZZQX-control-title-88", &[]), 1).unwrap();
        store.delete(&["bafyC".to_string()]).unwrap();
        assert!(contains(&std::fs::read(store.path()).unwrap(), "ZZQX-control-title-88"));
    }

    #[test]
    fn finds_items_older_than_a_cutoff() {
        let store = Store::open(&temp_dir("age")).unwrap();
        store.upsert("old", &page("old", "o", &[]), 10).unwrap();
        store.upsert("new", &page("new", "n", &[]), 1000).unwrap();
        assert_eq!(store.items_older_than(500).unwrap(), vec!["old".to_string()]);
    }

    #[test]
    fn open_fails_on_a_file_or_garbage() {
        let dir = temp_dir("bad");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(Store::open(&file).is_err(), "DATA_DIR that is a file");

        std::fs::write(dir.join("index.sqlite"), b"this is not a sqlite database at all, 32+ bytes").unwrap();
        assert!(Store::open(&dir).is_err(), "corrupt index.sqlite");
    }

    #[test]
    fn decode_skips_malformed_keyword_lines() {
        let decoded = decode_keywords("good 3\nno-count\nbad x\n\nalso 1");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.get("good"), Some(&3));
        assert_eq!(decode_keywords(&encode_keywords(&decoded)), decoded);
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test store:: 2>&1 | tail -5`
Expected: compile errors (`cannot find type Store`, `cannot find function decode_keywords`).

- [ ] **Step 4: Implement** (insert above the `#[cfg(test)]` block in `src/store.rs`)

```rust
/**
 * The persisted index (#106): one SQLite file, `index.sqlite`, under DATA_DIR. secure_delete
 * zeroes deleted rows in the file and the rollback journal is kept in memory (never on disk),
 * so a takedown removes the text from disk, not just from memory.
 */
pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(dir: &Path) -> rusqlite::Result<Store> {
        let path = dir.join("index.sqlite");
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "secure_delete", true)?;
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pages (
                item       TEXT PRIMARY KEY,
                cid        TEXT NOT NULL,
                title      TEXT NOT NULL,
                excerpt    TEXT NOT NULL,
                keywords   TEXT NOT NULL,
                indexed_at INTEGER NOT NULL
            )",
        )?;
        Ok(Store { conn: Mutex::new(conn), path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn upsert(&self, item: &str, result: &IndexResult, indexed_at: i64) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO pages (item, cid, title, excerpt, keywords, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![item, result.cid, result.title, result.excerpt, encode_keywords(&result.keywords), indexed_at],
        )?;
        Ok(())
    }

    /// Deletes in one transaction; returns how many rows went
    pub fn delete(&self, items: &[String]) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut deleted = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM pages WHERE item = ?1")?;
            for item in items {
                deleted += stmt.execute([item])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    }

    pub fn load_all(&self) -> rusqlite::Result<Vec<(String, IndexResult, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item, cid, title, excerpt, keywords, indexed_at FROM pages")?;
        let rows = stmt.query_map([], |r| {
            let keywords: String = r.get(4)?;
            Ok((
                r.get(0)?,
                IndexResult::new(r.get(1)?, r.get(2)?, r.get(3)?, decode_keywords(&keywords)),
                r.get(5)?,
            ))
        })?;
        rows.collect()
    }

    pub fn items_older_than(&self, cutoff: i64) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item FROM pages WHERE indexed_at < ?1")?;
        let rows = stmt.query_map([cutoff], |r| r.get(0))?;
        rows.collect()
    }
}

#[cfg(test)]
impl Store {
    // makes every following read/write fail, to exercise callers' error handling
    pub(crate) fn break_for_test(&self) {
        self.conn.lock().unwrap().execute_batch("DROP TABLE pages").unwrap();
    }
}

// "word count" per line; words come from split_whitespace so they never contain the separator
fn encode_keywords(keywords: &HashMap<String, u32>) -> String {
    keywords.iter().map(|(w, c)| format!("{} {}\n", w, c)).collect()
}

fn decode_keywords(text: &str) -> HashMap<String, u32> {
    text.lines()
        .filter_map(|line| {
            let (word, count) = line.split_once(' ')?;
            Some((word.to_string(), count.parse().ok()?))
        })
        .collect()
}
```

Note: SQLite opens a non-database file lazily, so `Connection::open` succeeds on garbage. The first `pragma_update`/`execute_batch` then fails with "file is not a database", which is what `open_fails_on_a_file_or_garbage` relies on. If that test still passes `open`, add `conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;` after the pragmas and re-run.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test store:: 2>&1 | grep "test result"`
Expected: `test result: ok. 6 passed`

- [ ] **Step 6: Format, full suite, commit**

```bash
cargo fmt && cargo test 2>&1 | grep "test result"
git add Cargo.toml Cargo.lock src/store.rs src/main.rs
git commit -m "Add SQLite page store with secure_delete (#106)"
```

---

### Task 2: `IndexQueue` uses the store

**Files:**
- Modify: `src/index_queue.rs` (struct and `new()` at ~60-90, `purge` at ~117, `start` at ~201-222, the keyword block in `process_content` at ~370-393)
- Test: `src/index_queue.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Store`, `now_secs`, `store::tests::{temp_dir, page}` (Task 1).
- Produces:
  - `pub fn attach_store(&self, store: Store, max_age_secs: i64) -> rusqlite::Result<(usize, usize, usize)>`, returning `(loaded, blocked, expired)`
  - `pub fn expire(&self, max_age_secs: i64) -> usize`
  - `fn index_keywords(&self, key: &str, counts: &HashMap<String, u32>)`
  - `pub fn persist(&self, item: &str, result: &IndexResult)`: the single insert path a worker calls; it inserts into the map and upserts to the store unless the item or CID is now blocked
  - `fn blocked_quietly(&self, item: &str) -> bool`

- [ ] **Step 1: Write the failing tests** (append inside `mod tests`)

```rust
    use crate::store::tests::{page, temp_dir};
    use crate::store::{now_secs, Store};

    const DAY: i64 = 24 * 60 * 60;
    const GOOD: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
    const BAD: &str = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";

    fn file_has(store_dir: &std::path::Path, needle: &str) -> bool {
        let bytes = std::fs::read(store_dir.join("index.sqlite")).unwrap();
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    fn seed(dir: &std::path::Path, rows: &[(&str, &str, i64)]) {
        let store = Store::open(dir).unwrap();
        for (item, title, at) in rows {
            store.upsert(item, &page(item, title, &[("shared", 1), (title, 1)]), *at).unwrap();
        }
    }

    #[test]
    fn attach_reloads_pages_and_keywords() {
        let dir = temp_dir("attach");
        seed(&dir, &[(GOOD, "goodtitle", now_secs())]);
        let queue = IndexQueue::new();
        let (loaded, blocked, expired) = queue.attach_store(Store::open(&dir).unwrap(), 30 * DAY).unwrap();
        assert_eq!((loaded, blocked, expired), (1, 0, 0));
        assert_eq!(queue.search("goodtitle".to_string()).len(), 1);
        assert_eq!(queue.search("shared".to_string()).len(), 1);
    }

    #[test]
    fn attach_drops_denylisted_and_expired_rows_from_disk() {
        let dir = temp_dir("recheck");
        seed(&dir, &[
            (GOOD, "keeptitle", now_secs()),
            (&format!("{}/page", BAD), "blockedtitle", now_secs()),
            ("bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze/old", "expiredtitle", now_secs() - 40 * DAY),
        ]);
        let queue = IndexQueue::new();
        // local takedown list loaded before attach, as main.rs does
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        let (loaded, blocked, expired) = queue.attach_store(Store::open(&dir).unwrap(), 30 * DAY).unwrap();
        assert_eq!((loaded, blocked, expired), (1, 1, 1));
        assert_eq!(queue.index_length(), 1);
        assert!(!file_has(&dir, "blockedtitle"));
        assert!(!file_has(&dir, "expiredtitle"));
        assert!(file_has(&dir, "keeptitle"));
    }

    #[test]
    fn purge_deletes_from_disk() {
        let dir = temp_dir("purge");
        seed(&dir, &[(&format!("{}/page", BAD), "purgetitle", now_secs()), (GOOD, "stays", now_secs())]);
        let queue = IndexQueue::new();
        queue.attach_store(Store::open(&dir).unwrap(), 30 * DAY).unwrap();
        assert!(file_has(&dir, "purgetitle"));

        queue.set_denylist(Denylist::parse(&format!("{}\n", BAD)));
        assert_eq!(queue.index_length(), 1);
        assert!(!file_has(&dir, "purgetitle"), "takedown reaches the disk");
        assert!(file_has(&dir, "stays"));
    }

    #[test]
    fn expire_removes_old_pages_from_memory_and_disk() {
        let dir = temp_dir("expire");
        seed(&dir, &[(GOOD, "agingtitle", now_secs() - 10 * DAY)]);
        let queue = IndexQueue::new();
        queue.attach_store(Store::open(&dir).unwrap(), 30 * DAY).unwrap();
        assert_eq!(queue.expire(30 * DAY), 0);
        assert_eq!(queue.expire(5 * DAY), 1);
        assert_eq!(queue.index_length(), 0);
        assert!(!file_has(&dir, "agingtitle"));
    }

    #[test]
    fn crawled_pages_are_persisted_but_not_if_blocked_meanwhile() {
        let dir = temp_dir("crawl");
        let queue = IndexQueue::new();
        queue.attach_store(Store::open(&dir).unwrap(), 30 * DAY).unwrap();
        queue.persist(GOOD, &page(GOOD, "crawledtitle", &[]));
        assert!(file_has(&dir, "crawledtitle"));

        // a takedown that lands between fetch and insert must not be re-written by the worker
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.persist(BAD, &page(BAD, "racetitle", &[]));
        assert!(!file_has(&dir, "racetitle"));
    }

    #[test]
    fn store_write_failure_does_not_panic() {
        let dir = temp_dir("fail");
        let queue = IndexQueue::new();
        queue.attach_store(Store::open(&dir).unwrap(), 30 * DAY).unwrap();
        queue.store.get().unwrap().break_for_test(); // e.g. disk full / read-only remount
        queue.persist(GOOD, &page(GOOD, "t", &[])); // upsert fails: logs, doesn't panic
        assert_eq!(queue.index_length(), 1, "still indexed in memory");
        assert_eq!(queue.expire(0), 0, "expire tolerates the broken store too");
        queue.set_denylist(Denylist::parse(&format!("{}\n", GOOD))); // purge's delete fails: logs only
        assert_eq!(queue.index_length(), 0);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test index_queue::tests 2>&1 | grep -E "error\[|cannot find|test result" | head`
Expected: compile errors (`no method named attach_store`, `persist`, `expire`).

- [ ] **Step 3: Implement**

Add the imports at the top of `src/index_queue.rs`:
```rust
use crate::store::{now_secs, Store};
use std::sync::OnceLock;
```

Add a field to `IndexQueue` and initialize it in `new()`:
```rust
    // the persisted index (#106); unset when DATA_DIR isn't configured
    store: OnceLock<Store>,
```
```rust
            store: OnceLock::new(),
```

Replace `purge`:
```rust
    fn purge(&self, denylist: &Denylist) {
        let mut removed = Vec::new();
        self.map.retain(|item, result| {
            let keep = !denylist.is_blocked(item) && !denylist.is_blocked(&result.cid);
            if !keep {
                removed.push(item.clone());
            }
            keep
        });
        if let Some(store) = self.store.get() {
            if let Err(err) = store.delete(&removed) {
                warn!("Couldn't delete {} purged pages from the store: {}", removed.len(), err);
            }
        }
    }

    // is_blocked without the per-item warn!, for bulk checks
    fn blocked_quietly(&self, item: &str) -> bool {
        self.denylist.read().unwrap().is_blocked(item) || self.local_denylist.read().unwrap().is_blocked(item)
    }
```

Add these methods next to `set_local_denylist`:
```rust
    /**
     * Use `store` for this queue: drop rows that are denylisted or older than max_age_secs
     * (deleting them from disk), then load the rest into the map and keyword index. Call after
     * both denylists are loaded and before crawling or serving.
     */
    pub fn attach_store(&self, store: Store, max_age_secs: i64) -> rusqlite::Result<(usize, usize, usize)> {
        let cutoff = now_secs() - max_age_secs;
        let (mut drop, mut blocked, mut expired) = (Vec::new(), 0, 0);
        let mut keep = Vec::new();
        for (item, result, indexed_at) in store.load_all()? {
            if self.blocked_quietly(&item) || self.blocked_quietly(&result.cid) {
                blocked += 1;
                drop.push(item);
            } else if indexed_at < cutoff {
                expired += 1;
                drop.push(item);
            } else {
                keep.push((item, result));
            }
        }
        store.delete(&drop)?;
        for (item, result) in &keep {
            self.index_keywords(&result.cid, &result.keywords);
            self.map.insert(item.clone(), result.clone());
        }
        let _ = self.store.set(store);
        Ok((keep.len(), blocked, expired))
    }

    /// Drop pages indexed more than max_age_secs ago, from the store and the map
    pub fn expire(&self, max_age_secs: i64) -> usize {
        let Some(store) = self.store.get() else { return 0 };
        let old = match store.items_older_than(now_secs() - max_age_secs) {
            Ok(old) => old,
            Err(err) => {
                warn!("Couldn't check for expired pages: {}", err);
                return 0;
            }
        };
        if let Err(err) = store.delete(&old) {
            warn!("Couldn't delete expired pages: {}", err);
            return 0;
        }
        for item in &old {
            self.map.remove(item);
        }
        old.len()
    }

    /**
     * Add a crawled page to the index and the store. Re-checks the denylists first: a takedown
     * that landed while this page was being fetched must not be written back to disk.
     */
    pub fn persist(&self, item: &str, result: &IndexResult) {
        if self.blocked_quietly(item) || self.blocked_quietly(&result.cid) {
            return;
        }
        self.map.insert(item.to_string(), result.clone());
        if let Some(store) = self.store.get() {
            if let Err(err) = store.upsert(item, result, now_secs()) {
                warn!("Couldn't persist {}: {}", item, err);
            }
        }
    }

    // add a page's keyword counts to the search index (shared by crawling and attach_store)
    fn index_keywords(&self, key: &str, counts: &HashMap<String, u32>) {
        for word in counts.keys() {
            let keyword_map = self.keywords.entry(word.clone()).or_insert_with(DashMap::new);
            keyword_map.insert(key.to_string(), ());
            self.keyword_rank.insert(word.clone(), keyword_map.len() as u32);
        }
    }
```

In `start()`, replace `self.map.insert(item.clone(), result.unwrap());` with:
```rust
                        self.persist(&item, &result.unwrap());
```

In `process_content`, replace the per-word block (from `if self.keywords.contains_key(word.as_str()) {` through its closing `}` and the `else` branch) so the loop only counts. Then, right after the `for word in iter { … }` loop, add:
```rust
            self.index_keywords(&cid, &index_keywords);
```

The counting loop becomes:
```rust
            for word in iter {
                if word.len() > 3 {
                    let word = word.to_lowercase();
                    *index_keywords.entry(word).or_insert(0) += 1;
                }
            }
```

`blocked_quietly` replaces nothing; `is_blocked` (with `warn!`) stays for the enqueue and fetch paths.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test 2>&1 | grep -E "test result|FAILED|panicked"`
Expected: all pass (the 20 existing + 6 from Task 1 + 6 here).

- [ ] **Step 5: Format and commit**

```bash
cargo fmt && cargo fmt --all -- --check
git add src/index_queue.rs src/store.rs
git commit -m "Persist indexed pages; purges and expiry delete from disk (#106)"
```

---

### Task 3: Startup wiring in `main.rs`

**Files:**
- Modify: `src/main.rs` (the startup block ~105-150)

**Interfaces:**
- Consumes: `Store::open`, `IndexQueue::attach_store`, `IndexQueue::expire`, `Denylist::load_file`.

- [ ] **Step 1: Load the local denylist synchronously, attach the store, expire daily**

Replace the `LOCAL_DENYLIST` block with:
```rust
    // optional hand-maintained takedown list (plain CIDs / cid/path lines). Loaded once now, so
    // the store's startup check below sees it, then re-read every 5 min.
    let local_denylist = std::env::var("LOCAL_DENYLIST").ok();
    if let Some(path) = &local_denylist {
        match Denylist::load_file(path) {
            Ok(denylist) => index_queue.set_local_denylist(denylist),
            Err(err) => warn!("Couldn't read local denylist {}: {}", path, err),
        }
    }

    // the persisted index (#106): only nothing-blocked, unexpired pages are loaded, and nothing
    // is crawled or served until that's done. A configured but unusable DATA_DIR is fatal.
    let max_age_secs = std::env::var("INDEX_MAX_AGE_DAYS")
        .ok()
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(30)
        * 24 * 60 * 60;
    if let Ok(dir) = std::env::var("DATA_DIR") {
        let attached = store::Store::open(std::path::Path::new(&dir))
            .and_then(|store| index_queue.attach_store(store, max_age_secs));
        match attached {
            Ok((loaded, blocked, expired)) => info!(
                "Loaded {} pages from {}/index.sqlite (dropped {} blocked, {} expired)",
                loaded, dir, blocked, expired
            ),
            Err(err) => {
                error!("Couldn't use DATA_DIR {}: {}", dir, err);
                std::process::exit(1);
            }
        }
    }

    if let Some(path) = local_denylist {
        let local_queue = Arc::clone(&index_queue);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5 * 60));
            match Denylist::load_file(&path) {
                Ok(denylist) => local_queue.set_local_denylist(denylist),
                Err(err) => warn!("Couldn't read local denylist {}: {}", path, err),
            }
        });
    }
```

In the existing 24 h badbits refresh thread, after the `match Denylist::fetch() { … }`, add:
```rust
        let expired = refresh_queue.expire(max_age_secs);
        if expired > 0 {
            info!("Expired {} pages older than the max age", expired);
        }
```
(`max_age_secs` is an `i64`, which is `Copy`, so the closure can capture it. Move the badbits refresh-thread spawn below the `max_age_secs` definition if needed.)

- [ ] **Step 2: Build, test, format**

Run: `cargo build 2>&1 | grep -E "^error" -A5; cargo fmt && cargo test 2>&1 | grep "test result"`
Expected: no errors, all tests pass.

- [ ] **Step 3: Run it for real (local binary, temp DATA_DIR)**

```bash
D=$(mktemp -d); (DATA_DIR=$D RUST_LOG=info timeout 150 ./target/debug/ipfs_indexer > $D/run1.log 2>&1 &)
until grep -q "Loaded 0 pages" $D/run1.log; do sleep 2; done
until curl -s 127.0.0.1:9090/status | grep -q "Index size: [1-9]"; do sleep 5; done; sleep 2
pkill -x ipfs_indexer; sleep 1
sqlite3 $D/index.sqlite "select count(*) from pages" 2>/dev/null || python3 -c "import sqlite3,sys;print(sqlite3.connect('$D/index.sqlite').execute('select count(*) from pages').fetchone()[0])"
(DATA_DIR=$D RUST_LOG=info timeout 30 ./target/debug/ipfs_indexer 127.0.0.1:1 > $D/run2.log 2>&1 &)
until grep -qE "Loaded [0-9]+ pages" $D/run2.log; do sleep 2; done; grep -E "Loaded [0-9]+ pages" $D/run2.log
curl -s 127.0.0.1:9090/status
```
Expected: run 1 logs `Loaded 0 pages`; the row count is ≥ 1; run 2 logs `Loaded N pages …` with N equal to that count, and `/status` shows `Index size: N`, even though run 2 has no working gateway (`127.0.0.1:1`).

Also check: `DATA_DIR=/etc/passwd ./target/debug/ipfs_indexer` exits 1 with `Couldn't use DATA_DIR`.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "Attach the store at startup after both denylists; expire daily (#106)"
```

---

### Task 4: Docs, Docker check, PR

**Files:**
- Modify: `README.md`

- [ ] **Step 1: README "Persistence" section** (insert before `## Running with docker`)

```markdown
## Persistence
Set `DATA_DIR` to keep the index across restarts in `$DATA_DIR/index.sqlite` (SQLite). Without it the index lives in memory only.

- **Stored per page:** the CID/path, title, 128-character excerpt and keyword counts. Nothing else.
- **Startup:** both denylists (badbits and `LOCAL_DENYLIST`) are loaded first. Stored pages that are now denylisted, or older than `INDEX_MAX_AGE_DAYS` (default 30), are deleted. Only then are the remaining pages loaded, and crawling and serving start.
- **Takedowns:** whenever a denylist refresh removes pages, their rows are deleted from the file. `secure_delete` overwrites deleted rows with zeros, and the rollback journal is kept in memory, so no plaintext page copies go to a journal file.
- **Expiry:** pages older than the max age are dropped daily. The crawler re-adds them if it reaches them again.
- **No backups:** the index is deliberately not backed up. It can be rebuilt by crawling, and a backup is a copy takedowns can't reach.
- **Wipe:** stop the indexer, `rm $DATA_DIR/index.sqlite*`, and start it again.
- If `DATA_DIR` is set but can't be opened, the indexer exits rather than silently running without persistence.
```

- [ ] **Step 2: Docker check**

```bash
docker build -q -f docker/Dockerfile -t ipfs-indexer-persist . >/dev/null
V=$(mktemp -d); chmod 777 $V
docker run -d --rm --name persist -e DATA_DIR=/var/lib/ipfs_indexer -v $V:/var/lib/ipfs_indexer -p 127.0.0.1:19095:9090 ipfs-indexer-persist
until curl -s 127.0.0.1:19095/status | grep -q "Index size: [1-9]"; do sleep 5; done
docker restart persist; sleep 2; until docker logs persist 2>&1 | grep -qE "Loaded [1-9][0-9]* pages"; do sleep 2; done
docker logs persist 2>&1 | grep -E "Loaded [0-9]+ pages"; curl -s 127.0.0.1:19095/status
docker stop persist; docker rmi ipfs-indexer-persist; docker run --rm -v $V:/d alpine:3.24 rm -rf /d/index.sqlite
```
Expected: after the restart, the log reports the pages loaded, and `/status` shows them before any new crawl.

- [ ] **Step 3: Commit, push, PR**

```bash
git add README.md
git commit -m "Document persistence, retention, wipe and no-backup policy (#106)"
git push -u origin feat/persist-index
gh pr create --base main --title "Persist the index on disk (#106)" --body "<summary of the 7 criteria and how each is met; test list; Fixes #106>"
```

The iac role already sets `DATA_DIR` (iac #804). Deploying is `cube.yml --tags ipfs-indexer` once this merges.
