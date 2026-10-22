# Pause crawling per root, not for the whole gateway (#286): design

**Status:** agreed · **Date:** 2026-10-22

## Goal

#284 pauses every worker when 16 of the last 20 fetches are gateway errors. That was right while the queue was one root. Since discovery (#213, v0.5.1) the queue holds several large roots. On 2026-10-22, from 04:24 UTC, the crawl stood still for over 11 hours with 489,675 pages queued:

- kubo answered 504 for every page of Russian Wikipedia (`bafybeiezqkk…`). Its `/wiki/` directory is a huge sharded directory, and looking a page up in it outlasts kubo's timeout. All 48 pauses that day named that root.
- The whole crawl paused. Each 15-minute test popped the oldest page, which was again a Russian one. About 130 of them were queued together from the links on its front page.
- Meanwhile Turkish Wikipedia (274k queued) answered in under a second, and Chinese (214k queued) worked too. `/health` was 503 the whole time.

**Success:**
1. A root that keeps failing pauses only its own pages. Workers keep crawling every other root at the normal rate.
2. When kubo itself is down, every root with pages queued pauses, and the crawl counts as paused, as #284 does today: `/health` is 503, and the resolver (#213) waits.
3. `/status` and `/roots` show which roots are paused and why.
4. With no root paused, the crawl order is unchanged: the oldest queued page first.

## Decisions (agreed)

- **One pause per root, on #284's rule.** A root pauses when 16 of its last 20 fetches are gateway errors. The pause is 1 min, doubling up to 15 min, and then one test fetch of that root's oldest page decides whether it resumes. The constants stay as they are.
- **The gateway-wide pause goes.** "Every root with pages queued is paused" replaces it.
- **The queue records each page's root,** in a column with an index on (root, rowid).
- **The next page is the oldest page among roots that may fetch.** A root may fetch if it isn't paused, or if its pause has ended and nobody holds its test.
- **`/status`** adds `Paused roots: N`. `Gateway:` reads paused only when the whole crawl is.
- **`/roots`** adds a fifth column, the root's pause state or `-`, at the end of each line. darksearch reads the first four columns, so it keeps working. Showing the pause in darksearch's admin is a separate change.

## Unchanged

- What counts as a gateway error.
- Retrying a failed page at the end of the queue, up to 3 attempts in all.
- Dropping pages that are missing, skipped or blocked.
- One worker holding a given root's test, and the `TestTurn` guard: a test that fetches nothing goes back, and a panic during a test counts as a failure.

## 1. The queue (`src/store.rs`)

- **Schema:** `queue` gains `root TEXT NOT NULL`, the root CID of the item (`root_of`, the part before the first `/`), and an index on `(root, rowid)`.
- **Migration:** opening an index whose `queue` lacks the column adds it, fills it for every row, and creates the index, in one transaction. At production's 490k rows this takes seconds. A queue table that is a view in a test (see the `attempts` migration ruling in #284) must still open.
- **Writes:** `queue_add` and `queue_retry` set `root`.
- **Pop:** takes the oldest item whose root is not in a given set of roots to skip, with its attempts. It returns `None` when nothing is left outside that set. Cost:
  - Production has 71 roots with pages queued (out of 159 in `roots`). Popping runs about 25 times a second at 1,500 pages/min, while holding the store's write lock.
  - Finding the oldest page must cost about the same however many pages a paused root holds. A rowid scan that walks past a paused root's 200k queued pages on every pop is not acceptable.
  - The issue's approach meets this: for each root with `roots.queued > 0` that isn't skipped, look up its oldest rowid through the new index, and take the smallest.
- **`roots`** is unchanged. Its `queued` count already tracks pages queued per root.

## 2. Pauses per root (`src/breaker.rs`, `src/index_queue.rs`)

