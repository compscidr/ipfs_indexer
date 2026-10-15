# MaxScore for OR queries and pairs of very frequent words (#271): design

**Status:** draft · **Date:** 2026-10-15

## Goal

Make OR queries and pairs of very frequent words fast. #248's block-max top-K search handles single words and ANDs with a rare word, but not these. On 10M synthetic pages, top 100:

| Query | Time (v0.4.3) |
|---|---|
| `common` | 14 ms |
| `common half` | 20 ms |
| `half OR tenth` | 214 ms |
| `w3 w5` (two very frequent words) | 366 ms |

Profiling at 1M pages shows why. Nearly every candidate is fully scored, because block bounds rarely fall below θ:

| Query | Candidates scored | Skipped | θ | Best score per word | Ceiling (2.2 × idf) |
|---|---|---|---|---|---|
| `half OR tenth` | 443k | 0.1% | 5.70 | half 1.45, tenth 4.80 | half 1.525, tenth 5.066 |
| `w3 w5` | 568k | 0.3% | 0.882 | w3 0.351, w5 0.600 | w3 0.375, w5 0.643 |

- In `half OR tenth`, no page reaches θ on `half` alone (1.525 < 5.70). Every candidate that comes only from `half` is wasted.
- In `w3 w5`, each word alone is well below θ, and only the postings whose own weight is high can make it. A per-page check passes 10.1% of `w5`'s postings and 2.9% of `tenth`'s.

**Success:**
1. **Same results.** For any query and K, the pages and their order equal the exhaustive search's, as in #248.
2. **`half OR tenth`** top 100 in under 50 ms at 10M pages.
3. **`w3 w5`** at least 3× faster than v0.4.3 at 10M. It stays linear in the index size (about 70–120 ms at 10M is expected). Score-ordered postings that would fix this are #282.
4. **No regressions.** The other #248 queries are no more than 10% slower.

## Decisions (agreed)

- **Approach 1:** MaxScore plus a per-page check, inside the existing `walk` in `src/search.rs`.
- **No format change.** Segments stay v2. Nothing is rewritten.
- **`w3 w5` stays linear** (the user's option A). Score ordering is filed as #282.
- #248's randomized same-results test stays the guard.

## Ceilings

- A term's **ceiling** is c(t) = idf(t) · (k1 + 1). BM25's weight approaches it as count grows and never reaches it, for any length. It is computed at query time from the same idf the scorer uses.
- A group's ceiling is the sum of its terms' ceilings, over the terms that have a cursor in this segment.
- A term that appears in two groups is counted in both. That only loosens the bound; the score still counts it once, as now.

## The walk, per segment

The walk keeps its shape: cursors only move forward, the lead group supplies candidates, the other groups are seeked to them (leapfrog), and the region skip uses block bounds. What changes is which lead terms supply candidates, and how much work a candidate gets before it's dropped.

1. **Lead group:** the group with the smallest df in this segment, as now. **R** is the sum of the other groups' ceilings.
2. **Essential terms.** Sort the lead group's terms by ceiling, ascending. The longest prefix whose ceilings sum, plus R, is below θ is **non-essential**: no page can make the top K from those terms alone. The rest are **essential**.
   - Candidates come only from the essential terms' cursors.
   - The split is recomputed whenever θ rises (the prefix only grows).
   - If every lead term becomes non-essential, no remaining page in the segment can make the top K, and the walk of that segment ends.
   - A lead group of one term has nothing to split: the term is essential until its ceiling plus R is below θ, and then the walk of the segment ends.
3. **Region bound.** Before decoding, the bound for a region is: the essential cursors' block bounds + the non-essential terms' ceilings + the other groups' block bounds. If that is below θ, skip past the region's end as now. (The region's end is the smallest last id among the essential cursors' and the other groups' current blocks.)
4. **Per candidate,** from the essential cursors, in this order, stopping as soon as the page is dropped:
   1. **Dead:** skip it if it's in the dead set.
   2. **Count-only bound:** the sum of the count-only bounds of the essential cursors on the page + the non-essential ceilings + R. Skip if below θ. This needs no length lookup.
      - A cursor's count-only bound for count n is weight(n, l), where l is the length of the first impact in its current block whose count is at least n.
      - That is an upper bound: the posting is at or under some impact (c ≥ n, l' ≤ its length). Impacts are sorted with counts and lengths both ascending, so the first impact with c ≥ n has a length at most l'.
      - For a v1 block, this is the segment's shortest length, as v1's block bound is.
   3. **Exact essential weights:** look up the page's length and compute the essential terms' exact weights. Bound = those + non-essential ceilings + R. Skip if below θ.
   4. **Other groups, one at a time:** seek the group to the candidate. If it lands past it, that id becomes the next target (leapfrog), as now. Otherwise, replace the group's ceiling in the bound with its exact weight on the page, and skip if the bound is now below θ.
   5. **Non-essential lead terms:** seek them to the candidate, add their weights, and offer the score to the heap.
5. **Rounding and ties:** skip only when bound × (1 + 1e-9) < θ, as in #248, so ties are scored.

Unchanged: the buffers are scored in full into the same heap; totals; the heap and its order; the dead set; snapshots.

**Why results don't change:** every skip compares an upper bound of the page's score with θ, and θ only rises. Ceilings bound any weight; block bounds and count-only bounds bound any posting under them; exact weights are exact. A page dropped by any check can't beat the K-th score, so the heap ends the same as an exhaustive search's.

## Errors

No new error paths. A cursor read or decode error ends the search with no results and is logged, as now. With #266, a damage error also marks the index for a check at the next start.

## Testing

- **Same results (the main guard):** #248's randomized test compares `search` with the exhaustive search across v1 and v2 segments, the write buffer and dead pages, for K from 1 to 1,000 with offsets. Its query mix gains:
  - ORs of a frequent and a rare term;
  - ANDs of two frequent terms;
  - mixed shapes like `(a OR b) c`;
  - pages built to tie exactly at θ (same counts and length), so ties are still scored.
- **Ceiling:** for random counts and lengths, including length 0 and very large counts, every term's weight is below its ceiling.
- **Count-only bound:** for random blocks (v1 and v2, including coarsened impacts), every posting's weight is at or below its count-only bound.
- **Essential split:** for given ceilings, R and θ, the non-essential prefix is the longest one below θ; with θ = −∞ every term is essential.
- **Pruning works** (counted by a test hook for full scorings and for candidates drawn from non-essential terms), on a skewed corpus:
  - `rare OR common` draws no candidates from `common` once θ is set;
  - an AND of two frequent terms fully scores fewer than a tenth of its matches.
- **The walk ends early:** once every lead term is non-essential, the segment's remaining blocks aren't decoded (the existing blocks-decoded counter).

## Measurement

Throwaway code, not committed; the numbers go in the PR. The same synthetic 1M and 10M corpus as #248, v0.4.3 against the branch. Queries: the five from #248 plus a pair of rare words, top 100.

## Release

v0.4.4. The format and the API are unchanged, so darksearch needs nothing.

## Out of scope

- Score-ordered postings for pairs of very frequent words (#282).
- Choosing the lead group by bound rather than df.
- Segment count and per-segment memory (#276, #269).
- Format changes of any kind.
