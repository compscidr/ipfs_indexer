# Top-K search with block-max pruning (#248): design

**Status:** draft · **Date:** 2026-10-15

## Goal

Make search time depend on how many results are shown, not on how many pages match.

Today `search` reads every posting of every query term, scores every match, sorts them all, and then takes `offset..offset+limit`. On prod (v0.3.0, 1.3M pages, `?limit=100`):

| Query | Matches | Time |
|---|---|---|
| `wikipedia` | 1.0M | 3.2 s |
| `united states` | 326k | 1.9 s |
| `history` | 549k | 1.4 s |
| `war` | 286k | 0.7 s |
| `rust` | 5.9k | 0.07 s |

The time is linear in matches. At the roughly 100M pages of a 30-day window, `wikipedia` would take about 4 minutes. darksearch asks for 100 results (`?limit=100`) and shows 50.

**Success:**
1. **Same results.** For any query and K, the pages returned and their order match an exhaustive search that uses the same `df` (below).
2. **Fast common words.** Top 100 in under 50 ms at 10M synthetic pages for: a word on every page, two common words, a rare word plus a common one, and an `OR` of two common words.
3. **Cheap totals.** One-word totals cost nothing extra. Multi-word totals are exact when small, and otherwise estimated from a bounded sample.
4. **No wipe.** Existing v1 segments keep working and are upgraded in the background.

## Decisions (agreed)

