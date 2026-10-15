# Pages Live Only on Disk Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete `IndexQueue`'s in-memory page map and mirror map, so memory no longer grows with the number of indexed pages. SQLite becomes the only copy.

**Architecture:**
- Every former use of the maps becomes one of: an indexed SQLite lookup on dedicated read connections, a counter the `Store` maintains, a small `roots` table kept up to date in the write transactions, or a background denylist purge that scans `pages`.
- Search checks the results it returns against both denylists, so a takedown is hidden at once.
- Startup no longer loads pages or the queue.

**Tech Stack:** Rust 2021, rusqlite 0.40 (bundled SQLite), actix-web 4, dashmap. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-14-page-map-on-disk-design.md`

## Global Constraints

- CI runs `cargo fmt --all -- --check && cargo test`. Both must pass after every task.
- Three unused-import warnings in `src/index_queue.rs` (`MessageBody`, `Map`, `Not`) predate this work. Leave them alone.
- No new crates.
- Every write transaction stays bounded. The journal lives in memory (`journal_mode=MEMORY`), so a transaction's size is RAM. Write in batches of `WRITE_BATCH` (1000) items, and never put a whole-table write in one transaction. One exception: the one-time `roots` fill, which writes only a few hundred rows.
- Don't add migrations that rewrite existing tables. The `roots` fill is the only one-time step, and it only inserts into a new table.
- `secure_delete` stays on, and no plaintext goes to temp files: `temp_store=MEMORY` stays.
- Comments match the surrounding density and voice: short, lowercase-start line comments, `/** */` doc blocks on public functions. Issue numbers in parentheses (#231).
- Version: bump `Cargo.toml` to `0.3.0` in the last task. The deny reply changes, and startup and the log lines change.

## Review Focus

1. **A takedown arrives while a purge scan is running.** The new entry must still be erased. Expected: the swap sets "purge due" again, so the next round scans again. Pinned in Task 4: `a_denylist_change_during_a_purge_runs_another`.
2. **A page is re-crawled (new id) while expiry is deleting old pages.** Expiry must not delete the fresh version. Expected: each batch is re-selected and deleted under the writes lock. Pinned in Task 3: `expiry_keeps_a_page_recrawled_since_it_was_listed`.
3. **The admin undenies an entry before the purge runs.** The page must not be deleted. Expected: the scan checks the current lists, not a snapshot. Pinned in Task 4: `undeny_before_the_purge_keeps_the_page`.
4. **Crash recovery (`recover_unflushed`) deletes pages and re-queues them.** `roots` must stay equal to a `GROUP BY` after reopening. Pinned in Task 1: `roots_match_a_group_by_after_crash_recovery`.
5. **In-memory mode (no `DATA_DIR`).** The lookup and scan "connections" are the write connection. A purge scan must not deadlock against its own deletes. Expected: each batch is read, the lock released, then deleted. Pinned in Task 4: `purge_works_without_a_data_dir`.

---

## File map

| File | Changes |
|---|---|
| `src/store.rs` | `roots` table, its fill and upkeep; `pages_indexed_at` index; `mirrors` counter; `lookup` and `scan` connections; `known`, `pages_by_id`, `items_after`, `mirror_items_after`, `root_counts`, `older_than`; `load_all` / `mirrors_load` / `queue_load` become `#[cfg(test)]`; `items`, `rekey`, `queue_roots`, `items_older_than` are removed |
| `src/index_queue.rs` | `map` and `mirrors` removed; `admit`, `search`, `persist`, `recrawl`, `expire`, `attach_store`, `open_store`, `root_counts`, `index_length`, `mirror_length` rewritten; purge job (`purge_due`, `request_purge`, `run_purges`, `purge_blocked`); `set_denylist` / `set_local_denylist` swap and queue |
| `src/denylist.rs` | `#[derive(Clone)]` and `adds_to` |
| `src/admin.rs` | deny reply text; tests use `index_for_test` |
| `src/main.rs` | purge thread; startup expiry thread; startup log line |
| `README.md`, `Cargo.toml` | docs, v0.3.0 |

---

### Task 1: `roots` table, `indexed_at` index, mirror counter

**Files:**
- Modify: `src/store.rs`: `SCHEMA`; `Store` struct, `init`, `upsert`, `remove_page`, `delete_batch`, `queue_add`, `queue_remove`, `queue_pop`, `recover_unflushed`; new `root_of`, `bump_root`, `fill_roots`, `root_counts`, `mirror_count`; tests.
- Modify: `src/index_queue.rs`: `root_counts` uses the store; the test `root_counts_group_items_by_their_root_cid`.