- `Breaker` keeps its rule and its tests. It becomes the state of one root.
- The queue keeps a map from root to `Breaker`:
  - **Created** on a root's first recorded fetch.
  - **Removed** once the root is closed (crawling normally) and has no pages queued, so the many one-page roots don't pile up.
  - **A paused root is never removed,** even with nothing queued, so a link that queues it again doesn't skip its pause.
- **A worker, per page:**
  1. Asks the map for the roots it must skip: paused roots whose pause hasn't ended, and paused roots whose test someone holds.
  2. Pops the oldest page outside that set. If there is none, it takes nothing and sleeps 200 ms, as an idle worker does now.
  3. Asks that root's breaker for a permit:
     - **Go:** fetch, and record the outcome against that root.
     - **Test:** fetch as the root's test, inside a `TestTurn`.
     - **Wait:** another worker got this root's test between steps 1 and 2. The page must go back without losing its place in the queue or its attempts. Reinserting it with its old rowid does both; the builder may pick another way that does.
  4. Blocked pages are skipped as now. A test whose page turns out to be blocked hands the test back.
- **Crawl paused:** `crawl_paused()` returns `Some(reason)` when at least one root has pages queued and every root with pages queued is paused. The reason names how many roots are paused and the last error. `/health` and the resolver's check (`discovery::resolve_next`) keep calling it, unchanged.
- **Logs:**
  - A warning when a root pauses, naming the root, the count, the pause length and the last error.
  - An info line when it resumes.
  - The same per failed test as now, naming the root.

## 3. Status

- **`/status`:**
  - `Gateway:` is `paused …` only while the whole crawl is paused. Otherwise it is as now (`ok 3s ago`, `error 3s ago: …`, `no fetches yet`).
  - `Paused roots: N` goes just before `Gateway:`. It is `Paused roots: 0` when none are paused.
  - darksearch matches the gateway text by its start (#80's "Paused"), so it keeps working.
- **`/roots`:** each line gains a fifth tab-separated field:
  - the root's pause in #284's wording (`paused 4m: 16 of the last 20 fetches failed; last: <error>`, or `testing after …`);
  - `-` when the root isn't paused.
  - Tabs and newlines in the error become spaces.

## Errors

- No new error paths.
- If the migration fails, the store fails to open as other migrations do.
- A store error while popping is logged and noted as now.

## Testing

- **Store:**
  - A queue table without `root`, as written by v0.5.1, gains it on open, filled correctly, and its rows pop in their old order.
  - `queue_add` and `queue_retry` set it.
  - A pop skipping a root returns the oldest page of the others.
  - A pop skipping every root with pages returns `None`.
  - Popping costs about the same with 100k pages of a skipped root at the head of the queue as with none: a test with a generous bound, or a query-plan check that the index is used.
- **Crawl,** against the local test HTTP server, as #284's tests do:
  - With root A answering 504 and root B answering 200, A's pages queued ahead of B's:
    - A pauses;
    - B's pages are indexed during A's pause;
    - A's test comes when its pause ends;
    - A's pages are retried, or dropped after 3 attempts.
  - With every root failing, the crawl counts as paused, and `crawl_paused()` and `/health` say so.
  - When A's test succeeds, A resumes.
  - A worker that pops a page of a root whose test was just taken puts it back without changing its place or attempts.
  - The breaker map drops a closed root once it has nothing queued, and keeps a paused one.
- **Status:**
  - `/status` shows `Paused roots: N`, and `Gateway:` reads paused only when everything is.
  - `/roots` has the fifth field.

## Release

v0.5.2. The column is added in place. The API changes only by additions: a field in `/status` and a column at the end of `/roots`. darksearch needs nothing.

## Left to the builder

- How the map of breakers is stored and locked, as long as the per-pop work stays small next to a fetch.
- The exact SQL for the pop, within the cost limit above.
- How a page lost to a race goes back, within the rule above.

## Out of scope

- Showing paused roots in darksearch's admin.
- Making kubo's timeout longer, or fetching sharded directories another way.
- Configurable thresholds.
