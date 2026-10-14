# Pages live only on disk (#231): design

**Status:** draft · **Date:** 2026-10-14

## Goal

Stop the indexer's memory from growing with the number of indexed pages. Today `IndexQueue` keeps every page's item, CID, title and excerpt in an in-memory map, and every mirror in another one. That's roughly 500–600 bytes per page. `index.sqlite`'s `pages` and `mirrors` tables already hold the same data.

On prod (v0.2.9, 30 workers, about 2,300 pages/min), the 30-day window reaches about 100M pages. That's roughly 55 GB of map on a 30 GB host, so the indexer would run out of memory in about 2–3 weeks. An out-of-memory kill mid-write can corrupt `index.sqlite`, which v0.2.9 then wipes and rebuilds. This design removes both maps and makes SQLite the only copy.

**Success:**
1. **Flat memory.** Resident memory doesn't grow with the number of indexed pages, except for what segments already keep (per-page lengths, about 12 bytes per page). Measured as RSS at 0 and at 1M synthetic pages.
2. **Fast startup.** Startup reads neither every page nor every queued item. Time to serving doesn't depend on index or queue size, apart from the existing segment checksums.
3. **Takedowns are still hidden at once.** A page blocked by either denylist is never returned by search, from the moment the new list is in place. Its rows and segment text are erased by a background scan that starts at once and finishes within minutes at 100M pages, not seconds.
4. **Same behaviour otherwise.** The existing takedown, expiry, mirror, recrawl and search tests pass unchanged in what they check.
5. **Cheap `/roots`.** It answers without scanning `pages` or `queue`.

## Where this sits

