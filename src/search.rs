/*!
 * Top-K search over the segments (#248). Each segment is walked document at a time: a page must
 * have a term of every AND-ed group; block regions whose best possible score can't make the top
 * K are skipped by their headers alone (block-max). Lead terms that can't make it alone only
 * finish scores, and each candidate is dropped at its first bound below the K-th score
 * (MaxScore, #271). The buffer is scored in full. Totals are the dictionary df for one word, an
 * exact count when the smallest group is small, else an estimate.
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

// candidates the walk drew, page-length lookups it made and pages it fully scored on this
// thread, so tests can see what MaxScore skipped (#271)
#[cfg(test)]
thread_local! {
    static CANDIDATES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SCORED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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

/// A query's distinct terms, in order of first appearance, and its AND-ed groups of OR-ed terms;
/// a term repeated within a group is kept once
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
                let mut indices: Vec<usize> = Vec::new();
                for term in group {
                    let i = match terms.iter().position(|t| t == term) {
                        Some(i) => i,
                        None => {
                            terms.push(term.clone());
                            terms.len() - 1
                        }
                    };
                    if !indices.contains(&i) {
                        indices.push(i);
                    }
                }
                indices
            })
            .collect();
        Query { terms, groups }
    }
}

/// BM25 for one query: each term's idf, and the average page length
pub struct Scorer {
    // each term's df as given, for one word's total
    dfs: Vec<u64>,
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
            dfs: dfs.to_vec(),
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

    /// The most a term can weigh on any page: its weight approaches this as count grows (#271)
    pub fn ceiling(&self, term: usize) -> f64 {
        self.idf[term] * (K1 + 1.0)
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
    let mut total = total(view, query, scorer)?;
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

// an OR group's cursors in one segment. In the lead group, the first `passive` (the lowest
// ceilings) can't make the top K by themselves: they're only looked up to finish a score (#271)
struct Group<'a> {
    cursors: Vec<TermCursor<'a>>,
    passive: usize,
}

impl<'a> Group<'a> {
    fn df(&self) -> u64 {
        self.cursors.iter().map(|c| c.cursor.df()).sum()
    }

    // the cursors that supply candidates
    fn active(&self) -> &[TermCursor<'a>] {
        &self.cursors[self.passive..]
    }

    fn active_mut(&mut self) -> &mut [TermCursor<'a>] {
        &mut self.cursors[self.passive..]
    }

    fn done(&self) -> bool {
        self.active().iter().all(|c| c.cursor.done())
    }

    // every active cursor to the block that could hold `target`, reading headers only
    fn seek_block(&mut self, target: u64) -> io::Result<()> {
        for c in self.active_mut() {
            c.cursor.seek_block(target)?;
        }
        Ok(())
    }

    // the smallest last id among the active cursors' blocks: up to it, their bounds hold
    fn block_end(&self) -> Option<u64> {
        self.active()
            .iter()
            .filter_map(|c| c.cursor.block_last())
            .min()
    }

    // the most a page in the current blocks can score from this group's active terms
    fn bound(&self, scorer: &Scorer) -> f64 {
        self.active()
            .iter()
            .map(|c| {
                c.cursor
                    .block_bound(|count, length| scorer.weight(c.term, count, length))
            })
            .sum()
    }

    // the most this group's terms can score on any page (#271)
    fn ceiling(&self, scorer: &Scorer) -> f64 {
        self.cursors.iter().map(|c| scorer.ceiling(c.term)).sum()
    }

    // every active cursor to its first page at or after `target`; the first of those, none if done
    fn seek(&mut self, target: u64) -> io::Result<Option<u64>> {
        let mut first: Option<u64> = None;
        for c in self.active_mut() {
            if let Some((id, _)) = c.cursor.seek(target)? {
                first = Some(first.map_or(id, |f| f.min(id)));
            }
        }
        Ok(first)
    }

    // the passive cursors to `id`, to finish its score
    fn seek_passive(&mut self, id: u64) -> io::Result<()> {
        for c in self.cursors[..self.passive].iter_mut() {
            c.cursor.seek(id)?;
        }
        Ok(())
    }

    // how many of the active cursors sit on `id`: its postings in this group
    fn on(&mut self, id: u64) -> io::Result<u64> {
        let mut n = 0;
        for c in self.active_mut() {
            if c.cursor.current()?.is_some_and(|(at, _)| at == id) {
                n += 1;
            }
        }
        Ok(n)
    }

    // the active cursors on `id`: the most their counts allow, before its length is known (#271)
    fn count_bound(&mut self, scorer: &Scorer, id: u64) -> io::Result<f64> {
        let mut bound = 0.0;
        for c in self.active_mut() {
            if let Some((at, count)) = c.cursor.current()? {
                if at == id {
                    let term = c.term;
                    bound += c
                        .cursor
                        .count_bound(count, |n, l| scorer.weight(term, n, l));
                }
            }
        }
        Ok(bound)
    }

    // the active cursors on `id`: their weights on a page of `length`
    fn weight(&mut self, scorer: &Scorer, id: u64, length: u32) -> io::Result<f64> {
        let mut weight = 0.0;
        for c in self.active_mut() {
            if let Some((at, count)) = c.cursor.current()? {
                if at == id {
                    weight += scorer.weight(c.term, count, length);
                }
            }
        }
        Ok(weight)
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
        groups.push(Group {
            cursors,
            passive: 0,
        });
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

// a bound that can't reach `threshold`, allowing for rounding; a tie is kept
fn below(bound: f64, threshold: f64) -> bool {
    bound * (1.0 + SLACK) < threshold
}

/**
 * How many of the lead group's terms, lowest ceiling first, can't make the top K even with every
 * other group at its ceiling (`rest`) (#271): the longest prefix of `ceilings` (ascending) whose
 * sum plus `rest` is below `threshold`. Candidates come only from the terms after it.
 */
