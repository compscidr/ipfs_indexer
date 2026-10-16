# Retry gateway errors and pause when the gateway keeps failing (#284): design

**Status:** agreed · **Date:** 2026-10-16

## Goal

On 2026-10-16, from 07:40 to 13:10 UTC, kubo answered nearly every fetch with 504 Gateway Timeout. Indexing fell from about 4,500 pages/min to about 100.

- **Pages were lost.** `retrieve_content` drops a page on any fetch error, so about 20k pages left the queue without being indexed. They come back only if a page crawled later links to them.
- **Every worker kept trying.** 30 workers each waited out kubo's 30 s timeout, about 3,400 failed requests an hour, with two log warnings for each one.

**Success:**
1. A gateway outage loses no pages. A page that fails with a gateway error is retried later, up to 3 attempts in all.
2. While the gateway keeps failing, workers pause instead of sending requests, and resume on their own when it recovers.
3. `/status` shows the pause. The log gets a few lines per pause instead of two per failed fetch.
4. Normal crawling is unchanged. Pages that are missing (404 and other 4xx), skipped content types and oversized pages are still dropped at once.

## Decisions (agreed)

- **One pause for the whole gateway (approach A).** Today the queue is effectively one root: Wikipedia is all but 42 of 2.9M queued pages. A pause per root needs a `root` column and index on the queue, and pays off only with several large roots. That is #286 (after #213).
- **A failed page goes to the end of the queue.** The queue is first in, first out, so its retry comes after everything already queued, hours or days later. That spacing is wanted.
- **Thresholds are constants,** not settings.

## 1. Retrying a page (`src/store.rs`, `src/index_queue.rs`)

- **Schema:** `queue` gains `attempts INTEGER NOT NULL DEFAULT 0`. Opening an index whose `queue` lacks the column adds it with `ALTER TABLE`. That rewrites no rows, so it's instant at 2.9M, and existing rows read 0.
- **`queue_pop`** returns the item and its attempts. Order (oldest rowid first) and the `roots` counts are unchanged.
- **`queue_retry(item, attempts)`** inserts the item at the end of the queue with that attempts value, `INSERT OR IGNORE`. If a link queued the page again meanwhile, the row already there wins. It updates `roots` and the queued counter as `queue_add` does.
- **A fetch result says why it failed.** `retrieve_content` tells a gateway error (`is_gateway_error`: 5xx, 429, no status, so timeouts and connection errors) apart from every other outcome.
- **`process_next`**, on a gateway error:
  - with attempts + 1 below 3, it calls `queue_retry(item, attempts + 1)` and logs at debug;
  - otherwise it warns that the page was dropped after 3 attempts, with the error, and drops it.
- Other failures are dropped as now. A crash mid-fetch still loses at most the page each worker holds. Blocked pages are still skipped before fetching.

## 2. The pause (`Breaker`, `src/index_queue.rs`)

One `Breaker` behind a mutex, shared by all workers.

- **Record:** the outcomes of the last 20 fetches. A gateway error is a failure. Anything else is a success, a 404 included, since the gateway answered.
- **Closed** (normal): when 16 or more of the last 20 are failures, it **opens** for the current pause length, which starts at 1 minute. A warning is logged: the count, the pause length and the last error.
- **Before popping,** a worker asks the breaker for a permit:
  - **Go:** closed; fetch as now.
  - **Wait:** open and the pause hasn't ended, or another worker holds the test. The worker pops nothing and sleeps 200 ms, as an idle worker does.
  - **Test:** open, the pause has ended and no test is out. This worker fetches one page.
- **The test's result:**
  - **Success:** close the breaker, clear the record, reset the pause length to 1 minute, and log at info that crawling resumes.
  - **Failure:** reopen with the pause length doubled, capped at 15 minutes.
  - **No fetch happened** (queue empty, or the page is blocked): hand the test back, so the next worker to ask gets it.
  - The test page itself is retried or dropped like any other page.
- **Fetches in flight when it opens** finish as normal and retry their pages. Their outcomes don't change an open breaker; only the test's result does.
- **Tests** build the breaker with millisecond durations. Production uses 20, 16, 1 min and 15 min.

## 3. Status and logs

- `gateway_health()` (shown in `/status` and on the admin page):
  - open: `paused 4m: 18 of the last 20 fetches failed; last: <error>`;
  - test out: `testing after 18 of the last 20 fetches failed; last: <error>`;
  - otherwise unchanged: `ok 3s ago`, `error 3s ago: …`, `no fetches yet`.
- Logs:
  - a warning when a pause starts and an info line when crawling resumes;
  - a debug line per page retried;
  - a warning per page dropped after 3 attempts.

## Errors

No new error paths. If `queue_retry` fails on a store error, the page is lost, as today. The error is logged and `note_error` is called, as other queue writes do.

## Testing

- **Store:**
  - a `queue` table without `attempts` (as written by v0.4.4) gains the column on open, and its rows pop with attempts 0;
  - `queue_pop` returns attempts;
  - `queue_retry` appends after existing items, bumps `roots` and the queued count, and leaves an already-queued copy (and its attempts) alone.
- **Crawl,** against a local test HTTP server, as existing fetch tests do:
  - a 504 retries the page with attempts 1;
  - a page at attempts 2 that fails again is dropped and not requeued;
  - a 404 is dropped at once;
  - a retried page that then fetches fine is indexed.
- **Breaker:**
  - 15 failures in 20 stays closed, and 16 opens;
  - open gives every caller Wait until the pause ends;
  - then exactly one Test, with Wait for the rest;
  - a test success closes and resets the length;
  - a test failure doubles it, capped at the maximum;
  - a handed-back test goes to the next caller;
  - outcomes recorded while open don't change it.
- **End to end:** with the server returning 504, workers stop popping once the breaker opens, so the queue length holds. When the server recovers, the test succeeds, crawling resumes and the retried pages are indexed.
- **`/status`** shows the paused and testing texts.

## Release

v0.4.5. The column is added in place, and the API and segment format are unchanged, so darksearch needs nothing.

## Out of scope

- A pause per root (#286).
- An alert when indexing stalls (#285).
- Configurable thresholds.
