# Segmented inverted index with takedown erasure (#247, #249): design

**Status:** approved · **Date:** 2026-10-13

## Goal

Replace the SQLite `postings` and `terms` tables with our own immutable, sorted segment files (#247), so indexing writes sequentially instead of scattering B-tree updates, and search never waits on the crawl. Ship it together with takedown erasure (#249): a takedown hides the page from search at once, and promptly afterwards no file under `DATA_DIR` contains its terms.

SQLite keeps `pages`, `queue` and `mirrors` (#255).

**Success:**
1. Search returns the same results, in the same order, as the SQLite postings do today (BM25 #251, OR #252).
2. After a takedown the page is gone from search before `delete` returns. Within seconds (not at the next routine merge), the bytes of its terms are in no file under `DATA_DIR`.
3. Expired pages and superseded versions of re-crawled pages also leave the disk, within a bounded time.
4. A crash loses at most the last unflushed buffer of pages, and those are re-queued, not silently lost.
5. Measured before and after on the synthetic copy of prod: `Store upsert … took`, index size on disk, and `/search` latency.

## Where this sits among the open work

- **Lands after #255, #256 and #257.** All three rewrite `upsert`, `delete`, `postings` and `search`. This branch rebases onto `main` once they're merged.
- **#244 (term ids) is superseded.** Each segment has its own term dictionary and stores page ids as varints, so term text is no longer repeated per posting. Close #244.
- **#245 (writer thread) isn't a prerequisite.** Its cost driver, a per-page commit of scattered `postings` writes, goes away. The Store buffers postings itself. #245 can still follow for `queue_add` and lock contention.
- **#250 (benchmark game) isn't a prerequisite.** Numbers come from the existing timings (see Success 5).
- **#248 (top-K) and #128 (positions)** aren't built. The format leaves room for both: postings in fixed blocks with a per-block header, and a version byte.
- **#231 (memory cap):** the write buffer flushes at a size cap, and segment dictionaries are read through a sparse in-memory index, so memory doesn't grow with the index.

## Layout on disk

```
$DATA_DIR/index.sqlite         pages, queue, mirrors, dead_pages (postings/terms dropped)
$DATA_DIR/segments/MANIFEST    the live segments; replaced atomically
$DATA_DIR/segments/<gen>.seg   immutable segment files, <gen> a 16-digit hex counter
```

### Page ids

`pages.id` becomes `INTEGER PRIMARY KEY AUTOINCREMENT`, so an id is never reused. Otherwise a dead id still present in a segment could come back as a different page.

A re-crawl gives the page a **new id**. The old id becomes dead, because its postings sit in an older, immutable segment. `mirrors.page` is repointed from the old id to the new one.

### `dead_pages`

```sql
CREATE TABLE dead_pages (
    id      INTEGER PRIMARY KEY,  -- a page id that may still be in a segment
    item    TEXT NOT NULL,        -- so a takedown can find every version of an item
    cid     TEXT NOT NULL,        -- and versions whose own (redirected) cid gets blocked
    died_at INTEGER NOT NULL,     -- unix seconds
    erase   INTEGER NOT NULL      -- 1: rewrite the segments holding it now
);
```

- Ids are inserted when a page is deleted (takedown, expiry, the #255 mirror case) or superseded by a re-crawl.
- A row is removed once no live segment contains its id.
- The ids are mirrored in memory as a `HashSet<u64>` behind an `RwLock`, which search filters on. The table is how that set survives a restart.
- `item` is the same string as `pages.item`. A takedown deletes these rows along with the page, so it adds no text that the current schema doesn't already hold.

### `MANIFEST`

```
ipxman 1
next_gen <hex>
flushed_through <id>
<file name> <min_id> <max_id> <docs> <bytes>
...
```

- `flushed_through` is the highest page id whose postings are in a segment.
- Writing it: write `MANIFEST.tmp`, fsync, rename it over `MANIFEST`, then fsync the directory.
- A segment file is visible only once the manifest that names it is in place.
- At startup, `.seg` files the manifest doesn't name (left over from a crash mid-flush or mid-merge) are erased and removed.

### Segment file (version 1)

All integers are LEB128 varints unless marked otherwise.

```
header    "ipxseg" , version u8 (=1)
docs      count; then (id delta, length) per page, ascending by id
postings  per term: blocks of up to 128 postings
            block header: n, last id (absolute), byte length of the block body,
                          max count   -- block-max data for #248, unused for now
            block body:   id deltas, then counts
dict      per term, sorted bytewise: shared-prefix length, suffix length, suffix bytes,
          df, offset of its first block in the postings section
sparse    every 64th dictionary entry: full term, offset into dict
footer    offsets of docs/postings/dict/sparse (u64 LE each), FNV-1a 64 of everything before it (u64 LE), "ipxend"
```

- Per-page lengths live in the segment, as Lucene's norms do. BM25 then scores without a SQLite join, and SQLite is only asked for the item names of the results shown.
- **Opening a segment:** read the footer, check the checksum, load the sparse index (about one term in 64) and the docs section's id range. Postings and dictionary blocks are read on demand with `pread` (`FileExt::read_at`).
- **Term lookup:** binary-search the sparse index, read that one dictionary block (≤ 64 entries), and scan it.

## Write path

- `upsert` still commits the page row in SQLite as today: a new id, the old id moved to `dead_pages`, and the `mirrors` repoint.
- In the same critical section (the connection lock), `upsert` adds the page's postings to the **write buffer**: `BTreeMap<term, Vec<(id, count)>>` plus `(id, length)`. Ids only increase, so each list is already sorted.
- Search reads the buffer as one more segment.
- **Flush** happens when the buffer holds 1,000,000 postings or is 60 s old, whichever comes first, and never while it's empty. A flush writes a new `.seg` file, fsyncs it, and swaps the manifest with `flushed_through` set to the buffer's highest id. It runs on the background **upkeep thread**, and doesn't wait for a merge in progress. The buffer is swapped for an empty one under its lock, so `upsert` doesn't wait on disk.
- **In-memory store (no `DATA_DIR`):** the buffer is never flushed and holds everything. That matches today's in-memory mode, where the cache cap doesn't apply.
- **Crash recovery:** at startup, pages with `id > flushed_through` have no postings on disk. Their rows are deleted and their items re-queued, so they're crawled again.

## Read path

- A search takes a **snapshot**: an `Arc<Vec<Arc<Segment>>>` of the live segments, plus a read lock on the buffer for the length of the query. A manifest swap replaces the `Arc`; readers holding the old one keep using it, so writes never block search.
- `Store::postings(term)` returns `(id, count, length)` from every segment and the buffer, dropping ids in the dead set.
- `df` is the number of live postings, exact as today.
- `search()` keys pages by id instead of item. After ranking, `Store::items(ids)` resolves only the shown page of results (`offset..offset+limit`) to items in one query. `total` counts live matches.
- The `map` check stays for the shown results only. A page whose store write failed has no postings, so it can't match anyway.
- **`term_count` and `top_terms` (`/status`, `/keywords`):** computed by a streaming k-way merge of the segment dictionaries plus the buffer, summing df. Memory is O(segments + n). The result is cached and invalidated on every manifest swap.
  - Like Lucene's docFreq, df here counts dead postings until they're merged away. The `/keywords` and `/status` numbers are approximate in that way; search scoring is not.

## Merging (#247)

- Merges run on a **merge thread** of their own. Flushes, erase rewrites and retiring run on the **upkeep thread** and never wait for a merge; a merge cascade also runs them between merges.
- **Installs** are a read-modify-write of the live set at install time: remove the inputs, add the output. A flush installed during a merge survives the merge's install. If an input is no longer live (an erase rewrite replaced it), the merge's output is erased and nothing is retired; a running merge checks for this and stops early, so it can't hold a taken-down file on disk until it finishes.
- **Tiers:** a segment's tier is `floor(log10(bytes / 1 MiB))`. When a tier has 10 segments, it merges them into one.
- **Size cap:** segments of tier 3 (1 GiB) or more are never merged, so the largest merge is ten segments under 1 GiB each (output up to ~10 GiB). This bounds the longest merge and what a takedown or expiry rewrite has to copy. The cost is more segments per search at the largest scale, which is accepted.
- **Dead postings:** a segment that is 20% or more dead (by docs) is rewritten on its own, which reclaims space and erases superseded versions.
- A merge streams the inputs' dictionaries in sorted order, skips dead ids, writes the new segment, swaps the manifest, and then **retires** the inputs.
- **Retiring a segment:** wait until no snapshot holds it (`Arc::strong_count == 1`, checked every 100 ms), then overwrite the file with zeros, fsync, and unlink it. Zeroing before readers finish would hand them zeros, which is why the wait comes first. Then delete the `dead_pages` rows for ids that no live segment contains any more.

## Takedowns (#249)

`delete(items, erase)`. Purges (badbits and the local list) and expiry pass `erase = true`. The #255 "re-crawl now matches another page" case passes `false`.

1. **One SQLite transaction:**
   - delete the `pages` rows, plus mirrors as #255 does;
   - insert their ids into `dead_pages` with `erase`;
   - for `erase`, set `erase = 1` on every older `dead_pages` row for the same items (superseded versions).
2. **Immediately, before `delete` returns:** add the ids to the in-memory dead set, so search stops returning them at once (as the `map` check does today). Remove their postings from the write buffer outright; it's only in RAM, so nothing needs erasing.
3. **Wake the upkeep thread.** It doesn't wait for merges (see Merging), and rewrites every segment whose `[min_id, max_id]` range contains an `erase = 1` id and whose docs section actually holds it. A rewrite drops *all* dead ids in that segment, and the old files are then retired as above.
4. **Superseded versions by cid:** a purge also checks each `dead_pages` row's `item` and `cid` against the new list and marks the blocked ones `erase = 1`. This catches an old version whose cid after redirects differs from the current page's.
5. **At startup,** any `erase = 1` rows left from before a crash are handled before the crawl or HTTP server starts.

- **Expiry:** segments are written in crawl order, so expired pages are mostly whole old segments, which get retired outright instead of rewritten.
- **Superseded versions:** `expire()` also marks `erase = 1` on `dead_pages` rows whose `died_at` is older than the max age. Old versions of re-crawled pages therefore don't outlive the retention window either.
- **Limits:** zeroing then unlinking makes the bytes unreadable through the filesystem. As with SQLite's `secure_delete`, it can't guarantee physical erasure on SSD wear-levelling or copy-on-write filesystems. That caveat is already true today; it goes in the README's retention section.

## Migration

Once at startup, when the `postings` table exists:

1. Stream `SELECT term, page, count FROM postings ORDER BY term, page` straight into one segment, with lengths from `pages.length`. It's already in sorted order, so memory stays flat. Write the manifest with `flushed_through = max(pages.id)`.
2. In one transaction: rebuild `pages` with `AUTOINCREMENT`, keeping ids; create `dead_pages`; drop `postings`, `terms` and `postings_page`.
3. `PRAGMA temp_store = MEMORY`, then `VACUUM`, so the freed space is returned and no temporary file ever holds plaintext. `secure_delete` already zeroes the dropped pages.

- **Crash safety:** if the process dies before step 2 commits, `postings` still exists. The next start erases any segments and the manifest, then redoes step 1.

## Code layout

- `src/segment.rs`: the file format. A writer from a sorted stream, a reader (open, lookup, iterate dictionary), and varint/FNV helpers. No knowledge of SQLite.
- `src/segments.rs`: the segment set. Manifest, write buffer, snapshots, dead set, and the upkeep (flush, erase, retire) and merge work run by two threads in `main.rs`.
- `src/store.rs`: SQLite only, plus calls into `segments` from `upsert`, `delete` and `postings`. The `postings`/`terms` SQL, `remove_postings` and `adjust_terms` go away.
- `src/index_queue.rs`: `search()` works on ids, and `delete` callers pass `erase`.

No new dependencies. `Store::open` returns a boxed error, since the migration and segment set can fail with I/O errors as well as SQLite ones.

## Testing

**Segment format**
- Round-trip (write → open → every term's postings), including multi-block terms and shared prefixes.
- Corrupted checksum refuses to open.

**Search parity**
- The existing search tests (BM25 order, OR, AND) pass unchanged.
- One new test runs the same queries before and after a flush and after a merge, and checks the results are equal.

**Erasure (#249 acceptance)** builds on `delete_erases_the_text_from_the_file`. Index a page with a unique term (`ZZQX-segment-term-91`), take it down, run upkeep, then assert the bytes are in no file under `DATA_DIR` (recursive). Run it for a page that is:
- still in the write buffer;
- in a flushed segment;
- in a merged segment;
- re-crawled, with the term only in the old version's segment;
- left over from a crash, with an `erase = 1` row present at startup.

**Erasure control**
- With segment rewrite disabled, the term *is* still found, which proves the assertion can fail.

**Immediacy**
- Search drops the page as soon as `delete` returns, before upkeep runs.

**Crash recovery**
- Pages past `flushed_through` are deleted and re-queued.
- Unnamed `.seg` files are erased.

**Concurrency**
- A search holding a snapshot completes correctly while a merge retires its segments (the existing `search_reads_while_a_write_is_in_progress`, extended).

**Migration**
- An old-schema file with postings migrates to segments with identical search results.
- The term bytes are then absent from `index.sqlite`.