fn passive_terms(ceilings: &[f64], rest: f64, threshold: f64) -> usize {
    let mut sum = rest;
    for (n, &ceiling) in ceilings.iter().enumerate() {
        sum += ceiling;
        if !below(sum, threshold) {
            return n;
        }
    }
    ceilings.len()
}

/**
 * Offers one segment's matches to `top` (#248, #271). Regions of blocks whose bound can't make
 * it are skipped by their headers. In the lead group, terms whose ceilings can't make it even
 * with every other group at its ceiling go passive: candidates come only from the rest. Each
 * candidate is dropped at the first upper bound below the K-th score: its lead counts alone,
 * then each other group's counts in turn (after the leapfrog), then its exact weights once its
 * length is looked up; only what's left is fully scored. A region's end and bound are kept
 * until the walk passes it.
 */
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
    // the lead's terms by ceiling, lowest first, so the passive ones are a prefix
    groups[lead]
        .cursors
        .sort_by(|a, b| scorer.ceiling(a.term).total_cmp(&scorer.ceiling(b.term)));
    let ceilings: Vec<f64> = groups[lead]
        .cursors
        .iter()
        .map(|c| scorer.ceiling(c.term))
        .collect();
    // the other groups, and for each, the sum of the ceilings of those after it; summed rather
    // than subtracted, so rounding can't take a bound below a score
    let others: Vec<usize> = (0..groups.len()).filter(|&g| g != lead).collect();
    let mut beyond = vec![0.0; others.len()];
    for i in (0..others.len().saturating_sub(1)).rev() {
        beyond[i] = beyond[i + 1] + groups[others[i + 1]].ceiling(scorer);
    }
    let rest = others
        .first()
        .map_or(0.0, |&g| groups[g].ceiling(scorer) + beyond[0]);
    let mut target = 0u64;
    // the current region: its last id and bound, valid while target is at or before it
    let mut region: Option<(u64, f64)> = None;
    loop {
        let threshold = top.threshold();
        // the K-th score only rises, so terms only go passive
        let passive = passive_terms(&ceilings, rest, threshold).max(groups[lead].passive);
        if passive == ceilings.len() {
            // no page left here can make the top K
            return Ok(());
        }
        if passive != groups[lead].passive {
            groups[lead].passive = passive;
            region = None;
        }
        let passive_ceiling: f64 = ceilings[..passive].iter().sum();
        let (end, bound) = match region {
            Some((end, bound)) if target <= end => (end, bound),
            _ => {
                for group in groups.iter_mut() {
                    group.seek_block(target)?;
                    if group.done() {
                        return Ok(());
                    }
                }
                // ids from target to end are each in their cursors' current blocks, if anywhere
                let end = groups.iter().filter_map(Group::block_end).min().unwrap();
                let bound = passive_ceiling + groups.iter().map(|g| g.bound(scorer)).sum::<f64>();
                region = Some((end, bound));
                (end, bound)
            }
        };
        if below(bound, threshold) {
            // a garbled last id of u64::MAX would wrap back to 0 and loop
            let Some(after) = end.checked_add(1) else {
                return Ok(());
            };
            target = after;
            continue;
        }
        let Some(candidate) = groups[lead].seek(target)? else {
            return Ok(());
        };
        if candidate > end {
            target = candidate;
            continue;
        }
        #[cfg(test)]
        CANDIDATES.with(|c| c.set(c.get() + 1));
        let mut next = candidate.checked_add(1);
        'consider: {
            if view.is_dead(candidate) {
                break 'consider;
            }
            // its counts alone, at the shortest length their blocks allow
            let mut counts = groups[lead].count_bound(scorer, candidate)?;
            if below(counts + passive_ceiling + rest, threshold) {
                break 'consider;
            }
            for (i, &g) in others.iter().enumerate() {
                match groups[g].seek(candidate)? {
                    None => return Ok(()),
                    Some(id) if id > candidate => {
                        next = Some(id);
                        break 'consider;
                    }
                    Some(_) => {}
                }
                counts += groups[g].count_bound(scorer, candidate)?;
                if below(counts + passive_ceiling + beyond[i], threshold) {
                    break 'consider;
                }
            }
            let length = segment.length(candidate).unwrap_or(0);
            #[cfg(test)]
            LOOKUPS.with(|l| l.set(l.get() + 1));
            let mut exact = 0.0;
            for group in groups.iter_mut() {
                exact += group.weight(scorer, candidate, length)?;
            }
            if below(exact + passive_ceiling, threshold) {
                break 'consider;
            }
            groups[lead].seek_passive(candidate)?;
            #[cfg(test)]
            SCORED.with(|s| s.set(s.get() + 1));
            let score = score(&mut groups, query, scorer, candidate, length)?;
            top.offer(score, candidate);
        }
        match next {
            Some(id) => target = id,
            None => return Ok(()),
        }
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
// of its pages: (live matches, the group's postings those pages hold, all its postings, whether
// it walked them all). An OR group's terms can share pages, so its postings outnumber its pages
fn count(
    segment: &Segment,
    query: &Query,
    view: &View,
    limit: u64,
) -> io::Result<(u64, u64, u64, bool)> {
    let Some(mut groups) = open_groups(segment, query)? else {
        return Ok((0, 0, 0, true));
    };
    let lead = lead(&groups);
    let lead_df = groups[lead].df();
    let (mut matches, mut walked, mut consumed, mut target) = (0, 0, 0, 0u64);
    while walked < limit {
        let Some(candidate) = groups[lead].seek(target)? else {
            return Ok((matches, consumed, lead_df, true));
        };
        walked += 1;
        consumed += groups[lead].on(candidate)?;
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
        // the last id there can be: nothing follows it
        let Some(after) = candidate.checked_add(1) else {
            return Ok((matches, consumed, lead_df, true));
        };
        target = after;
    }
    let complete = groups[lead].seek(target)?.is_none();
    Ok((matches, consumed, lead_df, complete))
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

// how many pages match: df for one word (the scorer's), an exact count for a small smallest
// group, else an estimate from a sample of each segment
fn total(view: &View, query: &Query, scorer: &Scorer) -> io::Result<Total> {
    if query.groups.is_empty() {
        return Ok(Total { n: 0, about: false });
    }
    if query.groups.len() == 1 && query.groups[0].len() == 1 {
        return Ok(Total {
            n: scorer.dfs[query.groups[0][0]] as usize,
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
        let (matches, consumed, lead_df, complete) = count(segment, query, view, limit)?;
        if complete {
            n += matches as f64;
        } else if consumed > 0 {
            // matches per posting sampled, times the postings
            n += matches as f64 / consumed as f64 * lead_df as f64;
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

    #[test]
    fn no_weight_exceeds_its_terms_ceiling() {
        // the last df is above the page count, so it's capped
        let scorer = Scorer::new(1000, 120.0, &[1, 500, 1000, 5000]);
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for t in 0..4 {
            let mut pairs: Vec<(u32, u32)> = Vec::new();
            for count in [1, 2, 7, 1000, u32::MAX] {
                for length in [0, 1, 120, 100_000, u32::MAX] {
                    pairs.push((count, length));
                }
            }
            for _ in 0..1000 {
                pairs.push((1 + rng.below(1000) as u32, rng.below(10_000) as u32));
            }
            for (count, length) in pairs {
                assert!(
                    scorer.weight(t, count, length) <= scorer.ceiling(t),
                    "term {} count {} length {}",
                    t,
                    count,
                    length
                );
            }
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

    // the walk's candidates and full scorings while `f` runs
    fn counted(f: impl FnOnce()) -> (usize, usize) {
        let before = (CANDIDATES.with(|c| c.get()), SCORED.with(|s| s.get()));
        f();
        (
            CANDIDATES.with(|c| c.get()) - before.0,
            SCORED.with(|s| s.get()) - before.1,
        )
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
        // shapes MaxScore treats differently (#271): t0 is on every page, t29 on few
        let mut queries: Vec<Vec<Vec<String>>> = vec![
            groups(&[&["t0", "t25"]]),
            groups(&[&["t1", "t29"]]),
            groups(&[&["t0"], &["t1"]]),
            groups(&[&["t0", "t1"], &["t2"]]),
            groups(&[&["t2", "t20"], &["t0"]]),
            groups(&[&["t0", "t1", "t28"], &["t1"]]),
        ];
        for _ in 0..300 {
            queries.push(
                (0..1 + rng.below(3))
                    .map(|_| {
                        (0..1 + rng.below(2))
                            .map(|_| format!("t{}", rng.below(VOCAB)))
                            .collect()
                    })
                    .collect(),
            );
        }
        for query in queries {
            let parsed = Query::new(&query);
            let view = segments.view(&terms(&parsed));
            let scorer = Scorer::new(pages, 120.0, &dfs(&view, &parsed).unwrap());
            let every = exhaustive(&view, &parsed, &scorer, usize::MAX).len();
            for k in [1, 3, 10, 100, 1000] {
                let (total, found) = search(&view, &parsed, &scorer, k).unwrap();
                // one word's total is its df, which counts dead pages not yet merged out
                if !total.about && parsed.terms.len() > 1 {
                    assert_eq!(total, every, "total of {:?} k={}", query, k);
                }
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
            assert_eq!(
                found,
                exhaustive(&view, &parsed, &scorer, 20),
                "{:?}",
                query
            );
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
        assert_eq!(
            total,
            Total {
                n: 6400,
                about: false
            }
        );
        assert!(decoded <= 1, "decoded {} of 50 blocks", decoded);
    }

    #[test]
    fn a_block_whose_high_counts_are_on_long_pages_is_skipped() {
        let segments = Segments::open(&temp_dir("search-impacts")).unwrap();
        // the first ten say it often on short pages; after them, every block has both a short
        // page that says it once and a long one that says it often, but no page doing both
        for id in 1..=6400u64 {
            let (length, count) = match id {
                1..=10 => (10, 9),
                _ if id % 2 == 0 => (10, 1),
                _ => (5000, 9),
            };
            segments.add(id, length, &words(&[("common", count)]));
        }
        segments.flush().unwrap();
        let query = Query::new(&groups(&[&["common"]]));
        let view = segments.view(&["common"]);
        let scorer = Scorer::new(6400, 2500.0, &dfs(&view, &query).unwrap());
        let before = DECODED.with(|d| d.get());
        let (_, top) = search(&view, &query, &scorer, 10).unwrap();
        let decoded = DECODED.with(|d| d.get()) - before;
        assert_eq!(top, (1..=10).collect::<Vec<u64>>());
        assert_eq!(top, exhaustive(&view, &query, &scorer, 10));
        // a bound of (highest count, shortest page) would beat the tenth score in every block
        assert!(decoded <= 1, "decoded {} of 50 blocks", decoded);
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
        assert_eq!(
            total(&[&["all"]]),
            Total {
                n: 40_000,
                about: false
            }
        );
        assert_eq!(total(&[&["all"], &["rare"]]), Total { n: 8, about: false });
        // the smallest group, third, has 13,333 pages: estimated; really 6,666
        let estimate = total(&[&["half"], &["third"]]);
        assert!(estimate.about, "{:?}", estimate);
        assert!((5_000..=8_333).contains(&estimate.n), "{:?}", estimate);
        assert_eq!(
            Total {
                n: 6_700,
                about: true
            }
            .to_string(),
            "about 6700"
        );
        assert_eq!(Total { n: 8, about: false }.to_string(), "8");
        assert_eq!(round2(6_666.0), 6_700);
        assert_eq!(round2(40.4), 40);
    }

    #[test]
    fn an_or_group_is_estimated_by_its_pages_not_its_postings() {
        let segments = Segments::open(&temp_dir("search-total-or")).unwrap();
        for id in 1..=40_000u64 {
            let mut pairs = vec![("all", 1)];
            for (word, every) in [("half", 2), ("third", 3)] {
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
        // the lead is "half OR third": 33,333 postings on 26,667 pages, every one a match; counted
        // by postings it came out about 33,000
        let estimate = total(&[&["half", "third"], &["all"]]);
        assert!(estimate.about, "{:?}", estimate);
        assert!((24_000..=29_333).contains(&estimate.n), "{:?}", estimate);
        // "half OR half" is "half": it leads with 20,000 pages, not 40,000 postings
        let estimate = total(&[&["half", "half"], &["all"]]);
        assert!((18_000..=22_000).contains(&estimate.n), "{:?}", estimate);
        assert_eq!(total(&[&["half", "half"]]), total(&[&["half"]]));
        assert_eq!(
            total(&[&["half", "half"], &["third"]]),
            total(&[&["half"], &["third"]])
        );
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
    fn the_highest_page_id_ends_a_walk_instead_of_wrapping() {
        let segments = Segments::open(&temp_dir("search-max-id")).unwrap();
        for id in [1, u64::MAX] {
            segments.add(id, 5, &words(&[("alpha", 1), ("beta", 1)]));
        }
        segments.flush().unwrap();
        for query in [groups(&[&["alpha"]]), groups(&[&["alpha"], &["beta"]])] {
            let parsed = Query::new(&query);
            let view = segments.view(&terms(&parsed));
            let scorer = Scorer::new(2, 5.0, &dfs(&view, &parsed).unwrap());
            let (total, found) = search(&view, &parsed, &scorer, 10).unwrap();
            assert_eq!(found, vec![1, u64::MAX], "{:?}", query);
            assert_eq!(total, Total { n: 2, about: false });
        }
    }

    #[test]
    fn idf_stays_positive_when_df_exceeds_the_page_count() {
        let scorer = Scorer::new(100, 10.0, &[150, 100, 1]);
        for t in 0..3 {
            assert!(scorer.weight(t, 1, 10) > 0.0, "term {}", t);
        }
        assert!(
            scorer.weight(2, 1, 10) > scorer.weight(1, 1, 10),
            "rarer weighs more"
        );
    }

    #[test]
    fn the_passive_terms_are_the_longest_low_prefix_below_the_threshold() {
        let ceilings = [1.0, 2.0, 4.0];
        assert_eq!(passive_terms(&ceilings, 0.0, f64::NEG_INFINITY), 0);
        assert_eq!(passive_terms(&ceilings, 0.0, 1.0), 0, "a tie isn't below");
        assert_eq!(passive_terms(&ceilings, 0.0, 1.5), 1);
        assert_eq!(passive_terms(&ceilings, 0.0, 3.5), 2);
        assert_eq!(
            passive_terms(&ceilings, 0.5, 3.5),
            1,
            "other groups' ceilings count"
        );
        assert_eq!(passive_terms(&ceilings, 0.0, 7.5), 3);
        assert_eq!(passive_terms(&[], 0.0, 1.0), 0);
    }

    #[test]
    fn an_or_with_a_rare_term_stops_drawing_candidates_from_the_common_one() {
        let segments = Segments::open(&temp_dir("search-maxscore-or")).unwrap();
        // common once on every page; rare three times on every 64th
        for id in 1..=6400u64 {
            let mut pairs = vec![("common", 1)];
            if id % 64 == 0 {
                pairs.push(("rare", 3));
            }
            segments.add(id, 100, &words(&pairs));
        }
        segments.flush().unwrap();
        let query = Query::new(&groups(&[&["rare", "common"]]));
        let view = segments.view(&["rare", "common"]);
        // 64,000 pages in all, so common's idf isn't near 0
        let scorer = Scorer::new(64_000, 100.0, &dfs(&view, &query).unwrap());
        let mut found = Vec::new();
        let (candidates, _) = counted(|| found = search(&view, &query, &scorer, 10).unwrap().1);
        assert_eq!(found, (1..=10).map(|i| i * 64).collect::<Vec<u64>>());
        assert_eq!(found, exhaustive(&view, &query, &scorer, 10));
        // every page up to the tenth rare one (640), then only rare's 90 after it
        assert!(
            candidates <= 640 + 90,
            "{} candidates of 6,400 matches",
            candidates
        );
    }

    #[test]
    fn an_and_of_two_frequent_terms_fully_scores_only_pages_that_can_make_it() {
        let segments = Segments::open(&temp_dir("search-maxscore-and")).unwrap();
        // both words on every page. The first ten say both often on short pages. After them,
        // every block has a short page saying alpha often and beta once and one the other way
        // round, so every block's bound reaches the tenth score, though no page after the
        // tenth does
        for id in 1..=6400u64 {
            let (alpha, beta, length) = match id {
                1..=10 => (5, 5, 20),
                _ if id % 64 == 0 => (5, 1, 20),
                _ if id % 64 == 32 => (1, 5, 20),
                _ => (1, 1, 500 + (id % 500) as u32),
            };
            segments.add(id, length, &words(&[("alpha", alpha), ("beta", beta)]));
        }
        segments.flush().unwrap();
        let query = Query::new(&groups(&[&["alpha"], &["beta"]]));
        let view = segments.view(&["alpha", "beta"]);
        let scorer = Scorer::new(64_000, 500.0, &dfs(&view, &query).unwrap());
        let mut found = Vec::new();
        let (_, scored) = counted(|| found = search(&view, &query, &scorer, 10).unwrap().1);
        assert_eq!(found, (1..=10).collect::<Vec<u64>>());
        assert_eq!(found, exhaustive(&view, &query, &scorer, 10));
        assert!(scored < 640, "fully scored {} of 6,400 matches", scored);
    }

    #[test]
    fn a_page_another_group_lacks_costs_no_length_lookup() {
        let segments = Segments::open(&temp_dir("search-maxscore-lookups")).unwrap();
        // alpha on even pages, beta on odd ones: no page has both
        for id in 1..=2000u64 {
            let word = if id % 2 == 0 { "alpha" } else { "beta" };
            segments.add(id, 100, &words(&[(word, 1)]));
        }
        segments.flush().unwrap();
        let query = Query::new(&groups(&[&["alpha"], &["beta"]]));
        let view = segments.view(&["alpha", "beta"]);
        let scorer = Scorer::new(2000, 100.0, &dfs(&view, &query).unwrap());
        let mut top = TopK::new(10);
        let before = LOOKUPS.with(|l| l.get());
        walk(&view.segments[0], &query, &scorer, &view, &mut top).unwrap();
        assert_eq!(LOOKUPS.with(|l| l.get()) - before, 0);
        assert!(top.ranked().is_empty());
    }

    #[test]
    fn pages_tied_with_the_kth_score_are_still_ranked_by_id() {
        let segments = Segments::open(&temp_dir("search-maxscore-ties")).unwrap();
        // forty identical pages: the older twenty in a segment, the newer twenty in the buffer,
        // which is scored first and fills the heap with ties the segment's pages must displace
        for id in 1..=40u64 {
            segments.add(id, 50, &words(&[("alpha", 2), ("beta", 2)]));
            if id == 20 {
                segments.flush().unwrap();
            }
        }
        for query in [
            groups(&[&["alpha"], &["beta"]]),
            groups(&[&["alpha", "beta"]]),
        ] {
            let parsed = Query::new(&query);
            let view = segments.view(&terms(&parsed));
            let scorer = Scorer::new(400, 50.0, &dfs(&view, &parsed).unwrap());
            let (_, found) = search(&view, &parsed, &scorer, 10).unwrap();
            assert_eq!(found, (1..=10).collect::<Vec<u64>>(), "{:?}", query);
            assert_eq!(found, exhaustive(&view, &parsed, &scorer, 10));
        }
    }

    #[test]
    fn a_segment_whose_terms_cant_reach_the_kth_score_is_left_unread() {
        let segments = Segments::open(&temp_dir("search-maxscore-end")).unwrap();
        // a segment of pages with common alone, then ten in the buffer with rare as well
        for id in 1..=6400u64 {
            segments.add(id, 100, &words(&[("common", 1 + (id % 7) as u32)]));
        }
        segments.flush().unwrap();
        for id in 6401..=6410u64 {
            segments.add(id, 100, &words(&[("common", 1), ("rare", 3)]));
        }
        let query = Query::new(&groups(&[&["rare", "common"]]));
        let view = segments.view(&["rare", "common"]);
        let scorer = Scorer::new(64_000, 100.0, &dfs(&view, &query).unwrap());
        // the buffer first, as search does; then the segment alone (a total would read it)
        let mut top = TopK::new(10);
        for (id, score) in buffered(&view, &query, &scorer) {
            top.offer(score, id);
        }
        let before = DECODED.with(|d| d.get());
        let (candidates, scored) = counted(|| {
            walk(&view.segments[0], &query, &scorer, &view, &mut top).unwrap();
        });
        let decoded = DECODED.with(|d| d.get()) - before;
        // the segment lacks rare, and common's ceiling is below the tenth score
        assert_eq!((candidates, scored, decoded), (0, 0, 0));
        assert_eq!(top.ranked(), (6401..=6410).collect::<Vec<u64>>());
        let (_, found) = search(&view, &query, &scorer, 10).unwrap();
        assert_eq!(found, exhaustive(&view, &query, &scorer, 10));
    }
}