- **Before #248 (top-K search).** Search resolves results through the store here; #248 then changes how candidates are scored. Only the result resolution overlaps.
- **#245 (writer thread)** is unaffected.
- **Segments (#247/#249)** are unchanged.

## What replaces each use of the maps

`IndexQueue.map` (item → page) and `IndexQueue.mirrors` (item → original item) are deleted.

| Use | Today | After |
|---|---|---|
| Already indexed or mirrored, when admitting a link (`admit`) | map and mirrors lookups | `Store::known(item)`: `SELECT 1 FROM pages WHERE item = ?` or `SELECT 1 FROM mirrors WHERE item = ?`, on a dedicated read connection (`lookup`) so it doesn't wait on search or writes |
| Same check when a worker pops an item | none (only the startup queue load dropped indexed items) | `known(item)` on pop: a queued item indexed in the meantime is skipped, not fetched again |
| `index_length` (`/status` Index size, BM25's page count) | `map.len()` | `Store.pages` counter (exists) |
| `mirror_length` | `mirrors.len()` | a new `Store.mirrors` counter, kept the same way as `pages` |
| Search results → pages | `map.get(item)` | `Store::pages_by_id(ids)`: `item, cid, title, excerpt` for the K ids shown; ids not found (deleted meanwhile) are dropped, as now |
| Expiry | `items_older_than` scans `pages` | same query, using a new index `pages_indexed_at ON pages (indexed_at)` |
| `/roots` | iterates the map, plus `GROUP BY` over the whole `queue` | a `roots` table (below) |
| `/admin/recrawl` | collects every map key | streams `pages.item` in id order, in batches of `WRITE_BATCH`, into the queue |
| `persist`'s mirror bookkeeping | updates both maps | nothing: `Store::upsert` already maintains `mirrors` |
| Denylist purge | `map.retain` / `mirrors.retain` under the writes lock | a background scan (below) |
| Startup (`attach_store`) | loads every page, checks lists/expiry/spelling, loads every queued item | none of that (below) |

## Denylists

Badbits entries are SHA-256 hashes of `cid/` and `cid/path` prefixes, so the only way to find stored pages a new entry blocks is to hash each page's prefixes. At 100M pages that's a scan of a minute or a few.

### Hidden at once: checked when serving

`search` checks each result's item and CID against both lists before returning it (`blocked_quietly`, a few hashes per result, for at most `offset + limit` results). A blocked page is never served, whether or not the scan has reached it yet.

### Erased by a background purge

- **Swap first.** `set_denylist` / `set_local_denylist` take the writes lock only to swap the list in, as now. `persist` holds the read side across its check and insert, so once the swap returns, no blocked page can be written. Every page written before the swap is in the table by then.
- **Queue the change.** The swap computes the entries in the new list that weren't in the old one. If there are none (a re-read local list that didn't change, or a badbits refresh with only removals), nothing is queued and no scan runs. Otherwise the new entries go to a purge job.
- **One purge thread.** It takes everything queued so far, then scans `pages` in id order (`WHERE id > ? ORDER BY id LIMIT 10000`) on the `lookup` connection. Each row's item and CID are checked against the queued entries, and matches are deleted with `Store::delete(items, true)` (batched, erasing). Then it runs the existing blocked-superseded-versions pass over `dead_pages`. Entries queued during a scan go to the next one. The scan holds no lock across batches.
- **Mirrors.** `mirrors` rows whose own item is blocked are deleted the same way, by a scan of `mirrors`. Mirrors of a purged page already go with it in `delete`.
- **Return values.** `set_local_denylist` currently returns the number of pages purged, which `/admin/deny` reports ("Blocked; N pages removed"). It now returns once the swap is done, so the response becomes `Blocked; removal from disk queued` and the count goes to the log when the scan finishes. darksearch shows this text as is.
- **Startup.** With no previous list to diff against, startup queues one full purge (every entry of both lists) after the store is attached. It runs in the background while crawling and serving start, and the check at serving time covers the gap.

## Startup without loading pages

`attach_store` currently loads every page (blocked/expired/spelling checks) and every queued item (dropping blocked, indexed or misspelled ones). After this change it:

1. attaches the store;
2. queues a full purge (above);
3. runs one expiry (`expire`, the indexed query) in the background, as the daily refresh thread does;
4. finishes the erasures a crash interrupted (`maintain`, as now).

**Removed:** the pre-#218 re-spelling of stored items (`rekey`, `normalize_item` on load). Like the migrations dropped in v0.2.9, it only fixed old data, and prod has none since the wipe. Queued items are already normalized by `admit` when added. Blocked and already-indexed queued items are now skipped when popped (`process_next` already checks `is_blocked`, and adds `known`).

`attach_store`'s return value (pages loaded, blocked, expired) becomes the page count and the number of queued items; blocked and expired counts are logged when their background jobs finish.

## `roots` table

```sql
CREATE TABLE IF NOT EXISTS roots (
    root   TEXT PRIMARY KEY,  -- the item up to its first '/'
    pages  INTEGER NOT NULL,  -- indexed pages under it
    queued INTEGER NOT NULL   -- queued items under it
);
```

- **Kept in step** with the rows it counts, inside the same transaction:
  - `upsert` of a new item: `pages + 1`;
  - `remove_page` / `delete`: `pages − 1` (re-crawls and mirrors net out);
  - `queue_add` / `queue_pop` / `queue_remove`: `queued ± 1`, only for rows actually inserted or deleted.

  Rows reaching zero in both columns are deleted.
- **`/roots`** joins `roots` with `pages` for each root page's title (`pages.item = roots.root`). That's one indexed lookup per root, and there are few roots.
- **Filled once, the first time a v0.2.9 index is opened.** The fill is two `INSERT … SELECT … GROUP BY` statements, one over `pages` and one over `queue`, in one transaction, and it happens before anything else writes. Memory stays bounded: the rollback journal only covers the new `roots` pages, and the `GROUP BY` sorter holds one row per root (hundreds). On prod (about 2M pages and 3M queued items at deploy time) this takes seconds. The presence of the `roots` table marks it done.

## Read connections

`Store` has a write connection and one read connection (`reader`, used by search and `items`). Two more read-only connections are added, so the new per-link and per-pop lookups and the purge scan don't queue behind a long search:

- `lookup`, for `known`;
- `scan`, for the purge job and `/admin/recrawl`'s stream.

They're opened like `reader`: read-only, with a busy timeout and the default cache size. In-memory mode (no `DATA_DIR`) has no separate connections, so they all fall back to the write connection, as `read_conn` does today.

## Errors

- **`known` fails** (store error): the link is admitted. Admitting a known page re-queues it, which wastes a fetch. Losing a link would mean never crawling it.
- **`pages_by_id` fails:** search logs and returns no results, as `items` failing does today.
- **A purge batch fails:** it's logged and the scan goes on with the next batch. Unpurged pages are still hidden by the check at serving time, and the next startup's full purge retries them.
- **Counters** (`pages`, `mirrors`) are derived from the tables at open, as `pages` is now, so a crash can't leave them wrong across restarts.

## Testing

- **Existing tests:** they insert into `queue.map` directly and switch to `index_for_test` or `persist`. What they assert stays the same (Success 4).
- **New tests:**
  - A page blocked by a new local list entry is absent from `search` immediately after `set_local_denylist`, before the purge job has run; after `purge_for_test()` it's gone from `pages` and from every file (`dir_has`).
  - Setting the same local list twice queues no purge.
  - `attach_store` on a store with pages and queued items doesn't call `load_all` / `queue_load`. Asserted by those functions being removed, plus a test that a blocked queued item is skipped on pop and an already-indexed one isn't fetched again.
  - `roots` counts match a `GROUP BY` over `pages` and `queue` after a mix of upserts, re-crawls, mirrors, deletes, queue adds, pops and removes; and after reopening.
  - The `roots` fill on a v0.2.9-shaped index matches the `GROUP BY`.
  - `/admin/recrawl` queues every indexed page with more than `WRITE_BATCH` pages.
  - `known` is true for indexed pages and mirrors, false otherwise.
- **Measurement:** a throwaway test (not committed), as for #247:
  - 1M synthetic pages through `persist`;
  - RSS before and after against `main`;
  - `known` p50/p99;
  - startup time;
  - `/roots` latency;
  - purge scan time at 1M (×100 for prod's 100M).

## Out of scope

- **Per-segment page lengths** (12 bytes per page in memory): fine up to about 100M pages; moving them out can come later.
- **The denylist sets themselves** (about 511k hashes, roughly 50 MB): fixed size, not per page.
- **#248's top-K scoring.** Search still reads every posting; only result resolution changes here.