**Interfaces:**
- Produces: `Store::root_counts(&self) -> rusqlite::Result<Vec<(String, usize, usize, String)>>` (root, pages, queued, the root page's title or `""`), sorted by pages desc, then queued desc, then root asc. `Store::mirror_count(&self) -> usize`. `fn root_of(item: &str) -> &str` (module-private).

- [ ] **Step 1: Write the failing tests** (in `store.rs`'s `mod tests`)

```rust
    // roots as a GROUP BY over pages and queue would count them
    fn roots_by_scan(store: &Store) -> Vec<(String, usize, usize)> {
        let conn = store.conn.lock().unwrap();
        let mut counts: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        for (table, col) in [("pages", 0), ("queue", 1)] {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT substr(item, 1, instr(item || '/', '/') - 1), count(*) FROM {} GROUP BY 1",
                    table
                ))
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))
                .unwrap();
            for row in rows {
                let (root, n) = row.unwrap();
                let entry = counts.entry(root).or_default();
                if col == 0 { entry.0 = n } else { entry.1 = n }
            }
        }
        counts.into_iter().map(|(r, (p, q))| (r, p, q)).collect()
    }

    fn roots_table(store: &Store) -> Vec<(String, usize, usize)> {
        let mut rows: Vec<_> = store
            .root_counts()
            .unwrap()
            .into_iter()
            .map(|(root, pages, queued, _)| (root, pages, queued))
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn roots_track_pages_mirrors_deletes_and_the_queue() {
        let dir = temp_dir("roots");
        let store = Store::open(&dir, 0).unwrap();
        store.upsert("a", &page("a", "A root", &[("x", 1)]), 1).unwrap();
        store.upsert("a/1", &page("a/1", "t", &[("x", 1)]), 1).unwrap();
        store.upsert("a/1", &page("a/1", "t", &[("y", 1)]), 2).unwrap(); // re-crawl: new id
        store.upsert("b/1", &hashed("b/1", &[("z", 1)], b"same"), 1).unwrap();
        store.upsert("c/1", &hashed("c/1", &[("z", 1)], b"same"), 1).unwrap(); // mirror of b/1
        store.queue_add(&["a/2".into(), "a/3".into(), "d/1".into(), "a/2".into()]).unwrap();
        store.queue_pop().unwrap(); // a/2
        store.queue_remove(&["d/1".into(), "nope".into()]).unwrap();
        store.delete(&["b/1".to_string()], true).unwrap(); // takes its mirror c/1 too
        assert_eq!(roots_table(&store), roots_by_scan(&store));
        assert_eq!(
            store.root_counts().unwrap()[0],
            ("a".to_string(), 2, 1, "A root".to_string())
        );
        assert_eq!(store.mirror_count(), 0);
        drop(store);
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(roots_table(&store), roots_by_scan(&store));
    }

    #[test]
    fn roots_match_a_group_by_after_crash_recovery() {
        let dir = temp_dir("roots-recover");
        let store = Store::open(&dir, 0).unwrap();
        store.upsert("a/1", &page("a/1", "t", &[("x", 1)]), 1).unwrap();
        store.flush().unwrap();
        store.upsert("a/2", &page("a/2", "t", &[("x", 1)]), 1).unwrap(); // never flushed
        store.upsert("b/1", &hashed("b/1", &[("z", 1)], b"h"), 1).unwrap();
        store.upsert("c/1", &hashed("c/1", &[("z", 1)], b"h"), 1).unwrap(); // mirror of b/1
        std::mem::forget(store); // a crash: no flush on drop
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(roots_table(&store), roots_by_scan(&store));
        assert_eq!(store.queue_len(), 3, "a/2, b/1 and its mirror c/1 queued again");
    }

    #[test]
    fn roots_are_filled_once_from_an_index_without_them() {
        let dir = temp_dir("roots-fill");
        let store = Store::open(&dir, 0).unwrap();
        store.upsert("a/1", &page("a/1", "t", &[("x", 1)]), 1).unwrap();
        store.upsert("a", &page("a", "Root", &[("x", 1)]), 1).unwrap();
        store.queue_add(&["a/2".into(), "b/1".into()]).unwrap();
        store.conn.lock().unwrap().execute_batch("DROP TABLE roots").unwrap(); // a v0.2.9 index
        drop(store);
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(
            roots_table(&store),
            vec![("a".to_string(), 2, 1), ("b".to_string(), 0, 1)]
        );
    }

    #[test]
    fn expiry_query_uses_the_indexed_at_index() {
        let store = Store::open_in_memory().unwrap();
        let plan: String = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "EXPLAIN QUERY PLAN SELECT item FROM pages WHERE indexed_at < 5",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("pages_indexed_at"), "{}", plan);
    }
```

`hashed` already exists in this test module (`fn hashed(item, words, hash) -> IndexResult`). If it's defined later in the file, move these tests below it or reference it as is: Rust resolves functions in a module regardless of order.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test roots_ expiry_query 2>&1 | tail -20`
Expected: compile errors: `no method named root_counts`, `mirror_count`.

- [ ] **Step 3: Implement**

In `SCHEMA`, add before the closing `";`:

```sql
    -- indexed pages and queued items per root CID, kept in step with them for /roots (#231)
    CREATE TABLE IF NOT EXISTS roots (
        root   TEXT PRIMARY KEY,
        pages  INTEGER NOT NULL,
        queued INTEGER NOT NULL
    );
```

Add a field to `Store`, after `pages`:

```rust
    // rows in `mirrors`, for /status (#253)
    mirrors: AtomicUsize,
```

Add module-level helpers below `refuse_old_format`:

```rust
// a "cid/path" item's root CID
fn root_of(item: &str) -> &str {
    item.split('/').next().unwrap_or(item)
}

// adds to a root's page and queued counts, dropping the row once both are 0
fn bump_root(tx: &Connection, item: &str, pages: i64, queued: i64) -> rusqlite::Result<()> {
    if pages == 0 && queued == 0 {
        return Ok(());
    }
    let root = root_of(item);
    tx.prepare_cached(
        "INSERT INTO roots (root, pages, queued) VALUES (?1, ?2, ?3)
         ON CONFLICT (root) DO UPDATE SET pages = pages + ?2, queued = queued + ?3",
    )?
    .execute(params![root, pages, queued])?;
    tx.prepare_cached("DELETE FROM roots WHERE root = ?1 AND pages <= 0 AND queued <= 0")?
        .execute([root])?;
    Ok(())
}

/**
 * Fills `roots` from `pages` and `queue`, once, for an index from before it (v0.2.9). Two
 * GROUP BYs in one transaction: it inserts one row per root (a few hundred), so the in-memory
 * journal and the sorter stay small however many pages there are.
 */
fn fill_roots(conn: &mut Connection) -> rusqlite::Result<()> {
    log::info!("Counting pages and queued items per root (one-time, #231)");
    let tx = conn.transaction()?;
    tx.execute_batch(
        "INSERT INTO roots (root, pages, queued)
           SELECT substr(item, 1, instr(item || '/', '/') - 1) AS r, count(*), 0
           FROM pages GROUP BY r;
         INSERT INTO roots (root, pages, queued)
           SELECT substr(item, 1, instr(item || '/', '/') - 1) AS r, 0, count(*)
           FROM queue GROUP BY r
           ON CONFLICT (root) DO UPDATE SET queued = excluded.queued;",
    )?;
    tx.commit()
}
```

The `GROUP BY` ends the `SELECT` unambiguously, so the upsert parses without SQLite's `WHERE true` workaround. This was checked with sqlite3 3.46.

In `init`, replace `conn.execute_batch(SCHEMA)?;` with:

```rust
        let has_roots: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'roots'")?
            .exists([])?;
        let has_pages: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'pages'")?
            .exists([])?;
        conn.execute_batch(SCHEMA)?;
        if has_pages && !has_roots {
            fill_roots(&mut conn)?;
        }
```

Add to the `CREATE INDEX IF NOT EXISTS` batch:

```sql
             CREATE INDEX IF NOT EXISTS pages_indexed_at ON pages (indexed_at);
```

After the existing `queued` count in `init`:

```rust
        let mirrors: i64 = conn.query_row("SELECT count(*) FROM mirrors", [], |r| r.get(0))?;
```

and in the `Store { … }` literal: `mirrors: AtomicUsize::new(mirrors as usize),`.

Add:

```rust
    pub fn mirror_count(&self) -> usize {
        self.mirrors.load(Ordering::Relaxed)
    }
```

**Upkeep in `remove_page`**, after the `dead_pages` insert: `bump_root(tx, item, -1, 0)?;`

**`upsert`:**
- In the mirror branch, replace the `INSERT INTO mirrors … ON CONFLICT` with:

  ```rust
                let replaced = tx
                    .prepare_cached("DELETE FROM mirrors WHERE item = ?1")?
                    .execute([item])?;
                tx.prepare_cached("INSERT INTO mirrors (item, page) VALUES (?1, ?2)")?
                    .execute(params![item, page])?;
  ```

  After `tx.commit()?; drop(conn);` add `self.mirrors.fetch_add(1 - replaced, Ordering::Relaxed);`.
- On the indexed path, change the leading `DELETE FROM mirrors WHERE item = ?1` to `let unmirrored = tx.prepare_cached(…)?.execute([item])?;`.
- After the `INSERT INTO pages … RETURNING id`, add `bump_root(&tx, item, 1, 0)?;`.
- After commit, add `self.mirrors.fetch_sub(unmirrored, Ordering::Relaxed);`.

**`delete_batch`:** count `mirror.execute([item])?` and `mirrors_of.execute([id])?` into a `let mut unmirrored = 0;` (`unmirrored += …`). After the commit, `self.mirrors.fetch_sub(unmirrored, Ordering::Relaxed);`.

**`queue_add`:** in the per-item loop:

```rust
                for item in batch {
                    let n = stmt.execute([item])?;
                    bump_root(&tx, item, 0, n as i64)?;
                    batch_added += n;
                }
```

`stmt` borrows `tx`, and `bump_root` takes `&Connection`. `&tx` derefs to `&Connection`, and both are shared borrows, so this compiles.

**`queue_remove`:** the same: `let n = stmt.execute([item])?; bump_root(&tx, item, 0, -(n as i64))?; removed += n;`.

**`queue_pop`:** take a transaction (`let mut conn = …; let tx = conn.transaction()?;`). Run the `DELETE … RETURNING item` on `tx`. If `Some(item)`, call `bump_root(&tx, &item, 0, -1)?`. Then `tx.commit()?`. Keep the counter and the timing log as they are.

**`recover_unflushed`:** replace the body after the `items.is_empty()` early return and the warn log:

```rust
    // their mirrors are crawled again too: the page they point at is going
    let mirrored: Vec<String> = tx
        .prepare("SELECT item FROM mirrors WHERE page > ?1")?
        .query_map([flushed_through as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    tx.execute("DELETE FROM mirrors WHERE page > ?1", [flushed_through as i64])?;
    tx.execute("DELETE FROM pages WHERE id > ?1", [flushed_through as i64])?;
    {
        let mut queue = tx.prepare("INSERT OR IGNORE INTO queue (item) VALUES (?1)")?;
        for item in &items {
            bump_root(&tx, item, -1, 0)?;
        }
        for item in items.iter().chain(&mirrored) {
            let n = queue.execute([item])?;
            bump_root(&tx, item, 0, n as i64)?;
        }
    }
    tx.commit()
```

`DELETE FROM mirrors WHERE page > ?1` is equivalent to the old `IN (SELECT id FROM pages WHERE id > ?1)`, because mirror pages are page ids.

**`Store::root_counts`** (add near `queue_len`):

```rust
    /// Indexed and queued counts per root CID with the root page's title ("" if it isn't
    /// indexed), most indexed first (#232); from `roots`, no scan (#231)
    pub fn root_counts(&self) -> rusqlite::Result<Vec<(String, usize, usize, String)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare_cached(
            "SELECT roots.root, roots.pages, roots.queued, coalesce(pages.title, '')
             FROM roots LEFT JOIN pages ON pages.item = roots.root
             ORDER BY roots.pages DESC, roots.queued DESC, roots.root",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? as usize,
                r.get::<_, i64>(2)? as usize,
                r.get(3)?,
            ))
        })?;
        rows.collect()
    }
```

Delete `Store::queue_roots`.

In `index_queue.rs`, replace the body of `IndexQueue::root_counts`:

```rust
    /// Indexed and queued counts per root CID (the first segment of "cid/path"), with the root
    /// page's title if it's indexed, most indexed first (#232)
    pub fn root_counts(&self) -> Vec<(String, usize, usize, String)> {
        self.store().root_counts().unwrap_or_else(|err| {
            warn!("Couldn't count pages by root: {}", err);
            Vec::new()
        })
    }
```

Update the test `root_counts_group_items_by_their_root_cid`: use `queue.index_for_test(item, &page(item, item, &[]))` in place of `queue.map.insert(...)`. The expected vector is unchanged.

- [ ] **Step 4: Run all the tests**

Run: `cargo fmt --all && cargo test 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: `test result: ok.` and no FAILED.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs src/index_queue.rs
git commit -m "Store: roots table kept in step with pages and the queue, indexed_at index, mirror count (#231)"
```

---

### Task 2: lookup and scan connections and the queries that replace the maps

**Files:**
- Modify: `src/store.rs`: `Store` struct (`lookup`, `scan`), `open`, `init`; new `known`, `pages_by_id`, `items_after`, `mirror_items_after`, `older_than`; remove `items`, `rekey`, `items_older_than`; move `load_all`, `mirrors_load`, `queue_load` into the `#[cfg(test)] impl Store` block; tests.

**Interfaces:**
- Produces:
  - `pub fn known(&self, item: &str) -> rusqlite::Result<bool>`: true if `pages` or `mirrors` has the item.
  - `pub fn pages_by_id(&self, ids: &[u64]) -> rusqlite::Result<HashMap<u64, (String, IndexResult)>>`: item and result (no keywords) for ids still in `pages`.
  - `pub fn items_after(&self, after: u64, n: usize) -> rusqlite::Result<Vec<(u64, String, String)>>`: (id, item, cid) with id > after, ascending, at most n.
  - `pub fn mirror_items_after(&self, after: i64, n: usize) -> rusqlite::Result<Vec<(i64, String)>>`: (rowid, item) of `mirrors`, ascending.
  - `pub fn older_than(&self, cutoff: i64, n: usize) -> rusqlite::Result<Vec<String>>`: items with `indexed_at < cutoff`, at most n, on the write connection (the caller holds the writes lock).

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn known_finds_pages_and_mirrors_only() {
        let dir = temp_dir("known");
        let store = Store::open(&dir, 0).unwrap();
        store.upsert("a/1", &hashed("a/1", &[("x", 1)], b"h"), 1).unwrap();
        store.upsert("b/1", &hashed("b/1", &[("x", 1)], b"h"), 1).unwrap(); // mirror
        store.queue_add(&["c/1".into()]).unwrap();
        assert!(store.known("a/1").unwrap());
        assert!(store.known("b/1").unwrap());
        assert!(!store.known("c/1").unwrap(), "queued isn't known");
        assert!(!store.known("zzz").unwrap());
    }

    #[test]
    fn pages_by_id_returns_rows_still_there() {
        let store = Store::open_in_memory().unwrap();
        store.upsert("a/1", &page("a/1", "Title A", &[("x", 1)]), 1).unwrap();
        store.upsert("a/2", &page("a/2", "Title B", &[("x", 1)]), 1).unwrap();
        let ids: Vec<u64> = store.items_after(0, 10).unwrap().iter().map(|r| r.0).collect();
        store.delete(&["a/2".to_string()], false).unwrap();
        let found = store.pages_by_id(&ids).unwrap();
        assert_eq!(found.len(), 1);
        let (item, result) = &found[&ids[0]];
        assert_eq!((item.as_str(), result.title.as_str()), ("a/1", "Title A"));
        assert!(result.keywords.is_empty());
    }

    #[test]
    fn items_after_pages_through_in_id_order() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            let item = format!("a/{}", i);
            store.upsert(&item, &page(&item, "t", &[("x", 1)]), 1).unwrap();
        }
        let first = store.items_after(0, 2).unwrap();
        let rest = store.items_after(first[1].0, 10).unwrap();
        let items: Vec<String> = first.iter().chain(&rest).map(|r| r.1.clone()).collect();
        assert_eq!(items, vec!["a/0", "a/1", "a/2", "a/3", "a/4"]);
    }

    #[test]
    fn older_than_lists_at_most_n() {
        let store = Store::open_in_memory().unwrap();
        for (item, at) in [("a/1", 1), ("a/2", 2), ("a/3", 9)] {
            store.upsert(item, &page(item, "t", &[("x", 1)]), at).unwrap();
        }
        assert_eq!(store.older_than(5, 1).unwrap().len(), 1);
        let mut all = store.older_than(5, 10).unwrap();
        all.sort();
        assert_eq!(all, vec!["a/1", "a/2"]);
    }
