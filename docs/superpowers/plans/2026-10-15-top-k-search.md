# Top-K Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Historical note:** this is the plan as executed task by task. The final whole-branch review found the per-block (max count, shortest page) bound too loose to prune common words. Before v2 shipped, it was replaced with per-block **impacts**: up to 8 (count, page length) pairs per block, read through `Cursor::block_last()` / `block_bound()`, with `HEAD_MAX = 200`. The review also moved v1 upgrades off the startup path, onto the merge thread with a backoff. The spec (`docs/superpowers/specs/2026-10-15-top-k-search-design.md`) and `src/segment.rs` describe what shipped.

**Goal:** Make search time depend on the number of results shown (top K), not on the number of matching pages (#248).

**Architecture:**
- Segment format v2 adds each block's shortest page length, so a block's best possible BM25 score can be bounded.
- A cursor reads a term's postings block by block and can skip past blocks by reading only their headers.
- A new `search` module walks each segment document at a time: AND-ed groups of OR-ed terms, a shared top-K heap, and skipping any block region whose bound can't beat the K-th score. Totals are free for one word, exact up to 10,000 for several words, and estimated beyond that.
- v1 segments are still read, and the merge thread upgrades them to v2 one at a time.

**Tech Stack:** Rust 2021, std only for the new code (no new crates), actix-web 4 for the handler.

**Spec:** `docs/superpowers/specs/2026-10-15-top-k-search-design.md`

## Global Constraints

- CI runs `cargo fmt --all -- --check && cargo test`. Both must pass after every task.
- Three unused-import warnings in `src/index_queue.rs` (`MessageBody`, `dashmap::Map`, `Not`) predate this work. Leave them, and add no new warnings.
- No new crates (tests use a small xorshift generator, not `rand`).
- BM25 is unchanged: k1 = 1.2, b = 0.75, `idf = ln(1 + (pages − df + 0.5)/(df + 0.5))`. The one change: `df` is capped at `pages`, so idf stays positive.
- `df` = the sum of the term's dictionary `df` across live segments, plus its live buffered postings.
- A page's score sums the weights of the query's distinct terms it has, in query term order (the first appearance in the query). The reference search in tests sums in the same order.
- Results are ordered by score descending, then lower page id.
- `MAX_RANKED = 1000`, `EXACT_UP_TO = 10_000`, `SAMPLE = 1024`, read-ahead window 64 KiB. The default `limit` is 100.
- Estimated totals are rounded to two significant figures and shown as `about N`: `Results for {q} (about {n} total): `.
- Segments lock order (segments.rs header): merging, writes, install, retiring, then dead, buffer, state, retired. Never take `dead` while holding `buffer` or `state`.
- Comments match the surrounding voice: short, lowercase-start line comments, and `/** */` or `///` on public items. Put issue numbers in parentheses (#248).
- Version: `Cargo.toml` becomes `0.4.0` in Task 4 (the default `limit` and the totals text change).

## Review Focus

1. **The same term in two groups** (`rust rust`, `rust OR ipfs rust`): it is scored once, the bound is just looser, and the results equal the exhaustive search. Pinned in Task 3: `a_term_in_two_groups_is_scored_once`.
2. **A garbled block header** (`n` = 0, last id below the previous block's): the cursor returns an error and never panics or loops. Pinned in Task 1: `a_garbled_block_header_is_an_error_not_a_panic`.
3. **A merge or erasure while a search holds its view:** the view's segment files stay readable until it's dropped. Pinned in Task 2: `a_view_keeps_reading_its_segments_across_a_merge`.
4. **`df` above the live page count** (dead postings not yet merged out): idf stays positive, so bounds stay valid. Pinned in Task 3: `idf_stays_positive_when_df_exceeds_the_page_count`.
5. **An estimate that rounds below the results shown**, e.g. a sample that hits no matches: the total is never less than the number of results returned. Pinned in Task 3: `a_total_is_never_below_the_results_found`.

---

## File map

| File | Changes |
|---|---|
| `src/segment.rs` | `VERSION` 2 (pub(crate)), the block header's shortest length, `create_version`, a v1 reader, `version()`, `df()`, `cursor()`, `Cursor`, the test-only `DECODED` counter |
| `src/segments.rs` | `View` and `Segments::view`; v1 upgrade in `pick_merge`; test-only write version; `postings` becomes `#[cfg(test)]` |
| `src/store.rs` | `Store::view`; `Store::postings` becomes `#[cfg(test)]` |
| `src/search.rs` (new) | `Query`, `Scorer`, `Total`, `MAX_RANKED`, `dfs`, `search`: the top-K walk and totals |
| `src/index_queue.rs` | `search` uses the `search` module and returns `(Total, Vec<IndexResult>)`; BM25 constants move out |
| `src/main.rs` | `mod search;`; handler default `limit` 100, `{}` total |
| `README.md`, `Cargo.toml` | docs, v0.4.0 |

---

### Task 1: Segment format v2 and cursors

**Files:**
- Modify: `src/segment.rs` (whole file; layout comment lines 1-14, constants 23-29, `SegmentWriter`, `Segment`, `postings_at`, tests)

**Interfaces:**
- Produces (all in `crate::segment`):
  - `pub(crate) const VERSION: u8 = 2;`
  - `SegmentWriter::create_version(path: &Path, version: u8) -> io::Result<SegmentWriter>` (pub(crate)); `create` calls it with `VERSION`.
  - `Segment::version(&self) -> u8`, `Segment::df(&self, term: &[u8]) -> io::Result<u64>` (0 if absent), `Segment::cursor(&self, term: &[u8]) -> io::Result<Option<Cursor<'_>>>`.
  - `Cursor<'a>` methods:
    - `df(&self) -> u64`
    - `done(&self) -> bool`
    - `block(&self) -> Option<(u64, u32, u32)>`: last id, max count, shortest length
    - `seek_block(&mut self, target: u64) -> io::Result<()>`
    - `current(&mut self) -> io::Result<Option<Posting>>`
    - `seek(&mut self, target: u64) -> io::Result<Option<Posting>>`
    - `next(&mut self) -> io::Result<Option<Posting>>`
  - `#[cfg(test)] pub(crate) static DECODED` (thread-local `Cell<usize>`): blocks decoded by cursors on this thread.

- [ ] **Step 1: Write the failing tests** (add to `segment.rs`'s `mod tests`)

```rust
    #[test]
    fn every_posting_is_within_its_blocks_bounds_in_v1_and_v2() {
        for version in [1u8, 2] {
            let path = temp_dir(&format!("segment-bounds-v{}", version)).join("1.seg");
            let docs: Vec<(u64, u32)> = (1..=1000).map(|id| (id, (id * 37 % 500 + 1) as u32)).collect();
            let postings: Vec<Posting> = (1..=1000u64)
                .filter(|id| id % 3 != 0)
                .map(|id| (id, (id * 7 % 9 + 1) as u32))
                .collect();
            let mut writer = SegmentWriter::create_version(&path, version).unwrap();
            for &(id, length) in &docs {
                writer.add_doc(id, length).unwrap();
            }
            writer.add_term(b"word", &postings).unwrap();
            writer.finish().unwrap();

            let segment = Segment::open(&path).unwrap();
            assert_eq!(segment.version(), version);
            assert_eq!(segment.df(b"word").unwrap(), postings.len() as u64);
            assert_eq!(segment.df(b"absent").unwrap(), 0);
            let mut cursor = segment.cursor(b"word").unwrap().unwrap();
            // per block: (last id, shortest length it claims, lengths of its postings)
            let mut blocks: Vec<(u64, u32, Vec<u32>)> = Vec::new();
            let mut seen = Vec::new();
            let mut at = cursor.current().unwrap();
            while let Some((id, count)) = at {
                let (last, max_count, shortest) = cursor.block().unwrap();
                let length = segment.length(id).unwrap();
                assert!(id <= last && count <= max_count && length >= shortest, "{} v{}", id, version);
                if blocks.last().map(|b| b.0) != Some(last) {
                    blocks.push((last, shortest, Vec::new()));
                }
                blocks.last_mut().unwrap().2.push(length);
                seen.push((id, count));
                at = cursor.next().unwrap();
            }
            assert!(cursor.done());
            assert_eq!(seen, postings);
            if version == 2 {
                // tight: the block's own shortest page, not the segment's
                for (_, shortest, lengths) in &blocks {
                    assert_eq!(*shortest, *lengths.iter().min().unwrap());
                }
            } else {
                assert!(blocks.iter().all(|b| b.1 == 1), "v1 uses the segment's shortest page");
            }
        }
    }

    #[test]
    fn a_cursor_seeks_forward_to_exact_ids_gaps_block_edges_and_the_end() {
        let path = temp_dir("segment-cursor").join("1.seg");
        let docs: Vec<(u64, u32)> = (1..=1000).map(|id| (id, 5)).collect();
        // 500 postings: blocks 2..=256, 258..=512, 514..=768, 770..=1000
        let even: Vec<Posting> = (1..=1000u64).filter(|id| id % 2 == 0).map(|id| (id, 1)).collect();
        write(&path, &docs, &[("even", even)]);
        let segment = Segment::open(&path).unwrap();
        assert!(segment.cursor(b"odd").unwrap().is_none());

        let mut c = segment.cursor(b"even").unwrap().unwrap();
        assert_eq!(c.df(), 500);
        assert_eq!(c.seek(10).unwrap(), Some((10, 1)), "exact");
        assert_eq!(c.seek(11).unwrap(), Some((12, 1)), "between postings");
        assert_eq!(c.seek(5).unwrap(), Some((12, 1)), "never backwards");
        assert_eq!(c.seek(256).unwrap(), Some((256, 1)), "last of the first block");
        assert_eq!(c.seek(257).unwrap(), Some((258, 1)), "first of the second");
        assert_eq!(c.next().unwrap(), Some((260, 1)));

        let mut skipping = segment.cursor(b"even").unwrap().unwrap();
        let before = DECODED.with(|d| d.get());
        skipping.seek_block(900).unwrap();
        assert_eq!(DECODED.with(|d| d.get()), before, "headers only");
        assert_eq!(skipping.block().unwrap().0, 1000);
        assert_eq!(skipping.seek(1001).unwrap(), None);
        assert!(skipping.done());
        assert_eq!(skipping.block(), None);
    }

    #[test]
    fn a_garbled_block_header_is_an_error_not_a_panic() {
        let path = temp_dir("segment-garbled").join("1.seg");
        write(&path, &[(1, 1)], &[("word", vec![(1, 1)])]);
        let mut bytes = std::fs::read(&path).unwrap();
        // header (7 bytes), docs (id 1, length 1: 2 bytes), then the first block's n
        assert_eq!(bytes[9], 1);
        bytes[9] = 0;
        std::fs::write(&path, &bytes).unwrap();
        // open_written skips the checksum, as for a file this process just wrote
        let segment = Segment::open_written(&path).unwrap();
        assert!(segment.cursor(b"word").is_err());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --bin ipfs_indexer segment::tests 2>&1 | tail -5`
Expected: compile errors (`create_version`, `version`, `df`, `cursor`, `DECODED` not found).

- [ ] **Step 3: Implement**

Update the layout comment (lines 1-14) to version 2. The postings line becomes:

```
 *   postings  per term, blocks of up to BLOCK postings: n, last id, body bytes, max count, and
 *             (v2) the shortest page among them: the most a block's pages can score, for #248's
 *             pruning; then the id deltas, then the counts. A v1 block has no shortest page.
```

Constants:

```rust
pub(crate) const VERSION: u8 = 2;
// a block header is at most five varints
const HEAD_MAX: u64 = 50;
// how much a cursor reads at once
const WINDOW: u64 = 64 * 1024;

// blocks decoded by cursors on this thread, so tests can see what pruning skipped
#[cfg(test)]
thread_local! {
    pub(crate) static DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
```

`SegmentWriter`: add these fields:

```rust
    version: u8,
    // the pages written so far and their lengths, for each block's shortest page (v2)
    ids: Vec<u64>,
    lengths: Vec<u32>,
```

Replace `create` with:

```rust
    /// Starts a new segment file; fails if `path` exists
    pub fn create(path: &Path) -> io::Result<SegmentWriter> {
        SegmentWriter::create_version(path, VERSION)
    }

    /// Starts a segment file of an older format version; for tests of reading old files
    pub(crate) fn create_version(path: &Path, version: u8) -> io::Result<SegmentWriter> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = SegmentWriter {
            out: BufWriter::new(file),
            pos: 0,
            hash: FNV_OFFSET,
            docs: 0,
            min_id: 0,
            max_id: 0,
            postings_off: None,
            dict: Vec::new(),
            sparse: Vec::new(),
            terms: 0,
            last_term: Vec::new(),
            version,
            ids: Vec::new(),
            lengths: Vec::new(),
        };
        writer.write(MAGIC)?;
        writer.write(&[version])?;
        Ok(writer)
    }
```

In `add_doc`, after `self.write(&buf)?;`:

```rust
        self.ids.push(id);
        self.lengths.push(length);
```

In `add_term`, after the max-count `put_varint`:

```rust
            if self.version >= 2 {
                // a posting whose page wasn't added gets 0: the loosest bound, never a wrong one
                let shortest = block
                    .iter()
                    .map(|&(id, _)| match self.ids.binary_search(&id) {
                        Ok(i) => self.lengths[i],
                        Err(_) => 0,
                    })
                    .min()
                    .unwrap();
                put_varint(&mut head, shortest as u64);
            }
```

`Segment`: add these fields:

```rust
    version: u8,
    // the shortest page: a v1 block's bound, having none of its own
    shortest: u32,
```

In `read`:
- The version check becomes `let version = header[MAGIC.len()]; if version != 1 && version != VERSION { … Unsupported … }`.
- Compute `let shortest = lengths.iter().copied().min().unwrap_or(0);`.
- Set both fields in the struct literal.

Add:

```rust
    /// The format version it was written in
    pub fn version(&self) -> u8 {
        self.version
    }

    /// How many pages a term is on here (dead ones included), or 0
    pub fn df(&self, term: &[u8]) -> io::Result<u64> {
        Ok(self.find(term)?.map_or(0, |(df, _)| df))
    }

    /// A cursor over a term's postings (#248), or none if it isn't in this segment
    pub fn cursor(&self, term: &[u8]) -> io::Result<Option<Cursor<'_>>> {
        match self.find(term)? {
            Some((df, offset)) => Cursor::new(self, df, offset).map(Some),
            None => Ok(None),
        }
    }
```

In `postings_at`:
- Read `HEAD_MAX` instead of `40`, and update the comment to "at most five varints".
- After `_max_count`, add:

```rust
            if self.version >= 2 {
                get_varint(&head, &mut pos)?;
            }
```

Add after `impl Segment` (before `pub struct Terms`):

```rust
/**
 * Reads one term's postings in one segment block by block (#248). Block headers can be read
 * and skipped without decoding the postings behind them, so a search can pass over blocks
 * whose pages can't make its results. Moves only forward.
 */
pub struct Cursor<'a> {
    segment: &'a Segment,
    df: u64,
    // postings in the blocks before the current one
    passed: u64,
    // where the next block's header starts
    next: u64,
    // the current block; none once every posting is passed
    block: Option<Block>,
    // read-ahead: `window` holds the file from `window_at`
    window: Vec<u8>,
    window_at: u64,
}

struct Block {
    n: usize,
    // the block before's last id: the base of this one's first delta
    base: u64,
    last: u64,
    max_count: u32,
    shortest: u32,
    body_at: u64,
    body_len: u64,
    // decoded on first use
    postings: Option<Vec<Posting>>,
    // the current posting within it
    i: usize,
}

impl<'a> Cursor<'a> {
    fn new(segment: &'a Segment, df: u64, offset: u64) -> io::Result<Cursor<'a>> {
        let mut cursor = Cursor {
            segment,
            df,
            passed: 0,
            next: offset,
            block: None,
            window: Vec::new(),
            window_at: 0,
        };
        cursor.load_block(0)?;
        Ok(cursor)
    }

    // up to `len` bytes at `offset`, within the postings section, through the read-ahead window
    fn bytes(&mut self, offset: u64, len: u64) -> io::Result<&[u8]> {
        let end = self.segment.dict_off;
        let len = len.min(end.saturating_sub(offset));
        let held = self.window_at + self.window.len() as u64;
        if offset < self.window_at || offset + len > held {
            let size = len.max(WINDOW).min(end.saturating_sub(offset));
            self.window = read_at(&self.segment.file, offset, size)?;
            self.window_at = offset;
        }
        let start = (offset - self.window_at) as usize;
        Ok(&self.window[start..start + len as usize])
    }

    // makes the block whose header starts at `next` current, or none once all are passed
    fn load_block(&mut self, base: u64) -> io::Result<()> {
        if self.passed >= self.df {
            self.block = None;
            return Ok(());
        }
        let (version, segment_shortest, at) =
            (self.segment.version, self.segment.shortest, self.next);
        let head = self.bytes(at, HEAD_MAX)?;
        let mut pos = 0;
        let n = get_varint(head, &mut pos)? as usize;
        let last = get_varint(head, &mut pos)?;
        let body_len = get_varint(head, &mut pos)?;
        let max_count = get_varint(head, &mut pos)? as u32;
        let shortest = match version {
            1 => segment_shortest,
            _ => get_varint(head, &mut pos)? as u32,
        };
        if n == 0 || n > BLOCK || last < base || (self.passed > 0 && last == base) {
            return Err(corrupt());
        }
        let body_at = at + pos as u64;
        self.next = body_at + body_len;
        self.block = Some(Block {
            n,
            base,
            last,
            max_count,
            shortest,
            body_at,
            body_len,
            postings: None,
            i: 0,
        });
        Ok(())
    }

    fn advance_block(&mut self) -> io::Result<()> {
        let Some(block) = self.block.take() else {
            return Ok(());
        };
        self.passed += block.n as u64;
        self.load_block(block.last)
    }

    // decodes the current block's postings, once
    fn decode(&mut self) -> io::Result<()> {
        let (at, len, n, base) = match &self.block {
            Some(b) if b.postings.is_none() => (b.body_at, b.body_len, b.n, b.base),
            _ => return Ok(()),
        };
        let body = self.bytes(at, len)?;
        if body.len() as u64 != len {
            return Err(corrupt());
        }
        let (mut pos, mut prev) = (0, base);
        let mut ids = Vec::with_capacity(n);
        for _ in 0..n {
            prev = prev
                .checked_add(get_varint(body, &mut pos)?)
                .ok_or_else(corrupt)?;
            ids.push(prev);
        }
        let mut postings = Vec::with_capacity(n);
        for id in ids {
            postings.push((id, get_varint(body, &mut pos)? as u32));
        }
        let block = self.block.as_mut().unwrap();
        if postings.last().map(|p| p.0) != Some(block.last) {
            return Err(corrupt());
        }
        block.postings = Some(postings);
        #[cfg(test)]
        DECODED.with(|d| d.set(d.get() + 1));
        Ok(())
    }

    /// How many pages the term is on in this segment
    pub fn df(&self) -> u64 {
        self.df
    }

    /// Whether every posting is passed
    pub fn done(&self) -> bool {
        self.block.is_none()
    }

    /// The current block's last id, highest count and shortest page; none once done
    pub fn block(&self) -> Option<(u64, u32, u32)> {
        self.block
            .as_ref()
            .map(|b| (b.last, b.max_count, b.shortest))
    }

    /// Moves to the first block whose last id is at least `target`, reading only headers
    pub fn seek_block(&mut self, target: u64) -> io::Result<()> {
        while self.block.as_ref().is_some_and(|b| b.last < target) {
            self.advance_block()?;
        }
        Ok(())
    }

    /// The current posting, decoding its block if it isn't yet; none once done
    pub fn current(&mut self) -> io::Result<Option<Posting>> {
        self.decode()?;
        Ok(self
            .block
            .as_ref()
            .map(|b| b.postings.as_ref().unwrap()[b.i]))
    }

    /// Moves to the first posting whose id is at least `target` (never back); none once done
    pub fn seek(&mut self, target: u64) -> io::Result<Option<Posting>> {
        self.seek_block(target)?;
        self.decode()?;
        if let Some(b) = self.block.as_mut() {
            let postings = b.postings.as_ref().unwrap();
            // the block's last id is at least target, so this stops inside it
            b.i += postings[b.i..].partition_point(|p| p.0 < target);
        }
        self.current()
    }

    /// Moves past the current posting; the one after it, none once done
    pub fn next(&mut self) -> io::Result<Option<Posting>> {
        self.decode()?;
        if let Some(b) = self.block.as_mut() {
            b.i += 1;
            if b.i == b.n {
                self.advance_block()?;
            }
        }
        self.current()
    }
}
```

The garbled-header test: `Cursor::new` loads the first header, sees `n == 0`, and returns `Err(corrupt())`. So `segment.cursor(b"word")` is `Err`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --bin ipfs_indexer segment 2>&1 | grep -E "^test |test result"`
Expected: all `segment::` and `segments::` tests pass, including the 3 new ones. Merges, flushes and erasures now write v2 and still read back.

- [ ] **Step 5: Run everything, then commit**

Run: `cargo fmt --all -- --check && cargo test 2>&1 | grep -E "^test result|FAILED|^warning"`
Expected: `test result: ok.`, and only the 3 known warnings.

```bash
git add src/segment.rs
git commit -m "Segment format v2: each block's shortest page; cursors that skip blocks by header (#248)"
```

---

### Task 2: Views and the v1 upgrade

**Files:**
- Modify: `src/segments.rs`: new `View` and `Segments::view` after `postings` (~line 448); `postings` becomes `#[cfg(test)]`; `pick_merge` (~line 965); `write_segment` (~line 743); a test-only `write_version` field and setter; tests.
- Modify: `src/store.rs`: `Store::view` next to `postings` (~line 461); `postings` becomes `#[cfg(test)]`.

**Interfaces:**
- Consumes: `segment::VERSION`, `Segment::version`, `SegmentWriter::create_version` (Task 1).
- Produces:
  - `pub struct View<'a> { pub segments: Vec<Arc<Segment>>, pub buffered: HashMap<String, Vec<(u64, u32, u32)>>, owner: &'a Segments }`
    - `segments` is the live segments, most pages first.
    - `buffered` maps each term asked for (including terms with none) to its live buffered postings as (id, count, length).
  - `View::is_dead(&self, id: u64) -> bool`.
  - `Segments::view(&self, terms: &[&str]) -> View<'_>`; `Store::view(&self, terms: &[&str]) -> crate::segments::View<'_>`.
  - `#[cfg(test)] Segments::set_write_version(&self, version: u8)`.

- [ ] **Step 1: Write the failing tests** (in `segments.rs`'s `mod tests`)

```rust
    #[test]
    fn a_view_has_live_segments_largest_first_and_live_buffered_postings() {
        let dir = temp_dir("segments-view");
        let segments = Segments::open(&dir).unwrap();
        flushed(&segments, &[(1, &[("alpha", 1)])]);
        for id in 2..=4 {
            segments.add(id, 2, &words(&[("alpha", 2)]));
        }
        segments.flush().unwrap();
        segments.add(5, 3, &words(&[("alpha", 3), ("beta", 1)]));
        segments.add(6, 3, &words(&[("alpha", 1)]));
        segments.set_dead(&[6]);

        let view = segments.view(&["alpha", "beta", "absent", "alpha"]);
        let docs: Vec<u64> = view.segments.iter().map(|s| s.info().docs).collect();
        assert_eq!(docs, vec![3, 1]);
        assert_eq!(view.buffered["alpha"], vec![(5, 3, 3)]);
        assert_eq!(view.buffered["beta"], vec![(5, 1, 3)]);
        assert!(view.buffered["absent"].is_empty());
        assert!(view.is_dead(6) && !view.is_dead(5));
    }

    #[test]
    fn a_view_keeps_reading_its_segments_across_a_merge() {
        let dir = temp_dir("segments-view-merge");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=10 {
            flushed(&segments, &[(id, &[("common", 1)])]);
        }
        let view = segments.view(&["common"]);
        maintained(&segments);
        assert_eq!(segments.segment_count(), 1, "merged meanwhile");
        let total: u64 = view
            .segments
            .iter()
            .map(|s| s.df(b"common").unwrap())
            .sum();
        assert_eq!(total, 10, "the retired files are still readable");
        let mut cursor = view.segments[0].cursor(b"common").unwrap().unwrap();
        assert!(cursor.current().unwrap().is_some());
    }

    #[test]
    fn a_v1_segment_is_upgraded_alone_and_its_file_erased() {
        let dir = temp_dir("segments-upgrade");
        let segments = Segments::open(&dir).unwrap();
        segments.set_write_version(1);
        for id in 1..=3 {
            segments.add(id, 4, &words(&[("alpha", 1), ("zzqxupgradeterm", 3)]));
        }
        segments.flush().unwrap();
        segments.set_write_version(crate::segment::VERSION);
        let old = segments.snapshot()[0].path().to_path_buf();
        assert_eq!(segments.snapshot()[0].version(), 1);
        let before = segments.postings("alpha").unwrap();

        maintained(&segments);
        let live = segments.snapshot();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].version(), crate::segment::VERSION);
        assert!(!old.exists(), "the v1 file was retired and erased");
        assert_eq!(segments.postings("alpha").unwrap(), before);
        maintained(&segments);
        assert_eq!(segments.snapshot()[0].path(), live[0].path(), "upgraded once");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --bin ipfs_indexer segments::tests::a_v 2>&1 | tail -5`
Expected: compile errors (`view`, `set_write_version` not found).

- [ ] **Step 3: Implement**

In `segments.rs`, add a field to `Segments` next to `hook`:

```rust
    // the format version new files are written in; tests write v1 files to upgrade
    #[cfg(test)]
    write_version: std::sync::atomic::AtomicU8,
```

Initialize it in `Segments::new` as `#[cfg(test)] write_version: std::sync::atomic::AtomicU8::new(crate::segment::VERSION),`. Then, in `write_segment`, replace `SegmentWriter::create(&path)` with:

```rust
        #[cfg(test)]
        let created = SegmentWriter::create_version(&path, self.write_version.load(Ordering::SeqCst));
        #[cfg(not(test))]
        let created = SegmentWriter::create(&path);
        let written = created.and_then(|mut writer| {
```

The closure body stays the same. In the test-only `impl Segments` block (with `segment_count`), add:

```rust
    pub(crate) fn set_write_version(&self, version: u8) {
        self.write_version.store(version, Ordering::SeqCst);
    }
```

Add after `pub fn postings` (which stays as it is until Task 4):

```rust
    /**
     * What a search reads (#248): the live segments, most pages first, and each term's postings
     * in the buffer and in one being flushed, as (id, count, page length), dead ids dropped.
     * Holding it keeps the segments' files from being erased.
     */
    pub fn view(&self, terms: &[&str]) -> View<'_> {
        let in_buffer = |buffer: &Buffer, term: &str, out: &mut Vec<(u64, u32, u32)>| {
            if let Some(list) = buffer.postings.get(term) {
                out.extend(list.iter().map(|&(id, count)| (id, count, buffer.lengths[&id])));
            }
        };
        // the buffer and state together, so a flush moving postings between them isn't missed
        let (state, mut buffered) = {
            let buffer = self.buffer.read().unwrap();
            let state = self.state.read().unwrap().clone();
            let mut buffered: HashMap<String, Vec<(u64, u32, u32)>> = HashMap::new();
            for &term in terms {
                let list = buffered.entry(term.to_string()).or_default();
                if list.is_empty() {
                    in_buffer(&buffer, term, list);
                }
            }
            (state, buffered)
        };
        if let Some(frozen) = &state.frozen {
            for (term, list) in buffered.iter_mut() {
                in_buffer(frozen, term, list);
            }
        }
        let dead = self.dead.read().unwrap();
        for list in buffered.values_mut() {
            list.retain(|(id, _, _)| !dead.ids.contains(id));
        }
        drop(dead);
        let mut segments: Vec<Arc<Segment>> = state.segments.iter().cloned().collect();
        segments.sort_by_key(|s| std::cmp::Reverse(s.info().docs));
        View {
            segments,
            buffered,
            owner: self,
        }
    }
```

Note: a term listed twice whose buffer list is empty is looked up twice; that's harmless. A term listed twice whose list is non-empty is filled only once, because of the `is_empty` check.

After the `Segments` struct, add:

```rust
/// A search's snapshot of the index (#248); see Segments::view
pub struct View<'a> {
    pub segments: Vec<Arc<Segment>>,
    pub buffered: HashMap<String, Vec<(u64, u32, u32)>>,
    owner: &'a Segments,
}

impl View<'_> {
    /// Whether a page was deleted after its postings were written
    pub fn is_dead(&self, id: u64) -> bool {
        self.owner.dead.read().unwrap().ids.contains(&id)
    }
}
```

In `pick_merge`, after the mostly-dead loop's block and before `let sizes`:

```rust
        // a file from before format v2 (#248), rewritten alone so its blocks get tight bounds;
        // merges never reach one of a gibibyte or more, so it'd stay v1 otherwise
        if let Some(old) = live.iter().find(|s| s.version() < crate::segment::VERSION) {
            log::info!(
                "Upgrading segment {} to format v{}",
                old.path().display(),
                crate::segment::VERSION
            );
            return Some(vec![old.clone()]);
        }
```

In `store.rs`, add next to `postings`:

```rust
    /// What a search reads (#248); see Segments::view
    pub fn view(&self, terms: &[&str]) -> crate::segments::View<'_> {
        self.segments.view(terms)
    }
```

Leave `Store::postings` and `Segments::postings` un-gated in this task: `index_queue.rs`'s `search` still calls them until Task 4, which adds `#[cfg(test)]` to both.

- [ ] **Step 4: Run the tests**

Run: `cargo test --bin ipfs_indexer segments 2>&1 | grep -E "FAILED|test result"`
Expected: `test result: ok.`

- [ ] **Step 5: Run everything, then commit**

Run: `cargo fmt --all -- --check && cargo test 2>&1 | grep -E "^test result|FAILED|^warning"`
Expected: ok, and only the 3 known warnings.

```bash
git add src/segments.rs src/store.rs
git commit -m "Segments: a search view, and v1 segments upgraded to v2 one at a time (#248)"
```

---

### Task 3: The top-K search

**Files:**
- Create: `src/search.rs`
- Modify: `src/main.rs`: add `mod search;` in the module list (after `mod index_result;`).

**Interfaces:**
- Consumes: `Segment::{cursor, df, length}`, `Cursor`, and `segment::Posting` (Task 1); `View` (Task 2).
- Produces (`crate::search`):
  - `pub const MAX_RANKED: usize = 1000;`
  - `pub struct Total { pub n: usize, pub about: bool }`: derives Clone, Copy, Debug and PartialEq; implements `PartialEq<usize>` (compares `n`) and `Display` (`"about N"` or `"N"`).
  - `pub struct Query { pub terms: Vec<String>, pub groups: Vec<Vec<usize>> }`, with `Query::new(groups: &[Vec<String>]) -> Query`.
  - `pub struct Scorer`, with `Scorer::new(pages: u64, average: f64, dfs: &[u64]) -> Scorer` and `Scorer::weight(&self, term: usize, count: u32, length: u32) -> f64`.
  - `pub fn dfs(view: &View, query: &Query) -> io::Result<Vec<u64>>`.
  - `pub fn search(view: &View, query: &Query, scorer: &Scorer, k: usize) -> io::Result<(Total, Vec<u64>)>`: page ids, best first, at most `k`.

- [ ] **Step 1: Write the module with its tests**

Create `src/search.rs`:

```rust
/*!
 * Top-K search over the segments (#248). Each segment is walked document at a time: a page must
 * have a term of every AND-ed group; block regions whose best possible score can't make the top
 * K are skipped by their headers alone (block-max). The buffer is scored in full. Totals are the
 * dictionary df for one word, an exact count when the smallest group is small, else an estimate.
 */
use crate::segment::{Cursor, Segment};
use crate::segments::View;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io;

// BM25's term-frequency saturation and length normalization, Lucene's and Tantivy's defaults
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// The most results a search ranks: offset + limit is clamped to this
pub const MAX_RANKED: usize = 1000;
// a total is counted exactly while the smallest group has at most this many postings
const EXACT_UP_TO: u64 = 10_000;
// else it's estimated from this many of the smallest group's pages in each segment
const SAMPLE: u64 = 1024;
// a region is skipped only if its bound is below the K-th score by more than rounding explains
const SLACK: f64 = 1e-9;

/// How many pages match: counted, or estimated (shown as "about")
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Total {
    pub n: usize,
    pub about: bool,
}

impl PartialEq<usize> for Total {
    fn eq(&self, other: &usize) -> bool {
        self.n == *other
    }
}

impl std::fmt::Display for Total {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.about {
            true => write!(f, "about {}", self.n),
            false => write!(f, "{}", self.n),
        }
    }
}

/// A query's distinct terms, in order of first appearance, and its AND-ed groups of OR-ed terms
pub struct Query {
    pub terms: Vec<String>,
    pub groups: Vec<Vec<usize>>,
}

impl Query {
    pub fn new(groups: &[Vec<String>]) -> Query {
        let mut terms: Vec<String> = Vec::new();
        let groups = groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|term| match terms.iter().position(|t| t == term) {
                        Some(i) => i,
                        None => {
                            terms.push(term.clone());
                            terms.len() - 1
                        }
                    })
                    .collect()
            })
            .collect();
        Query { terms, groups }
    }
}

/// BM25 for one query: each term's idf, and the average page length
pub struct Scorer {
    idf: Vec<f64>,
    average: f64,
}

impl Scorer {
    /// `dfs[t]`: how many pages term t is on; capped at `pages`, so no idf goes negative
    pub fn new(pages: u64, average: f64, dfs: &[u64]) -> Scorer {
        let n = pages as f64;
        let idf = dfs
            .iter()
            .map(|&df| {
                let df = df.min(pages) as f64;
                (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
            })
            .collect();
        Scorer {
            idf,
            average: average.max(1.0),
        }
    }

    /// A term's weight on a page: more with more occurrences, less on a longer page
    pub fn weight(&self, term: usize, count: u32, length: u32) -> f64 {
        let count = count as f64;
        let norm = K1 * (1.0 - B + B * length as f64 / self.average);
        self.idf[term] * count * (K1 + 1.0) / (count + norm)
    }
}

/// Each query term's df: its pages in every live segment (dead ones until merged out) and the
/// buffer
pub fn dfs(view: &View, query: &Query) -> io::Result<Vec<u64>> {
    query
        .terms
        .iter()
        .map(|term| {
            let mut df = view.buffered[term].len() as u64;
            for segment in &view.segments {
                df += segment.df(term.as_bytes())?;
            }
            Ok(df)
        })
        .collect()
}

/// The ids of the best `k` matching pages, best first, and how many pages match (#248)
pub fn search(
    view: &View,
    query: &Query,
    scorer: &Scorer,
    k: usize,
) -> io::Result<(Total, Vec<u64>)> {
    let mut top = TopK::new(k);
    if k > 0 && !query.groups.is_empty() {
        for (id, score) in buffered(view, query, scorer) {
            top.offer(score, id);
        }
        for segment in &view.segments {
            walk(segment, query, scorer, view, &mut top)?;
        }
    }
    let ranked = top.ranked();
    let mut total = total(view, query)?;
    // an estimate can come out below what was found
    total.n = total.n.max(ranked.len());
    Ok((total, ranked))
}

// a ranked page; Ord puts the worse one greater, so a BinaryHeap's top is the K-th best
#[derive(Clone, Copy, Debug, PartialEq)]
struct Ranked {
    score: f64,
    id: u64,
}

impl Eq for Ranked {}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        // lower scores, then higher ids (indexed later), are worse
        other
            .score
            .total_cmp(&self.score)
            .then(self.id.cmp(&other.id))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct TopK {
    k: usize,
    heap: BinaryHeap<Ranked>,
}

impl TopK {
    fn new(k: usize) -> TopK {
        TopK {
            k,
            heap: BinaryHeap::new(),
        }
    }

    fn offer(&mut self, score: f64, id: u64) {
        let ranked = Ranked { score, id };
        if self.heap.len() < self.k {
            self.heap.push(ranked);
        } else if self.heap.peek().is_some_and(|worst| ranked < *worst) {
            self.heap.pop();
            self.heap.push(ranked);
        }
    }

    // the score a page must reach to matter: none until K are held
    fn threshold(&self) -> f64 {
        match self.heap.len() < self.k {
            true => f64::NEG_INFINITY,
            false => self.heap.peek().map_or(f64::INFINITY, |worst| worst.score),
        }
    }

    fn ranked(self) -> Vec<u64> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|r| r.id)
            .collect()
    }
}

// one query term's postings in one segment
struct TermCursor<'a> {
    term: usize,
    cursor: Cursor<'a>,
}

// an OR group's cursors in one segment
struct Group<'a> {
    cursors: Vec<TermCursor<'a>>,
}

impl Group<'_> {
    fn df(&self) -> u64 {
        self.cursors.iter().map(|c| c.cursor.df()).sum()
    }

    fn done(&self) -> bool {
        self.cursors.iter().all(|c| c.cursor.done())
    }

    // every cursor to the block that could hold `target`, reading headers only
    fn seek_block(&mut self, target: u64) -> io::Result<()> {
        for c in self.cursors.iter_mut() {
            c.cursor.seek_block(target)?;
        }
        Ok(())
    }

    // the smallest last id among the current blocks: up to it, their bounds hold
    fn block_end(&self) -> Option<u64> {
        self.cursors
            .iter()
            .filter_map(|c| c.cursor.block().map(|b| b.0))
            .min()
    }

    // the most a page in the current blocks can score from this group's terms
    fn bound(&self, scorer: &Scorer) -> f64 {
        self.cursors
            .iter()
            .filter_map(|c| {
                let (_, max_count, shortest) = c.cursor.block()?;
                Some(scorer.weight(c.term, max_count, shortest))
            })
            .sum()
    }

    // every cursor to its first page at or after `target`; the first of those, none if done
    fn seek(&mut self, target: u64) -> io::Result<Option<u64>> {
        let mut first: Option<u64> = None;
        for c in self.cursors.iter_mut() {
            if let Some((id, _)) = c.cursor.seek(target)? {
                first = Some(first.map_or(id, |f| f.min(id)));
            }
        }
        Ok(first)
    }
}

// the query's groups as cursors over one segment; none if a group has no term in it
fn open_groups<'a>(segment: &'a Segment, query: &Query) -> io::Result<Option<Vec<Group<'a>>>> {
    let mut groups = Vec::new();
    for group in &query.groups {
        let mut cursors = Vec::new();
        for &term in group {
            if let Some(cursor) = segment.cursor(query.terms[term].as_bytes())? {
                cursors.push(TermCursor { term, cursor });
            }
        }
        if cursors.is_empty() {
            return Ok(None);
        }
        groups.push(Group { cursors });
    }
    Ok(Some(groups))
}

// the group to draw candidates from: the one with the fewest postings
fn lead(groups: &[Group]) -> usize {
    (0..groups.len()).min_by_key(|&g| groups[g].df()).unwrap()
}

// a matching page's score: the weights of the query's terms it has, in query order; every
// cursor is at or past `id`
fn score(
    groups: &mut [Group],
    query: &Query,
    scorer: &Scorer,
    id: u64,
    length: u32,
) -> io::Result<f64> {
    let mut score = 0.0;
    for t in 0..query.terms.len() {
        let mut count = None;
        'find: for group in groups.iter_mut() {
            for c in group.cursors.iter_mut().filter(|c| c.term == t) {
                if let Some((at, n)) = c.cursor.current()? {
                    if at == id {
                        count = Some(n);
                        break 'find;
                    }
                }
            }
        }
        if let Some(count) = count {
            score += scorer.weight(t, count, length);
        }
    }
    Ok(score)
}

// offers one segment's matches to `top`, skipping block regions that can't make it
fn walk(
    segment: &Segment,
    query: &Query,
    scorer: &Scorer,
    view: &View,
    top: &mut TopK,
) -> io::Result<()> {
    let Some(mut groups) = open_groups(segment, query)? else {
        return Ok(());
    };
    let lead = lead(&groups);
    let mut target = 0u64;
    loop {
        for group in groups.iter_mut() {
            group.seek_block(target)?;
            if group.done() {
                return Ok(());
            }
        }
        // ids from target to end are each in their cursors' current blocks, if anywhere
        let end = groups.iter().filter_map(Group::block_end).min().unwrap();
        let bound: f64 = groups.iter().map(|g| g.bound(scorer)).sum();
        if bound * (1.0 + SLACK) < top.threshold() {
            target = end + 1;
            continue;
        }
        let Some(candidate) = groups[lead].seek(target)? else {
            return Ok(());
        };
        if candidate > end {
            target = candidate;
            continue;
        }
        let mut next = None;
        for (g, group) in groups.iter_mut().enumerate() {
            if g == lead {
                continue;
            }
            match group.seek(candidate)? {
                None => return Ok(()),
                Some(id) if id > candidate => {
                    next = Some(id);
                    break;
                }
                Some(_) => {}
            }
        }
        if let Some(id) = next {
            target = id;
            continue;
        }
        if !view.is_dead(candidate) {
            let length = segment.length(candidate).unwrap_or(0);
            let score = score(&mut groups, query, scorer, candidate, length)?;
            top.offer(score, candidate);
        }
        target = candidate + 1;
    }
}

// each query term's buffered postings: page id -> (count, length)
fn buffered_lists(view: &View, query: &Query) -> Vec<HashMap<u64, (u32, u32)>> {
    query
        .terms
        .iter()
        .map(|term| {
            view.buffered[term]
                .iter()
                .map(|&(id, count, length)| (id, (count, length)))
                .collect()
        })
        .collect()
}

// the buffered pages matching every group
fn buffered_matches(lists: &[HashMap<u64, (u32, u32)>], query: &Query) -> Vec<u64> {
    let has = |group: &Vec<usize>, id: &u64| group.iter().any(|&t| lists[t].contains_key(id));
    let size = |group: &&Vec<usize>| -> usize { group.iter().map(|&t| lists[t].len()).sum() };
    let Some(smallest) = query.groups.iter().min_by_key(size) else {
        return Vec::new();
    };
    let candidates: HashSet<u64> = smallest
        .iter()
        .flat_map(|&t| lists[t].keys().copied())
        .collect();
    candidates
        .into_iter()
        .filter(|id| query.groups.iter().all(|group| has(group, id)))
        .collect()
}

// the buffered matches and their scores, in full: at most a minute of pages
fn buffered(view: &View, query: &Query, scorer: &Scorer) -> Vec<(u64, f64)> {
    let lists = buffered_lists(view, query);
    buffered_matches(&lists, query)
        .into_iter()
        .map(|id| {
            let mut score = 0.0;
            for (t, list) in lists.iter().enumerate() {
                if let Some(&(count, length)) = list.get(&id) {
                    score += scorer.weight(t, count, length);
                }
            }
            (id, score)
        })
        .collect()
}

// walks a segment's smallest group from the start, checking the others, for at most `limit`
// of its pages: (live matches, pages walked, the group's postings, whether it walked them all)
fn count(segment: &Segment, query: &Query, view: &View, limit: u64) -> io::Result<(u64, u64, u64, bool)> {
    let Some(mut groups) = open_groups(segment, query)? else {
        return Ok((0, 0, 0, true));
    };
    let lead = lead(&groups);
    let lead_df = groups[lead].df();
    let (mut matches, mut walked, mut target) = (0, 0, 0u64);
    while walked < limit {
        let Some(candidate) = groups[lead].seek(target)? else {
            return Ok((matches, walked, lead_df, true));
        };
        walked += 1;
        let mut all = true;
        for (g, group) in groups.iter_mut().enumerate() {
            if g != lead && group.seek(candidate)? != Some(candidate) {
                all = false;
                break;
            }
        }
        if all && !view.is_dead(candidate) {
            matches += 1;
        }
        target = candidate + 1;
    }
    let complete = groups[lead].seek(target)?.is_none();
    Ok((matches, walked, lead_df, complete))
}

// the smallest group's postings in one segment
fn lead_df(segment: &Segment, query: &Query) -> io::Result<u64> {
    let mut smallest = u64::MAX;
    for group in &query.groups {
        let mut df = 0;
        for &term in group {
            df += segment.df(query.terms[term].as_bytes())?;
        }
        smallest = smallest.min(df);
    }
    Ok(smallest)
}

// rounded to two significant figures, as estimates are shown
fn round2(n: f64) -> usize {
    if n < 100.0 {
        return n.round() as usize;
    }
    let scale = 10f64.powi(n.log10().floor() as i32 - 1);
    ((n / scale).round() * scale) as usize
}

// how many pages match: df for one word, an exact count for a small smallest group, else an
// estimate from a sample of each segment
fn total(view: &View, query: &Query) -> io::Result<Total> {
    if query.groups.is_empty() {
        return Ok(Total { n: 0, about: false });
    }
    if query.groups.len() == 1 && query.groups[0].len() == 1 {
        let df = dfs(view, query)?[query.groups[0][0]];
        return Ok(Total {
            n: df as usize,
            about: false,
        });
    }
    let lists = buffered_lists(view, query);
    let buffered = buffered_matches(&lists, query).len() as f64;
    let mut lead = 0;
    for segment in &view.segments {
        lead += lead_df(segment, query)?;
    }
    let limit = match lead <= EXACT_UP_TO {
        true => u64::MAX,
        false => SAMPLE,
    };
    let (mut n, mut about) = (buffered, false);
    for segment in &view.segments {
        let (matches, walked, lead_df, complete) = count(segment, query, view, limit)?;
        if complete {
            n += matches as f64;
        } else {
            n += matches as f64 / walked as f64 * lead_df as f64;
            about = true;
        }
    }
    Ok(match about {
        true => Total {
            n: round2(n),
            about: true,
        },
        false => Total {
            n: n as usize,
            about: false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::DECODED;
    use crate::segments::tests::words;
    use crate::segments::Segments;
    use crate::store::tests::temp_dir;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    const VOCAB: u64 = 30;

    fn groups(query: &[&[&str]]) -> Vec<Vec<String>> {
        query
            .iter()
            .map(|g| g.iter().map(|t| t.to_string()).collect())
            .collect()
    }

    fn terms(query: &Query) -> Vec<&str> {
        query.terms.iter().map(String::as_str).collect()
    }

    // every match scored and sorted: what search must equal
    fn exhaustive(view: &View, query: &Query, scorer: &Scorer, k: usize) -> Vec<u64> {
        let mut on: Vec<HashMap<u64, (u32, u32)>> = Vec::new();
        for term in &query.terms {
            let mut map: HashMap<u64, (u32, u32)> = view.buffered[term]
                .iter()
                .map(|&(id, count, length)| (id, (count, length)))
                .collect();
            for segment in &view.segments {
                for (id, count) in segment.postings(term.as_bytes()).unwrap() {
                    if !view.is_dead(id) {
                        map.insert(id, (count, segment.length(id).unwrap()));
                    }
                }
            }
            on.push(map);
        }
        let all: HashSet<u64> = on.iter().flat_map(|m| m.keys().copied()).collect();
        let mut ranked: Vec<Ranked> = all
            .into_iter()
            .filter(|id| {
                query
                    .groups
                    .iter()
                    .all(|g| g.iter().any(|&t| on[t].contains_key(id)))
            })
            .map(|id| {
                let mut score = 0.0;
                for (t, map) in on.iter().enumerate() {
                    if let Some(&(count, length)) = map.get(&id) {
                        score += scorer.weight(t, count, length);
                    }
                }
                Ranked { score, id }
            })
            .collect();
        ranked.sort();
        ranked.truncate(k);
        ranked.into_iter().map(|r| r.id).collect()
    }

    // three segments (the first v1), a buffer, and every 17th page dead; term i is on a page with
    // odds about 1 in i+1, so lists run from every page to a few
    fn corpus(name: &str) -> (Segments, u64) {
        let segments = Segments::open(&temp_dir(name)).unwrap();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut id = 0;
        let page = |segments: &Segments, rng: &mut Rng, id: u64| {
            let mut pairs: Vec<(String, u32)> = Vec::new();
            for i in 0..VOCAB {
                if rng.below(i + 1) == 0 {
                    pairs.push((format!("t{}", i), 1 + rng.below(5) as u32));
                }
            }
            let length = pairs.iter().map(|p| p.1).sum::<u32>() + rng.below(400) as u32;
            let borrowed: Vec<(&str, u32)> = pairs.iter().map(|(t, c)| (t.as_str(), *c)).collect();
            segments.add(id, length, &words(&borrowed));
        };
        for (pages, version) in [(700, 1u8), (500, 2), (300, 2)] {
            segments.set_write_version(version);
            for _ in 0..pages {
                id += 1;
                page(&segments, &mut rng, id);
            }
            segments.flush().unwrap();
        }
        segments.set_write_version(crate::segment::VERSION);
        for _ in 0..50 {
            id += 1;
            page(&segments, &mut rng, id);
        }
        let dead: Vec<u64> = (1..=id).filter(|i| i % 17 == 0).collect();
        segments.set_dead(&dead);
        (segments, id)
    }

    #[test]
    fn search_returns_exactly_what_an_exhaustive_search_ranks_first() {
        let (segments, pages) = corpus("search-equal");
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..300 {
            let query: Vec<Vec<String>> = (0..1 + rng.below(3))
                .map(|_| {
                    (0..1 + rng.below(2))
                        .map(|_| format!("t{}", rng.below(VOCAB)))
                        .collect()
                })
                .collect();
            let parsed = Query::new(&query);
            let view = segments.view(&terms(&parsed));
            let scorer = Scorer::new(pages, 120.0, &dfs(&view, &parsed).unwrap());
            for k in [1, 3, 10, 100, 1000] {
                let (_, found) = search(&view, &parsed, &scorer, k).unwrap();
                assert_eq!(
                    found,
                    exhaustive(&view, &parsed, &scorer, k),
                    "{:?} k={}",
                    query,
                    k
                );
            }
        }
    }

    #[test]
    fn a_term_in_two_groups_is_scored_once() {
        let (segments, pages) = corpus("search-twice");
        for query in [
            groups(&[&["t1"], &["t1"]]),
            groups(&[&["t1", "t2"], &["t1"]]),
            groups(&[&["t3", "t3"]]),
        ] {
            let parsed = Query::new(&query);
            let view = segments.view(&terms(&parsed));
            let scorer = Scorer::new(pages, 120.0, &dfs(&view, &parsed).unwrap());
            let (_, found) = search(&view, &parsed, &scorer, 20).unwrap();
            assert_eq!(found, exhaustive(&view, &parsed, &scorer, 20), "{:?}", query);
            assert!(!found.is_empty());
        }
    }

    #[test]
    fn a_top_10_decodes_only_the_blocks_it_needs() {
        let segments = Segments::open(&temp_dir("search-prune")).unwrap();
        // 6,400 pages (50 blocks); the first ten are short and say it often, the rest once in passing
        for id in 1..=6400u64 {
            let (length, count) = if id <= 10 { (10, 9) } else { (1000, 1) };
            segments.add(id, length, &words(&[("common", count)]));
        }
        segments.flush().unwrap();
        let query = Query::new(&groups(&[&["common"]]));
        let view = segments.view(&["common"]);
        let scorer = Scorer::new(6400, 990.0, &dfs(&view, &query).unwrap());
        let before = DECODED.with(|d| d.get());
        let (total, top) = search(&view, &query, &scorer, 10).unwrap();
        let decoded = DECODED.with(|d| d.get()) - before;
        assert_eq!(top, (1..=10).collect::<Vec<u64>>());
        assert_eq!(total, Total { n: 6400, about: false });
        assert!(decoded <= 2, "decoded {} of 50 blocks", decoded);
    }

    #[test]
    fn totals_are_exact_for_one_word_or_a_small_group_and_estimated_beyond() {
        let segments = Segments::open(&temp_dir("search-totals")).unwrap();
        for id in 1..=40_000u64 {
            let mut pairs = vec![("all", 1)];
            for (word, every) in [("half", 2), ("third", 3), ("rare", 5000)] {
                if id % every == 0 {
                    pairs.push((word, 1));
                }
            }
            segments.add(id, 10, &words(&pairs));
        }
        segments.flush().unwrap();
        let total = |query: &[&[&str]]| {
            let parsed = Query::new(&groups(query));
            let view = segments.view(&terms(&parsed));
            let scorer = Scorer::new(40_000, 10.0, &dfs(&view, &parsed).unwrap());
            search(&view, &parsed, &scorer, 10).unwrap().0
        };
        assert_eq!(total(&[&["all"]]), Total { n: 40_000, about: false });
        assert_eq!(total(&[&["all"], &["rare"]]), Total { n: 8, about: false });
        // the smallest group, third, has 13,333 pages: estimated; really 6,666
        let estimate = total(&[&["half"], &["third"]]);
        assert!(estimate.about, "{:?}", estimate);
        assert!((5_000..=8_333).contains(&estimate.n), "{:?}", estimate);
        assert_eq!(Total { n: 6_700, about: true }.to_string(), "about 6700");
        assert_eq!(Total { n: 8, about: false }.to_string(), "8");
        assert_eq!(round2(6_666.0), 6_700);
        assert_eq!(round2(40.4), 40);
    }

    #[test]
    fn a_total_is_never_below_the_results_found() {
        let segments = Segments::open(&temp_dir("search-total-floor")).unwrap();
        // both groups over 10,000 pages, overlapping only from 8,001: the sample of "early"'s
        // first 1,024 pages finds no match, so the estimate alone would be 0
        for id in 1..=20_000u64 {
            let mut pairs = Vec::new();
            if id <= 12_000 {
                pairs.push(("early", 1));
            }
            if id > 8_000 {
                pairs.push(("tail", 1));
            }
            segments.add(id, 10, &words(&pairs));
        }
        segments.flush().unwrap();
        let parsed = Query::new(&groups(&[&["early"], &["tail"]]));
        let view = segments.view(&terms(&parsed));
        let scorer = Scorer::new(20_000, 10.0, &dfs(&view, &parsed).unwrap());
        let (total, found) = search(&view, &parsed, &scorer, 1000).unwrap();
        assert_eq!(found.len(), 1000);
        assert!(total.about, "{:?}", total);
        assert!(total.n >= found.len(), "{:?} < {}", total, found.len());
    }

    #[test]
    fn idf_stays_positive_when_df_exceeds_the_page_count() {
        let scorer = Scorer::new(100, 10.0, &[150, 100, 1]);
        for t in 0..3 {
            assert!(scorer.weight(t, 1, 10) > 0.0, "term {}", t);
        }
        assert!(scorer.weight(2, 1, 10) > scorer.weight(1, 1, 10), "rarer weighs more");
    }
}
```

Add `mod search;` to `src/main.rs`'s module list, after `mod index_result;`.

Until Task 4 wires `search` in, its items are unused outside tests, which raises dead-code warnings in non-test builds. Put `#![allow(dead_code)]` at the top of `src/search.rs` with the comment `// wired into IndexQueue::search in the next commit (#248)`. Task 4 removes it.

- [ ] **Step 2: Run the new tests**

Run: `cargo test --bin ipfs_indexer search::tests 2>&1 | grep -E "^test |test result"`
Expected: 6 pass. If `search_returns_exactly_what_an_exhaustive_search_ranks_first` fails, the message names the query and K. Debug the walk against that query; don't loosen the test.

- [ ] **Step 3: Run everything, then commit**

Run: `cargo fmt --all -- --check && cargo test 2>&1 | grep -E "^test result|FAILED|^warning"`
Expected: ok, and only the 3 known warnings.

```bash
git add src/search.rs src/main.rs
git commit -m "Top-K search: block-max walk over segment cursors, exact or estimated totals (#248)"
```

---

### Task 4: Wire it in: IndexQueue, the handler, docs, v0.4.0

**Files:**
- Modify: `src/index_queue.rs`: `search` (~line 784), the BM25 constants (~line 113, now in `search.rs`), and call sites in tests that compare a total inside a tuple or a typed `Vec`.
- Modify: `src/main.rs`: the `search` handler (~line 143) and its tests.
- Modify: `src/store.rs`, `src/segments.rs`: `#[cfg(test)]` on `Store::postings` and `Segments::postings`.
- Modify: `src/search.rs`: remove `#![allow(dead_code)]`.
- Modify: `README.md` (the `/search` API row), and `Cargo.toml` (`0.4.0`, plus `Cargo.lock` via `cargo build`).

**Interfaces:**
- Consumes: `search::{Query, Scorer, Total, MAX_RANKED, dfs, search}` (Task 3) and `Store::view` (Task 2).
- Produces: `IndexQueue::search(&self, query: &str, offset: usize, limit: usize) -> (Total, Vec<IndexResult>)`.

- [ ] **Step 1: Write the failing tests**

In `index_queue.rs` tests:

```rust
    #[test]
    fn search_ranks_at_most_a_thousand_and_reports_the_total_past_them() {
        let queue = IndexQueue::new();
        for i in 0..1005 {
            index_text(&queue, &format!("bafyPage{}", i), "rust");
        }
        let (total, page) = queue.search("rust", 0, usize::MAX);
        assert_eq!((total.n, page.len()), (1005, 1000));
        let (total, page) = queue.search("rust", 995, 10);
        assert_eq!((total.n, page.len()), (1005, 5));
        let (total, page) = queue.search("rust", 1000, 10);
        assert_eq!((total.n, page.len()), (1005, 0), "past the ranked: the total, no results");
    }
```

`index_text(queue, name, text)` indexes `{GOOD}/{name}`, so any name works.

In `main.rs` tests, add:

```rust
    #[actix_web::test]
    async fn test_search_defaults_to_a_hundred_results() {
        let index_queue = web::Data::new(IndexQueue::new());
        let cid = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        for i in 0..101 {
            let item = format!("{}/p{}", cid, i);
            let counts = HashMap::from([("word".to_string(), 1)]);
            index_queue.index_for_test(
                &item,
                &crate::index_result::IndexResult::new(item.clone(), "t".into(), "e".into(), counts),
            );
        }
        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(search)).await;
        let req = test::TestRequest::get().uri("/search/word").to_request();
        let body = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(body.starts_with("Results for word (101 total): "), "{}", body);
        assert_eq!(body.matches("CID: ").count(), 100);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --bin ipfs_indexer search_ranks_at_most 2>&1 | tail -3` and `cargo test --bin ipfs_indexer test_search_defaults 2>&1 | tail -3`
Expected: a compile error (`total.n` on a `usize`), then a failure (101 results).

- [ ] **Step 3: Implement**

In `index_queue.rs`:
- Remove `BM25_K1` and `BM25_B` and their comment.
- Add `use crate::search::{self, Query, Scorer, Total, MAX_RANKED};`.
- Replace `search` with:

```rust
    /**
     * The best pages for a query, ranked by BM25 (#248): at most `offset + limit` of them, clamped
     * to MAX_RANKED, are ranked; `offset..offset + limit` are returned. The total is exact for one
     * word or a small smallest group, else an estimate.
     */
    pub fn search(&self, query: &str, offset: usize, limit: usize) -> (Total, Vec<IndexResult>) {
        let none = (Total { n: 0, about: false }, Vec::new());
        let query = Query::new(&parse_query(query));
        let terms: Vec<&str> = query.terms.iter().map(String::as_str).collect();
        let view = self.store().view(&terms);
        let dfs = match search::dfs(&view, &query) {
            Ok(dfs) => dfs,
            Err(err) => {
                warn!("Couldn't search for {:?}: {}", query.terms, err);
                return none;
            }
        };
        // a group none of whose terms are indexed matches nothing
        if query.groups.iter().any(|g| g.iter().all(|&t| dfs[t] == 0)) {
            return none;
        }
        let scorer = Scorer::new(
            self.store().page_count() as u64,
            self.store().average_length(),
            &dfs,
        );
        let k = offset.saturating_add(limit).min(MAX_RANKED);
        let (total, ranked) = match search::search(&view, &query, &scorer, k) {
            Ok(found) => found,
            Err(err) => {
                warn!("Couldn't search for {:?}: {}", query.terms, err);
                return none;
            }
        };
        drop(view);
        // resolve only the requested page of results to items, not every match
        let shown: Vec<u64> = ranked.into_iter().skip(offset).take(limit).collect();
        let found = match self.store().pages_by_id(&shown) {
            Ok(found) => found,
            Err(err) => {
                warn!("Couldn't look up search results: {}", err);
                return none;
            }
        };
        // a page deleted since its postings were read isn't found; one a denylist blocks isn't
        // served even before the purge erases it (#231)
        let results = shown
            .iter()
            .filter_map(|page| found.get(page))
            .filter(|(item, result)| {
                !self.blocked_quietly(item) && !self.blocked_quietly(&result.cid)
            })
            .map(|(_, result)| result.clone());
        (total, results.collect())
    }
```

Remove any imports this leaves unused (`HashSet`, perhaps); `cargo build` names them. Fix test call sites that no longer compile:
- A tuple compare such as `assert_eq!((total, page.len()), (3, 0))` becomes `(total.n, page.len())`.
- A `Vec<(usize, Vec<String>)>` built from `search` (the reopen test near `let (total, found) = queue.search(query, 0, 100)`) uses `total.n`.
- Plain `assert_eq!(total, 3)` and `.0, 3` comparisons keep working through `PartialEq<usize>`.

Put `#[cfg(test)]` on `Store::postings` (store.rs) and `Segments::postings` (segments.rs). Remove `#![allow(dead_code)]` and its comment from `search.rs`.

In `main.rs`'s handler:
- Change the comment above it to: `// ranked results; ?limit=n&offset=m pages through them (#209): 100 by default, the first 1,000 at most (#248)`.
- Change `number("limit", usize::MAX)` to `number("limit", 100)`.
- The `if total == 0` and the `format!("Results for {} ({} total): {:?}", query, total, results)` stay as written. `Total`'s `Display` prints `about N` for an estimate.

`README.md`, `/search` row: after "(all of them by default)", replace that parenthetical and the sentence about `limit`/`offset` with:

> `limit` (default 100) and `offset` page through the results; only the first 1,000 are ranked, so with `offset` at 1,000 or more the list is empty but the total is still given. The body starts `Results for {query} ({total} total): `. The total is exact for one word (it includes pages replaced by a re-crawl until their segments are merged), and for several words when the rarest group has at most 10,000 pages; beyond that it's estimated and shown as `about {total}` (#248).

`Cargo.toml`: `version = "0.4.0"`, then `cargo build` to update `Cargo.lock`.

- [ ] **Step 4: Run everything**

Run: `cargo fmt --all -- --check && cargo test 2>&1 | grep -E "^test result|FAILED|^warning"`
Expected: ok, and only the 3 known warnings.

- [ ] **Step 5: Commit**

```bash
git add src/index_queue.rs src/main.rs src/store.rs src/segments.rs src/search.rs README.md Cargo.toml Cargo.lock
git commit -m "Search ranks only the top offset+limit (at most 1,000); limit defaults to 100; estimated totals say about (#248); v0.4.0"
```

---

## After the tasks (controller, not a task)

**darksearch PR** (`compscidr/darksearch.xyz`, its own branch and PR; must merge before v0.4.0 deploys):
- In `src/search.php` `fetchResults`, the regex becomes `/^Results for .* \(((?:about )?)(\d+) total\): /sU`. Return `"about" => $m[1] !== ""` along with `"total" => (int) $m[2]`.
- `searchResults` shows `of about N` when `about` is set: `of <?php echo $about ? "about " : ""; ?><?php echo number_format($total); ?>`.
- api.php's JSON gains `"approximate": bool` next to `total`.
- Update `tests/fake_indexer.php` and the API tests with an "about" case.

**Measurement (throwaway, not committed):**
- Synthetic 1M and 10M pages with Zipf-like terms.
- Time these against `main`: a word on every page, two common words, a rare word plus a common one, and an OR of two common words.
- The target is under 50 ms for the top 100 at 10M pages.
- The numbers go in the PR.