- **Totals: exact for one word, an estimate for several words** (the user's option B). An estimate is shown as `about N`.
- **Limits: cap them** (option A). `limit` defaults to 100. `offset + limit` is clamped to 1,000.
- **Approach: block-max, document at a time** (approach 1), over the existing segments.

## Segment format v2

- **Version:** `VERSION = 2`.
- **Block header:** n, last id (absolute), body byte length, max count, and new, the **shortest page length** among the block's postings, as a varint. The body is unchanged.
- **Writing it:**
  - `SegmentWriter` already receives every page's length (`add_doc`) before any term (`add_term`). It keeps the (id, length) pairs it has written, sorted by id, and `add_term` looks each posting's length up there.
  - Flushes, merges and #249 erase rewrites all write through `add_term`, so all three produce v2 with no other change.
- **Reading v1 and v2:**
  - `read()` accepts versions 1 and 2.
  - A v1 block has no shortest length, so its bound uses the segment's shortest page length. That length is computed at open from the `lengths` array already loaded. The bound is valid but loose.
- **Upgrade:**
  - Each round, the merge thread rewrites at most one v1 segment to v2 on its own. It uses the same rewrite path as #249 erasure: the new file is installed, and the old one is retired, then zeroed, synced and removed once no search reads it.
  - Segments of 1 GiB or more are included, since merges never touch them. Memory is bounded by one segment's rewrite, as for erasure.
  - Each upgrade is logged.

## Cursors

A term cursor reads one term's postings in one segment, block by block, through a read-ahead buffer (64 KiB).

- **`seek(target)`:** moves to the first posting with id ≥ `target`.
  - It walks block headers, skipping bodies, until it reaches a block whose last id ≥ `target`. Only that block's body is decoded.
  - It never moves backwards.
- **Current block:** exposes the block's last id, max count and shortest length, for bounds, before its body is decoded.
- **Current posting:** id and count. The page length comes from `Segment::length(id)`, as now.
- **`postings_at`** stays for merges and rewrites.

## Query execution

Parsing is unchanged: an AND of OR groups, with stopwords dropped.

1. **`df`.** For each term, `df` is the sum of its dictionary `df` across live segments, plus its postings in the write buffer and the frozen buffer.
   - This counts superseded versions (dead ids) until a merge or erasure removes them. Today's `df` counts live postings only.
   - The difference is small except right after `/admin/recrawl`, when it can reach about 2× until merges catch up. Scores shift slightly with it.
   - If any group's `df` is 0, the result is empty with a total of 0.
2. **Weight.** Same BM25 as now: `idf = ln(1 + (pages − df + 0.5)/(df + 0.5))`, `weight(count, length) = idf · count · (k1+1) / (count + k1·(1 − b + b·length/average))`, with k1 = 1.2 and b = 0.75.
   - `weight` increases with count and decreases with length, so `weight(max count, shortest length)` is an upper bound for every posting in a block.
   - Bounds are computed at query time from the current idf and average, so they never go stale.
3. **Heap.** It holds K = `offset + limit` entries, ordered by score descending, then lower page id, as today's sort is.
   - θ is the K-th score once the heap is full, and −∞ before that.
   - A candidate enters only if it beats the K-th entry in that order.
4. **Segments, largest first, sharing the heap:**
   - Skip a segment that lacks any group entirely.
   - **Lead:** the group with the smallest `df` in this segment. An OR group is the union of its terms' cursors.
   - For the lead's current candidate id, move every other group's cursors to the block that could hold it. This reads headers only.
   - **Bound:** the sum over groups of the group's block bound. For an OR group, that's the sum of its terms' bounds.
     - If the bound is below θ, advance the lead past the smallest last id among the current blocks. No body is decoded.
     - Otherwise, seek every group to the candidate. If a group lands past it, that id becomes the next candidate (leapfrog).
   - **Score:** if every group has the candidate and it isn't in the dead set, compute its exact score (the sum of the weights of the terms the page has, as now) and offer it to the heap.
   - **Rounding:** bounds are sums of per-cursor upper bounds, so a bound is never below the score it bounds except by floating-point rounding. A region is skipped only when bound × (1 + 1e-9) < θ (strict), so ties are scored.
5. **Write buffer and frozen buffer:** scored exhaustively into the same heap, as today. They hold at most 60 s of pages.
6. **After the walk:**
   - Take heap entries `offset..offset+limit` in order.
   - Resolve them with `pages_by_id`.
   - Apply the serve-time denylist check as now. Results can still come back short while a purge is pending.
7. **Snapshot:** searches read through the same `Arc<Segment>` snapshot and buffer read lock as today. A retired file is erased only once no search reads it, so cursors are safe across merges and erasures.

## Totals

- **One group with one term:** the total is that term's `df` (above). No extra work. It reads high by the number of superseded versions not yet removed.
- **Otherwise, when the lead group's `df` across segments is 10,000 or fewer:** the total is an exact count. Walk the lead group and seek the others, without scoring or pruning, and skip dead ids. That's at most 10,000 lead postings.
- **Otherwise:** an estimate.
  - In each segment, walk the first 1,024 postings of the lead group, seek the others, and count the live matches.
  - Estimate = Σ over segments of (matches ÷ postings walked) × the lead group's `df` in that segment.
  - Add the buffers' exact counts, and round to two significant figures.
- **Output:**
  - An exact total keeps `Results for {q} ({n} total): `.
  - An estimate becomes `Results for {q} (about {n} total): `.
  - `search` returns the total with a flag saying whether it's an estimate.

## API

- **`limit`** defaults to 100 (it was all results).
- **`offset + limit`** is clamped to 1,000. With `offset` at 1,000 or more, the result list is empty but the total is still reported.
- Non-numeric values still return 400.
- README: document the defaults, the clamp and "about".

**darksearch:** `fetchResults`' regex becomes `\((?:about )?(\d+) total\)`, and it keeps whether "about" was present. The page shows "Showing the first 50 of about 330,000 results". This PR must merge before this release deploys. With the old regex, an "about" total falls back to the count of visible results.

## Errors

- A read or decode error in a cursor is logged, and the search returns no results, as a postings error does today.
- A v1 segment that fails its upgrade rewrite stays v1 (still searchable with loose bounds), and the error is logged. It's tried again next round.

## Testing

- **Same results (Success 1):** a randomized test builds several segments (some v1, some v2), a write buffer and dead ids, with skewed term frequencies and page lengths. Over hundreds of queries (one word, AND, OR, mixed; K from 1 to 1,000; offsets), `search`'s ids and order must equal a test-only exhaustive search that uses the same `df`.
- **Bounds:** for v1 and v2 segments, every block's bound is at least the weight of every posting in it.
- **Cursor:** `seek` hits exact ids, ids between postings, ids past the end, and block boundaries.
- **Pruning works:** on a skewed corpus, a top-10 query decodes fewer blocks than the term has (counted by a test hook).
- **Upgrade:** a v1 segment is rewritten to v2 by one merge round, the old file is erased (`dir_has`), and results are unchanged.
- **Totals:** an exact count at or below 10,000; an estimate above it, formatted as `about`, and within 25% on a uniform synthetic corpus.
- **API:** the default `limit` is 100; `offset + limit` is clamped to 1,000; `offset` ≥ 1,000 gives empty results with the total.
- **Measurement (throwaway, not committed):** synthetic 1M and 10M pages with Zipf-distributed terms, timing the Success 2 queries before (main) and after. The numbers go in the PR.

## Out of scope

- Positions and phrase queries (#128).
- Changing the BM25 formula, or making `df` exact again.
- Caching query results.
- Hiding denylisted pages before they enter the heap. The serve-time check stays after resolution, as in v0.3.0.