```

Update existing tests that call removed functions:
- `store.items(&ids)` (around the test at `src/store.rs` ~line 878): use `store.pages_by_id(&ids).unwrap()` and map to the item (`.into_iter().map(|(id, (item, _))| (id, item)).collect::<HashMap<_, _>>()`).
- `store.items(&[old]).unwrap().is_empty()` (~1480): `store.pages_by_id(&[old]).unwrap().is_empty()`.
- `store.items_older_than(500)` (~1049): `store.older_than(500, usize::MAX)`.
- In `index_queue.rs` tests, `.items_older_than(now_secs())` (~2124): `.older_than(now_secs(), usize::MAX)`.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test known_ pages_by_id items_after older_than 2>&1 | tail -5`
Expected: compile errors for the missing methods.

- [ ] **Step 3: Implement**

Add fields to `Store` after `reader`:

```rust
    // "already indexed?" checks for every link the crawl finds (#231), on their own connection so
    // they don't wait behind a search on `reader`
    lookup: Option<Mutex<Connection>>,
    // long reads (the denylist purge's scan, /admin/recrawl), so they don't hold up either
    scan: Option<Mutex<Connection>>,
```

In `init`'s `Store { … }` literal: `lookup: None, scan: None,`.

In `open`, replace the reader block with a helper that opens one read-only connection, used three times:

```rust
        // default cache_size, not cache_mb: SQLite drops a connection's cache whenever another
        // one commits, which the crawl does constantly, so a big one here would rarely be warm
        let read_only = || -> rusqlite::Result<Mutex<Connection>> {
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            conn.busy_timeout(BUSY_TIMEOUT)?;
            Ok(Mutex::new(conn))
        };
        store.reader = Some(read_only()?);
        store.lookup = Some(read_only()?);
        store.scan = Some(read_only()?);
```

Add next to `read_conn`:

```rust
    fn lookup_conn(&self) -> MutexGuard<'_, Connection> {
        self.lookup.as_ref().unwrap_or(&self.conn).lock().unwrap()
    }

    fn scan_conn(&self) -> MutexGuard<'_, Connection> {
        self.scan.as_ref().unwrap_or(&self.conn).lock().unwrap()
    }
```

Add the queries:

```rust
    /// Whether an item is indexed or recorded as a mirror (#231: replaces the in-memory maps)
    pub fn known(&self, item: &str) -> rusqlite::Result<bool> {
        let conn = self.lookup_conn();
        let found = conn
            .prepare_cached(
                "SELECT 1 FROM pages WHERE item = ?1 UNION ALL SELECT 1 FROM mirrors WHERE item = ?1",
            )?
            .exists([item])?;
        Ok(found)
    }

    /// The item and page (without keyword counts) of page ids; ids no longer in `pages` are
    /// left out
    pub fn pages_by_id(&self, ids: &[u64]) -> rusqlite::Result<HashMap<u64, (String, IndexResult)>> {
        let conn = self.read_conn();
        let mut stmt =
            conn.prepare_cached("SELECT item, cid, title, excerpt FROM pages WHERE id = ?1")?;
        let mut found = HashMap::new();
        for &id in ids {
            let row = stmt
                .query_row([id as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        IndexResult::new(r.get(1)?, r.get(2)?, r.get(3)?, HashMap::new()),
                    ))
                })
                .optional()?;
            if let Some(row) = row {
                found.insert(id, row);
            }
        }
        Ok(found)
    }

    /// Up to n pages after id `after`, as (id, item, cid), in id order: for walking every page
    /// in batches without holding a connection for the whole walk
    pub fn items_after(&self, after: u64, n: usize) -> rusqlite::Result<Vec<(u64, String, String)>> {
        let conn = self.scan_conn();
        let mut stmt = conn
            .prepare_cached("SELECT id, item, cid FROM pages WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = stmt.query_map(params![after as i64, n as i64], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// Up to n mirrors after rowid `after`, as (rowid, item), in rowid order
    pub fn mirror_items_after(&self, after: i64, n: usize) -> rusqlite::Result<Vec<(i64, String)>> {
        let conn = self.scan_conn();
        let mut stmt = conn.prepare_cached(
            "SELECT rowid, item FROM mirrors WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after, n as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Up to n items indexed before `cutoff` (by the indexed_at index). On the write
    /// connection: expiry lists and deletes a batch under the writes lock, so a page re-crawled
    /// meanwhile isn't listed with its old date.
    pub fn older_than(&self, cutoff: i64, n: usize) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT item FROM pages WHERE indexed_at < ?1 LIMIT ?2")?;
        let rows = stmt.query_map(params![cutoff, n.min(i64::MAX as usize) as i64], |r| r.get(0))?;
        rows.collect()
    }
```

Remove `Store::items`, `Store::rekey` and `Store::items_older_than`. Move `load_all`, `mirrors_load` and `queue_load` unchanged into the existing `#[cfg(test)] impl Store { … }` block, with `pub(crate)` visibility.

`IndexQueue` still calls `items`, `rekey`, `load_all`, `mirrors_load`, `queue_load` and `items_older_than` at this point. Switch those callers now, minimally, so the crate builds:
- `search`: `store().items(&shown)` → `store().pages_by_id(&shown)`. The filter then uses the item from the tuple: `.filter_map(|page| items.get(page)).filter_map(|(item, _)| self.map.get(item).map(|r| r.clone()))`.
- `expire`: `store.items_older_than(now_secs() - max_age_secs)` → `store.older_than(now_secs() - max_age_secs, usize::MAX)`.
- `attach_store` keeps calling `load_all`, `mirrors_load`, `queue_load` and `rekey` until Task 3. To keep it building, have this task leave `rekey` in place, and keep `load_all`, `mirrors_load` and `queue_load` non-test (`pub`), each with a `// #231: removed with the maps (Task 3)` comment. Task 3 moves them under `#[cfg(test)]` and deletes `rekey`.

- [ ] **Step 4: Run all the tests**

