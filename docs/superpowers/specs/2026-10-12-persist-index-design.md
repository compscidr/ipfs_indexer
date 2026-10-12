# Persist the index on disk (#106): design

**Status:** approved in discussion, pending spec review · **Date:** 2026-10-12

## Goal

Keep indexed pages across restarts, without ever serving content that has been taken down or denylisted, and without keeping it on disk.

**Scope ("persist + reload"):** pages are stored on disk and reloaded into memory at startup. The in-memory keyword index and search are unchanged. An on-disk inverted index with an LRU cache (the issue's original idea) is out of scope until memory is actually a problem.

The acceptance criteria in #106 are binding:
1. Takedowns reach the disk.
2. Startup re-checks everything before serving.
3. The index isn't backed up.
4. Entries expire.
5. A one-command wipe.
6. Store only what's needed.
7. Retention is documented.

## Approach

SQLite through `rusqlite` (bundled build), one file at `$DATA_DIR/index.sqlite`, with:
- `PRAGMA secure_delete = ON`: deleted rows are overwritten with zeros in the database file, so a purge erases the text, not just the index entry.
- `PRAGMA journal_mode = MEMORY`: no journal file on disk at all. An on-disk rollback journal (DELETE mode) or WAL would write plaintext copies of pages and later only unlink them (changed after review). The trade-off: a crash mid-write can corrupt the file, which is recovered with the wipe; the index is rebuildable.

Rejected:
- **`redb`:** freed pages can keep deleted data until they're reused, so criterion 1 can't be shown.
- **A plain JSON-lines file:** a full rewrite on every purge, and fragile on crash.

## Behavior

### Opt-in
- `DATA_DIR` unset: no store; the indexer runs fully in memory exactly as today. Tests and local runs need nothing.
- `DATA_DIR` set: open or create `index.sqlite` there. If it can't be opened, **exit with an error**. Silently falling back to memory would hide a broken mount. The iac role sets `DATA_DIR=/var/lib/ipfs_indexer`, which is `/data/ipfs_indexer` on ubuntu-cube.

### Schema
```sql
CREATE TABLE IF NOT EXISTS pages (
  item       TEXT PRIMARY KEY,  -- the queued "cid[/path]", the IndexQueue.map key
  cid        TEXT NOT NULL,     -- IndexResult.cid (after redirects)
  title      TEXT NOT NULL,
  excerpt    TEXT NOT NULL,
  keywords   TEXT NOT NULL,     -- "word count" per line; words never contain whitespace
  indexed_at INTEGER NOT NULL   -- unix seconds
);
```
These are exactly the fields indexed today (criterion 6).

### Startup order (`main.rs`)
1. Fetch badbits (as today; exit if it fails).
2. **Load `LOCAL_DENYLIST` once, synchronously,** if set. Today the first load happens on the background thread and would race step 3.
3. If `DATA_DIR` is set, open the store and run **`attach_store`**:
   - delete rows whose `item` or `cid` is blocked by either denylist;
   - delete rows older than `INDEX_MAX_AGE_DAYS` (default 30);
   - load the rest into `map` and rebuild `keywords` and `keyword_rank`;
   - log `Loaded N pages from <path> (dropped B blocked, E expired)`.
4. Start the local-denylist refresh thread (5 min), the badbits refresh thread (24 h), crawler workers and the HTTP server.

Nothing is crawled or searchable before step 3 finishes (criterion 2).

### While running
- **Insert:** when a worker indexes a page (`start()` → `map.insert`), it also **upserts** the row with `indexed_at = now`.
- **Purge:** `set_denylist` and `set_local_denylist` already remove blocked entries from `map`. The removed `item`s are now collected and **deleted from the store in one transaction** (criterion 1).
- **Expiry:** the 24 h refresh thread also calls `expire()`. It selects rows older than the max age, deletes them, and removes those `item`s from `map` (criterion 4). It then re-queues the seed (`reseed()`), so the crawl walks back to expired pages instead of leaving a long-running instance empty (changed after review).

### Keyword rebuild
- The per-word insertion in `process_content` moves into one helper, `index_keywords(key, counts)`, used by both crawling and reload, so the in-memory index is built the same way either way.
- **Same key semantics as today:** keyword entries point at `IndexResult.cid`, and `search()` looks up `map` by that value.

### Wipe and backups
- **Wipe (criterion 5):** documented, not coded. Stop the container, `rm /data/ipfs_indexer/index.sqlite*`, start it. The next start begins empty.
- **Backups (criterion 3):** README states that the index is deliberately not backed up, because it can be rebuilt by crawling and backups are copies takedowns can't reach. The iac role gets a matching comment. Nothing in iac backs up cube's `/data/ipfs_indexer` today.

### Docs (criterion 7)
README "Persistence" section:
- what's stored, and where;
- the 30-day expiry and `INDEX_MAX_AGE_DAYS`;
- that purges zero the rows on disk (`secure_delete`);
- the wipe command;
- that there are no backups.

## Out of scope

- An on-disk inverted index / LRU cache.
- Re-crawling expired pages on a schedule.
- Fixing the existing map-key vs `IndexResult.cid` mismatch for redirected pages: keywords point at the post-redirect CID while `map` is keyed by the queued item, so search misses redirected pages. Track it on #209, where search lookups are being reworked.

## Testing

All tests are in Rust (`cargo test`), using a temp directory for the database.

- **Round trip:** index pages with a store attached, drop the queue, attach a new queue to the same file. The pages and keyword search results come back.
- **Purge reaches the disk:** index a page with a unique title, purge it via `set_denylist`. The row is gone (`SELECT`), **and the title's bytes no longer appear in `index.sqlite`'s raw contents**. Without `secure_delete` this assertion fails, which the test proves by toggling it once.
- **Startup re-check:** a row blocked by a denylist present at attach time is deleted from the file and never loaded into `map`.
- **Local denylist before attach:** an entry in `LOCAL_DENYLIST` blocks a stored page at startup (the race fix).
- **Expiry:** rows with an old `indexed_at` are dropped at attach and by `expire()`, both from the file and from `map`.
- **No store:** with `DATA_DIR` unset, nothing is written.
- **Bad `DATA_DIR`:** an unopenable path is an error.
- **Docker:** build the image; run with `DATA_DIR` on a temp volume; index a few pages, restart, and confirm they're still searchable and the log line reports them loaded.