Run: `cargo fmt --all && cargo test 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: `test result: ok.`

- [ ] **Step 5: Commit**

```bash
git add src/store.rs src/index_queue.rs
git commit -m "Store: lookup and scan connections; known, pages_by_id, items_after, older_than (#231)"
```

---

### Task 3: remove the maps from `IndexQueue`

**Files:**
- Modify: `src/index_queue.rs`: `IndexQueue` struct and `new`; `index_for_test`; `open_store`; `attach_store`; `recrawl`; `expire`; `persist`; `admit`; `index_length`; `mirror_length`; `search`; `purge` (temporary store-backed version, replaced in Task 4); `without_keywords` removed; tests.
- Modify: `src/store.rs`: delete `rekey`; move `load_all`, `mirrors_load` and `queue_load` under `#[cfg(test)]`.
- Modify: `src/admin.rs` tests: `queue.map.insert(...)` → `queue.index_for_test(...)`.
- Modify: `src/main.rs`: the startup log line for the new `attach_store` return value.

**Interfaces:**
- Consumes: `Store::known`, `pages_by_id`, `items_after`, `mirror_items_after`, `older_than`, `mirror_count`, `root_counts` (Tasks 1–2).
- Produces:
  - `IndexQueue::attach_store(&self, store: Store, max_age_secs: i64) -> rusqlite::Result<(usize, usize)>`: (pages, queued). `max_age_secs` is kept for main's signature and unused here; expiry runs from main in Task 4. Mark it `_max_age_secs`.
  - `IndexQueue::open_store(...) -> Result<(usize, usize), OpenError>`.
  - `IndexQueue::purge_blocked(&self) -> usize`: the scan. Task 4 calls it from the purge thread.

- [ ] **Step 1: Write the failing tests** (in `index_queue.rs`'s `mod tests`)

```rust
    #[test]
    fn search_hides_a_blocked_page_before_any_purge() {
        let queue = IndexQueue::new();
        queue.index_for_test(GOOD, &page(GOOD, "t", &[("alpha", 1)]));
        // swapped in without purging: what a takedown looks like before the scan reaches it
        *queue.local_denylist.write().unwrap() = Denylist::parse(&format!("{}\n", GOOD));
        assert!(queue.search("alpha", 0, 10).1.is_empty());
        assert_eq!(queue.index_length(), 1, "still on disk until the purge");
    }

    #[test]
    fn admit_skips_indexed_pages_and_mirrors_from_the_store() {
        let queue = IndexQueue::new();
        queue.persist(GOOD, &hashed_page(GOOD, b"h"));
        let mirror = format!("{}/copy", GOOD);
        queue.persist(&mirror, &hashed_page(&mirror, b"h"));
        assert_eq!(queue.mirror_length(), 1);
        assert_eq!(queue.admit(GOOD.to_string()).1, None);
        assert_eq!(queue.admit(mirror).1, None);
        assert!(queue.admit(format!("{}/new", GOOD)).1.is_some());
    }

    #[test]
    fn attach_store_doesnt_load_pages_and_keeps_the_queue() {
        let dir = temp_dir("attach-lazy");
        let store = Store::open(&dir, 0).unwrap();
        store.upsert(GOOD, &page(GOOD, "t", &[("alpha", 1)]), now_secs()).unwrap();
        store.queue_add(&[format!("{}/next", GOOD)]).unwrap();
        drop(store);
        let queue = IndexQueue::new();
        assert_eq!(
            queue.attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY).unwrap(),
            (1, 1)
        );
        assert_eq!(queue.search("alpha", 0, 10).1.len(), 1);
    }

    #[test]
    fn expiry_keeps_a_page_recrawled_since_it_was_listed() {
        let queue = IndexQueue::new();
        queue.store().upsert(GOOD, &page(GOOD, "old", &[("alpha", 1)]), 1).unwrap();
        // re-crawled: a fresh date under the same item
        queue.store().upsert(GOOD, &page(GOOD, "new", &[("alpha", 1)]), now_secs()).unwrap();
        assert_eq!(queue.expire(DAY), 0);
        assert_eq!(queue.search("alpha", 0, 10).1[0].title, "new");
    }

    #[test]
    fn recrawl_streams_more_pages_than_a_batch() {
        let queue = IndexQueue::new();
        for i in 0..(crate::store::WRITE_BATCH + 5) {
            let item = format!("{}/p{}", GOOD, i);
            queue.index_for_test(&item, &page(&item, "t", &[]));
        }
        assert_eq!(queue.recrawl(), crate::store::WRITE_BATCH + 5);
        assert_eq!(queue.queue_length(), crate::store::WRITE_BATCH + 5);
    }
```

Add the test helper next to `seed`:

```rust
    fn hashed_page(item: &str, hash: &[u8]) -> IndexResult {
        let mut result = page(item, "t", &[("w", 1)]);
        result.text_hash = Some(hash.to_vec());
        result
    }
```

Make `WRITE_BATCH` `pub(crate) const` in `store.rs`.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test search_hides admit_skips attach_store_doesnt expiry_keeps recrawl_streams 2>&1 | tail -20`
Expected: failures. For example, `attach_store` returns a 3-tuple, and search serves the blocked page.

- [ ] **Step 3: Implement**

1. **The struct and `new`:** remove the `map` and `mirrors` fields and their initializers. Delete `fn without_keywords`.

2. **`index_for_test`:** keep only `self.store().upsert(item, result, now_secs()).unwrap();`.

3. **`open_store`:** return type `Result<(usize, usize), OpenError>`. Delete the `self.map.clear(); self.mirrors.clear();` lines and the comment above them.

4. **`attach_store`:** replace it entirely:

```rust
    /**
     * Use `store` for this queue (call before crawling or serving). Nothing is loaded into memory
     * (#231): pages, mirrors and the queue stay on disk. Pages a denylist blocked while the
     * index was down are hidden by search's check and erased by the purge queued here; expiry
     * runs from main. Returns the pages and queued items there are.
     */
    pub fn attach_store(
        &self,
        store: Store,
        _max_age_secs: i64,
    ) -> rusqlite::Result<(usize, usize)> {
        store.maintain();
        let counts = (store.page_count(), store.queue_len());
        if self.store.set(store).is_err() {
            panic!("attach_store must run before the store is first used");
        }
        self.request_purge();
        Ok(counts)
    }
```

   Add `pub fn page_count(&self) -> usize { self.pages.load(Ordering::Relaxed) }` to `Store`.

   Add a minimal `request_purge` for now (Task 4 replaces it):

```rust
    // Task 4 runs this on the purge thread; until then the purge is synchronous
    fn request_purge(&self) {
        self.purge_blocked();
    }
```

5. **`purge_blocked`:** replace `fn purge(&self, denylist: &Denylist) -> usize` with a scan of the store against the current lists:

```rust
    /**
     * Deletes and erases (#249) every stored page, mirror and superseded version a denylist
     * blocks now, scanning `pages` in batches on the store's scan connection (#231). No lock is
     * held across batches: pages written meanwhile were checked against the lists already.
     * Returns how many pages went.
     */
    pub fn purge_blocked(&self) -> usize {
        let Some(store) = self.store.get() else {
            return 0;
        };
        let blocked = |item: &str| self.blocked_quietly(item);
        let mut removed = 0;
        let mut after = 0;
        loop {
            let batch = match store.items_after(after, 10_000) {
                Ok(batch) => batch,
                Err(err) => {
                    warn!("Couldn't scan pages for denylisted ones: {}", err);
                    break;
                }
            };
            let Some(last) = batch.last() else { break };
            after = last.0;
            let hits: Vec<String> = batch
                .into_iter()
                .filter(|(_, item, cid)| blocked(item) || blocked(cid))
                .map(|(_, item, _)| item)
                .collect();
            match store.delete(&hits, true) {
                Ok(n) => removed += n,
                Err(err) => warn!("Couldn't delete {} denylisted pages: {}", hits.len(), err),
            }
        }
        let mut after = 0;
        loop {
            let batch = match store.mirror_items_after(after, 10_000) {
                Ok(batch) => batch,
                Err(err) => {
                    warn!("Couldn't scan mirrors for denylisted ones: {}", err);
                    break;
                }
            };
            let Some(last) = batch.last() else { break };
            after = last.0;
            let hits: Vec<String> =
                batch.into_iter().map(|(_, item)| item).filter(|item| blocked(item)).collect();
            if let Err(err) = store.delete(&hits, true) {
                warn!("Couldn't delete {} denylisted mirrors: {}", hits.len(), err);
            }
        }
        // superseded versions whose own item or cid is now blocked, e.g. one that redirected
        // to a cid since taken down (#249)
        match store.dead_versions() {
            Ok(versions) => {
                let ids: Vec<u64> = versions
                    .into_iter()
                    .filter(|(_, item, cid, _)| blocked(item) || blocked(cid))
                    .map(|(id, ..)| id)
                    .collect();
                if let Err(err) = store.erase_ids(&ids) {
                    warn!("Couldn't mark {} blocked versions to erase: {}", ids.len(), err);
                }
            }
            Err(err) => warn!("Couldn't check old page versions: {}", err),
        }
        if removed > 0 {
            info!("Purged {} denylisted pages", removed);
        }
        removed
    }
```

   `set_denylist` and `set_local_denylist` swap first, then purge (synchronous until Task 4):

```rust
    pub fn set_denylist(&self, denylist: Denylist) {
        {
            let _writes = self.writes.write().unwrap();
            *self.denylist.write().unwrap() = denylist;
        }
        self.purge_blocked();
    }

    /// Same as set_denylist, for the local takedown list; returns how many pages it purged
    pub fn set_local_denylist(&self, denylist: Denylist) -> usize {
        {
            let _writes = self.writes.write().unwrap();
            *self.local_denylist.write().unwrap() = denylist;
        }
        self.purge_blocked()
    }
```

6. **`recrawl`:**

```rust
    /**
     * Queue every indexed page to be fetched again, so links to pages not indexed yet are followed
     * again (the crawl frontier was lost before it was saved, #225). Pages stay searchable while
     * they wait, and what's left of it survives a restart. Streams `pages` in batches (#231).
     */
    pub fn recrawl(&self) -> usize {
        let store = self.store();
        let (mut queued, mut after) = (0, 0);
        loop {
            let batch = match store.items_after(after, crate::store::WRITE_BATCH) {
                Ok(batch) => batch,
                Err(err) => {
                    warn!("Couldn't list pages to recrawl: {}", err);
                    break;
                }
            };
            let Some(last) = batch.last() else { break };
            after = last.0;
            let items = batch
                .into_iter()
                .map(|(_, item, _)| item)
                .filter(|item| !self.blocked_quietly(item))
                .collect();
            queued += self.push_queued(items);
        }
        queued
    }
```

   `push_queued` returns the number newly added, so items already queued aren't counted. That matches the existing test `recrawl_queues_every_indexed_page_once`.

7. **`expire`:** each batch is listed and deleted under the writes lock, so a page re-crawled in between isn't deleted:

```rust
    /// Drop pages indexed more than max_age_secs ago, a batch at a time under the writes lock,
    /// so a page re-crawled meanwhile isn't listed with its old date
    pub fn expire(&self, max_age_secs: i64) -> usize {
        let Some(store) = self.store.get() else {
            return 0;
        };
        let cutoff = now_secs() - max_age_secs;
        let mut expired = 0;
        loop {
            let _writes = self.writes.write().unwrap();
            let old = match store.older_than(cutoff, crate::store::WRITE_BATCH) {
                Ok(old) => old,
                Err(err) => {
                    warn!("Couldn't check for expired pages: {}", err);
                    break;
                }
            };
            if old.is_empty() {
                break;
            }
            match store.delete(&old, true) {
                Ok(_) => expired += old.len(),
                Err(err) => {
                    // what's left is still old enough for the next expiry to retry
                    warn!("Couldn't delete every expired page; the next expiry retries: {}", err);
                    break;
                }
            }
        }
        // old versions of re-crawled pages don't outlive the retention window either
        match store.dead_versions() {
            Ok(versions) => {
                let old_versions: Vec<u64> = versions
                    .into_iter()
                    .filter(|(.., died_at)| *died_at < cutoff)
                    .map(|(id, ..)| id)
                    .collect();
                if let Err(err) = store.erase_ids(&old_versions) {
                    warn!("Couldn't mark expired versions to erase: {}", err);
                }
            }
            Err(err) => warn!("Couldn't check old page versions: {}", err),
        }
        expired
    }
```

   The test `expiry_drops_pages_from_memory_even_if_the_store_fails_part_way` (v0.2.9) asserted on the map. Now that there is no memory copy, rewrite it to assert that `expire` returns 0 without panicking and that the page is still listed by `older_than` for the next run:

```rust
    #[test]
    fn expiry_that_fails_leaves_pages_for_the_next_run() {
        let dir = temp_dir("expire-fails");
        seed(&dir, &[(GOOD, "agingtitle", now_secs() - 10 * DAY)]);
        let queue = IndexQueue::new();
        queue.attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY).unwrap();
        // finding the old pages works, deleting them doesn't
        queue.store.get().unwrap().drop_table_for_test("mirrors");
        assert_eq!(queue.expire(5 * DAY), 0);
        assert_eq!(
            queue.store.get().unwrap().older_than(now_secs(), 10).unwrap(),
            vec![GOOD.to_string()],
            "still on disk, for the next expiry"
        );
    }
```

8. **`persist`:**

```rust
    pub fn persist(&self, item: &str, result: &IndexResult) -> bool {
        let _writes = self.writes.read().unwrap();
        if self.blocked_quietly(item) || self.blocked_quietly(&result.cid) {
            return false;
        }
        match self.store().upsert(item, result, now_secs()) {
            Ok(Upserted::Mirror(_)) => false,
            Ok(Upserted::Indexed) => true,
            Err(err) => {
                warn!("Couldn't persist {}: {}", item, err);
                // its links are still followed
                true
            }
        }
    }
```

9. **`admit`:** replace the `map`/`mirrors` check:

```rust
        match self.store().known(&item) {
            Ok(true) => {
                trace!("Already indexed {}", item);
                return (Enqueued::Queued, None);
            }
            Ok(false) => {}
            // admitted: a wasted fetch at worst, where dropping it would lose the link
            Err(err) => warn!("Couldn't check whether {} is indexed: {}", item, err),
        }
```

10. **Counts:** `index_length` → `self.store().page_count()`. `mirror_length` → `self.store().mirror_count()`.

11. **`search`:** `let pages = self.store().page_count() as f64;`. Replace the tail after `shown`:

```rust
        let found = match self.store().pages_by_id(&shown) {
            Ok(found) => found,
            Err(err) => {
                warn!("Couldn't look up search results: {}", err);
                return (0, Vec::new());
            }
        };
        // a page deleted since its postings were read isn't found; one a denylist blocks isn't
        // served even before the purge erases it (#231)
        let results = shown
            .iter()
            .filter_map(|page| found.get(page))
            .filter(|(item, result)| !self.blocked_quietly(item) && !self.blocked_quietly(&result.cid))
            .map(|(_, result)| result.clone());
        (total, results.collect())
```

12. **`store.rs`:** delete `rekey`. Move `load_all`, `mirrors_load` and `queue_load` into the `#[cfg(test)] impl Store` block as `pub(crate)`.

13. **`main.rs`:** in the `match attached` arm, use the new 2-tuple:

```rust
            Ok((pages, queued)) => info!(
                "Opened {}/index.sqlite: {} pages, {} queued",
                dir, pages, queued
            ),
```

14. **Tests.** Replace every `queue.map.insert(item, result)` with `queue.index_for_test(&item, &result)` in `index_queue.rs` and `admin.rs`.
    - Delete `queue.map.get(...)` assertions about empty keywords (around ~1760 and ~1781). The in-memory copy they checked no longer exists, so remove the two `assert!(… .keywords.is_empty() …)` lines and keep the search assertions.
    - Delete the test that asserted `attach_store` re-spells pre-#218 items (the one collecting `queue.map.iter()` keys, ~line 1680). The behaviour is removed by the spec.
    - Tests that call `attach_store(...).unwrap()` and destructure a 3-tuple: change them to the 2-tuple, or drop the blocked/expired assertions. Expiry and blocking now happen through `expire()` and the purge. Where a test asserted that blocked/expired rows were dropped at attach, call `queue.purge_blocked()` / `queue.expire(max_age)` after `attach_store` and assert the same end state.

- [ ] **Step 4: Run all the tests**

Run: `cargo fmt --all && cargo test 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: `test result: ok.` If a test fails because it relied on startup dropping blocked or expired pages, apply step 14's last bullet. Don't weaken what it asserts about the end state.

- [ ] **Step 5: Commit**

```bash
git add src/index_queue.rs src/store.rs src/admin.rs src/main.rs
git commit -m "IndexQueue: no in-memory page or mirror map; SQLite is the only copy (#231)"
```

---

### Task 4: background purge, purging only on list changes, startup expiry

**Files:**
- Modify: `src/denylist.rs`: `#[derive(Clone)]` on `Denylist`, `adds_to`, a test.
- Modify: `src/index_queue.rs`: `purge_due: Mutex<bool>` and `purge_wake: Condvar` fields; `request_purge`, `run_purges`, `purge_for_test`; `set_denylist` / `set_local_denylist` (diff-based); tests.
- Modify: `src/admin.rs`: the deny reply; tests.
- Modify: `src/main.rs`: purge thread, startup expiry thread.

**Interfaces:**
- Consumes: `IndexQueue::purge_blocked` (Task 3).
- Produces: `Denylist::adds_to(&self, old: &Denylist) -> bool`. `IndexQueue::run_purges(&self)` (blocks until a purge is due, then runs one). `IndexQueue::set_local_denylist(&self, denylist: Denylist)` with no return value.

- [ ] **Step 1: Write the failing tests**

In `denylist.rs` tests:

```rust
    #[test]
    fn adds_to_is_true_only_for_new_entries() {
        let old = Denylist::parse(&format!("{}\n", V1));
        assert!(!old.adds_to(&old));
        assert!(!Denylist::empty().adds_to(&old), "removals add nothing");
        let more = Denylist::parse(&format!("{}\n{}/x\n", V1, V1));
        assert!(more.adds_to(&old));
    }
```

In `index_queue.rs` tests:

```rust
    #[test]
    fn an_unchanged_local_list_queues_no_purge() {
        let queue = IndexQueue::new();
        queue.attach_store(Store::open_in_memory().unwrap(), 30 * DAY).unwrap();
        queue.purge_for_test(); // the one attach_store queued
        let list = format!("{}\n", BAD);
        queue.set_local_denylist(Denylist::parse(&list));
        assert!(*queue.purge_due.lock().unwrap());
        queue.purge_for_test();
        queue.set_local_denylist(Denylist::parse(&list));
        assert!(!*queue.purge_due.lock().unwrap(), "same list: nothing to scan");
    }

    #[test]
    fn a_takedown_is_hidden_at_once_and_erased_by_the_purge() {
        let dir = temp_dir("purge-erases");
        let queue = IndexQueue::new();
        queue.attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY).unwrap();
        queue.purge_for_test();
        queue.persist(BAD, &page(BAD, "t", &[("zzqxpurgeterm", 1)]));
        queue.flush_for_test();
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        assert!(queue.search("zzqxpurgeterm", 0, 10).1.is_empty(), "hidden at once");
        assert!(dir_has(&dir, "zzqxpurgeterm"), "not erased until the purge runs");
        queue.purge_for_test();
        queue.maintain_for_test();
        assert!(!dir_has(&dir, "zzqxpurgeterm"));
        assert_eq!(queue.index_length(), 0);
    }

    #[test]
    fn undeny_before_the_purge_keeps_the_page() {
        let queue = IndexQueue::new();
        queue.attach_store(Store::open_in_memory().unwrap(), 30 * DAY).unwrap();
        queue.purge_for_test();
        queue.persist(GOOD, &page(GOOD, "t", &[("alpha", 1)]));
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", GOOD)));
        queue.set_local_denylist(Denylist::empty()); // undone before the scan
        queue.purge_for_test();
        assert_eq!(queue.index_length(), 1);
        assert_eq!(queue.search("alpha", 0, 10).1.len(), 1);
    }

    #[test]
    fn a_denylist_change_during_a_purge_runs_another() {
        let queue = IndexQueue::new();
        queue.attach_store(Store::open_in_memory().unwrap(), 30 * DAY).unwrap();
        // the purge thread has taken the due flag and is scanning...
        assert!(queue.take_purge_due());
        // ...when a takedown lands
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        assert!(*queue.purge_due.lock().unwrap(), "the next round scans again");
    }

    #[test]
    fn purge_works_without_a_data_dir() {
        let queue = IndexQueue::new(); // in-memory store on first use
        queue.persist(BAD, &page(BAD, "t", &[("alpha", 1)]));
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.purge_for_test();
        assert_eq!(queue.index_length(), 0);
    }
```

In `admin.rs`, update `deny_appends_purges_and_survives_the_reread`:
- seed with `queue.index_for_test(&item, &IndexResult::new(...))`;
- expect `(200, "Blocked; removal from disk queued")`;
- then call `queue.purge_for_test()` before `assert_eq!(queue.index_length(), 0)`.

- [ ] **Step 2: Run them and confirm they fail**

Run: `cargo test adds_to unchanged_local takedown_is_hidden undeny_before denylist_change purge_works deny_appends 2>&1 | tail -20`
Expected: compile errors (`adds_to`, `purge_due`, `purge_for_test`, `take_purge_due`).

- [ ] **Step 3: Implement**

In `denylist.rs`: put `#[derive(Clone)]` on `pub struct Denylist`, and add:

```rust
    /// Whether this list has an entry `old` doesn't: only then can it block a stored page
    /// that wasn't blocked before
    pub fn adds_to(&self, old: &Denylist) -> bool {
        self.hashes.iter().any(|hash| !old.hashes.contains(hash))
    }
```

In `IndexQueue`: add the fields `purge_due: Mutex<bool>` and `purge_wake: std::sync::Condvar` (initialized to `Mutex::new(false)` and `Condvar::new()`). Import `Condvar` alongside `Mutex`. Replace Task 3's `request_purge`:

```rust
    // marks a purge as due and wakes the purge thread; one round covers every change before it
    fn request_purge(&self) {
        *self.purge_due.lock().unwrap() = true;
        self.purge_wake.notify_one();
    }

    // takes the due flag: a change after this sets it again, so it gets a round of its own
    fn take_purge_due(&self) -> bool {
        std::mem::replace(&mut *self.purge_due.lock().unwrap(), false)
    }

    /// Waits until a purge is due, then runs one (#231). For a thread of its own.
    pub fn run_purges(&self) {
        let mut due = self.purge_due.lock().unwrap();
        while !*due {
            due = self.purge_wake.wait(due).unwrap();
        }
        *due = false;
        drop(due);
        self.purge_blocked();
    }

    #[cfg(test)]
    pub(crate) fn purge_for_test(&self) {
        if self.take_purge_due() {
            self.purge_blocked();
        }
    }
```

`set_denylist` / `set_local_denylist`: swap under the writes lock and queue a purge only if the new list adds entries and a store is attached. A store-less queue has nothing on disk yet, and `attach_store` queues a full purge.

```rust
    /// Swaps in a new badbits list. Pages it blocks are hidden from search at once and erased
    /// by the purge thread, which only runs if the list gained entries (#231).
    pub fn set_denylist(&self, denylist: Denylist) {
        let added = {
            let _writes = self.writes.write().unwrap();
            let mut current = self.denylist.write().unwrap();
            let added = denylist.adds_to(&current);
            *current = denylist;
            added
        };
        if added && self.store.get().is_some() {
            self.request_purge();
        }
    }

    /// Same as set_denylist, for the local takedown list
    pub fn set_local_denylist(&self, denylist: Denylist) {
        let added = {
            let _writes = self.writes.write().unwrap();
            let mut current = self.local_denylist.write().unwrap();
            let added = denylist.adds_to(&current);
            *current = denylist;
            added
        };
        if added && self.store.get().is_some() {
            self.request_purge();
        }
    }
```

In-memory mode has no `attach_store`, and `store.get()` is `None` until first use. In `purge_works_without_a_data_dir`, `persist` initializes the in-memory store first, so `store.get()` is `Some` by the time the list is set. That's the intended order.

**`admin.rs`:**
- `apply` returns `std::io::Result<()>`: `queue.set_local_denylist(Denylist::parse(&text)); Ok(())`.
- `reload_local` returns `std::io::Result<()>`.
- In `edit_local`, the closure that formats the reply loses its `pages` argument. Change the deny reply to `"Blocked; removal from disk queued".to_string()`, and adjust the undeny closure's parameters to match. Read `edit_local`'s signature in `admin.rs` and remove the third closure parameter (the purge count) throughout.

**`main.rs`:**
- After the merge thread, spawn the purge thread:

```rust
    // erases pages a denylist change blocks (#231): search hides them at once, this takes them
    // off disk; started after attach_store, which queues one full purge
    let purge_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || loop {
        let round = std::panic::catch_unwind(AssertUnwindSafe(|| purge_queue.run_purges()));
        if round.is_err() {
            error!("Denylist purge panicked; carrying on");
        }
    });
    // pages that expired while the indexer was down; the refresh thread does it daily after
    let expire_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || {
        let expired = expire_queue.expire(max_age_secs);
        if expired > 0 {
            info!("Expired {} pages older than the max age", expired);
        }
    });
```

- `main` uses `set_local_denylist`'s return value nowhere except through `admin::reload_local`. Fix any `let _ =` / `.unwrap()` uses that the new `()` return breaks.

- [ ] **Step 4: Run all the tests**

Run: `cargo fmt --all && cargo test 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: `test result: ok.`

- [ ] **Step 5: Commit**

```bash
git add src/denylist.rs src/index_queue.rs src/admin.rs src/main.rs
git commit -m "Denylist purge on its own thread, only when a list gains entries; expiry at startup (#231)"
```

---

### Task 5: docs and version

**Files:**
- Modify: `README.md`: the Persistence section's **Startup** and **Takedowns** bullets; `/admin/deny`'s reply in the API table; `/roots` row.
- Modify: `Cargo.toml`: `version = "0.3.0"`. Also `Cargo.lock` via `cargo build`.

- [ ] **Step 1: Edit the README**

- **Startup bullet:** replace it with: "**Startup:** both denylists are loaded first. Nothing else is loaded into memory: pages, mirrors and the queue stay in `index.sqlite` (#231). Crawling and serving start at once. Pages blocked or expired while the indexer was down are hidden from search immediately, and a background purge and expiry erase them within minutes."
- **Takedowns, first sub-bullet:** replace it with: "When a denylist gains entries, pages it blocks leave search results at once (each result is checked against both lists). A background scan of `pages` then deletes their rows from `index.sqlite`, where `secure_delete` overwrites them with zeros. At 100M pages the scan takes a few minutes. A list that's re-read unchanged (the local list every 5 minutes) triggers no scan."
- **API table, `/admin/deny` row:** returns `Blocked; removal from disk queued`.
- **API table, `/roots` row:** add "counted in a `roots` table kept with the pages and queue, not scanned".

- [ ] **Step 2: Bump the version**

Set `version = "0.3.0"` in `Cargo.toml`, then run `cargo build` to update `Cargo.lock`.

- [ ] **Step 3: Run all the tests and the format check**

Run: `cargo fmt --all -- --check && cargo test 2>&1 | grep -E "^test result|FAILED"`
Expected: `test result: ok.`

- [ ] **Step 4: Commit**

```bash
git add README.md Cargo.toml Cargo.lock
git commit -m "README: pages stay on disk, background purge, roots table; v0.3.0 (#231)"
```

---

## Measurement (controller, not a task)

After Task 5, run a throwaway test, not committed: 1M synthetic pages through `persist` with `DATA_DIR`. Record:
- RSS at 0 and at 1M pages, against `main`;
- `known` p50/p99 over 100k random items (half indexed);
- `attach_store` time on the 1M-page index;
- `root_counts` latency;
- `purge_blocked` time at 1M (×100 for prod).

These numbers go in the PR description.
