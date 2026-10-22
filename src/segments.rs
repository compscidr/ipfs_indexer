/*!
 * The segment set (#247): the segment files named by MANIFEST, a write buffer of recent
 * postings, and the dead page ids that search filters out (#249). Two background threads keep
 * it: upkeep() flushes the buffer to a new segment, rewrites segments holding pages to erase, and
 * zeroes and removes retired files once no search still reads them; merge() merges segments.
 * A merge can take minutes, so neither waits for the other: each install swaps its inputs for
 * its output in the segment set as it is at that moment.
 *
 * Lock order: merging, writes, install, retiring, then dead, buffer, state, retired.
 * term_stats and refresh_stats take computing, then buffer and state (released), then stats. erase and
 * upgrades_after are only taken on their own. The wake signals are leaves: install raises
 * merge_due holding the install lock. The Store's connection lock comes before buffer (add), and
 * is never held across a call that waits on merging, writes, install or retiring.
 */
use crate::segment::{Posting, Segment, SegmentWriter};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, TryLockError};
use std::time::{Duration, Instant};

const FLUSH_POSTINGS: usize = 1_000_000;
const FLUSH_AGE: Duration = Duration::from_secs(60);
// merge this many segments of one size tier into one
const TIER_FANOUT: usize = 10;
// segments of this tier (1 GiB) and up aren't merged: it bounds the longest merge, and what a
// takedown's rewrite copies, at the cost of more segments per search at the largest scale
const MAX_MERGE_TIER: u32 = 3;
// rewrite a segment on its own once this share of its pages is dead
const DEAD_PERCENT: u64 = 20;
// /status and /keywords read a cached top STATS_TOP, recounted by the merge thread at most every STATS_TTL
const STATS_TOP: usize = 100;
pub const STATS_TTL: Duration = Duration::from_secs(60);
// a retired file whose erase fails this often is given up on; its ids stay dead
const ERASE_TRIES: u32 = 3;
// a running rewrite checks this often (pages, terms) whether its inputs were replaced
const CHECK_EVERY: usize = 4096;
// prune takes the dead lock for this many candidate ids at a time
const PRUNE_CHUNK: usize = 4096;
// after a failed upgrade to format v2 (#248), upgrades wait this long; merges go on meanwhile
const UPGRADE_BACKOFF: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
struct Buffer {
    postings: BTreeMap<String, Vec<Posting>>,
    lengths: BTreeMap<u64, u32>,
    // postings held
    count: usize,
    // the highest id ever added, even if since removed
    max_id: u64,
    // when the first page arrived
    since: Option<Instant>,
}

// what a search reads besides the live buffer, swapped as one so a flush is never half-seen
#[derive(Clone, Default)]
struct State {
    segments: Arc<Vec<Arc<Segment>>>,
    // a buffer being flushed: still searched until its segment is in the manifest
    frozen: Option<Arc<Buffer>>,
}

/**
 * The dead page ids, and which files hold them (#247). Each live or retired file's dead ids are
 * listed as they die, so the 20% rule reads a count and pruning only checks ids whose last file
 * just went, instead of scanning every dead id against every segment. Installs and retiring
 * change the set of files under this lock, so set_dead never misses one.
 */
#[derive(Default)]
struct Dead {
    ids: HashSet<u64>,
    // per live or retired segment file: the dead ids in it
    held: HashMap<PathBuf, Vec<u64>>,
    // dead ids that may be in no file, for the next prune to check
    unheld: Vec<u64>,
    // retired files whose erase was given up on: what they hold stays dead
    abandoned: Vec<Arc<Segment>>,
    // ids made dead while a segment was being written, from position log_start on, so its
    // install can list the ones it holds without scanning `ids`; kept only while a writer is
    // pinned (see Pin)
    log: Vec<u64>,
    log_start: u64,
    pins: Vec<u64>,
}

// a writer's place in Dead::log, taken before it reads the dead set; unpinned when dropped
struct Pin<'a> {
    segments: &'a Segments,
    at: u64,
}

impl Drop for Pin<'_> {
    fn drop(&mut self) {
        let mut dead = self
            .segments
            .dead
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(i) = dead.pins.iter().position(|&at| at == self.at) {
            dead.pins.swap_remove(i);
        }
        // what no pinned writer needs any more
        let end = dead.log_start + dead.log.len() as u64;
        let keep_from = dead.pins.iter().copied().min().unwrap_or(end);
        let trimmed = (keep_from - dead.log_start) as usize;
        dead.log.drain(..trimmed);
        dead.log_start = keep_from;
    }
}

struct Retired {
    segment: Arc<Segment>,
    // failed erases so far
    failures: u32,
}

// a flag a thread sleeps on until it's raised or a timeout passes
#[derive(Default)]
struct Signal {
    raised: Mutex<bool>,
    changed: Condvar,
}

impl Signal {
    fn raise(&self) {
        *lock(&self.raised) = true;
        self.changed.notify_all();
    }

    fn wait(&self, timeout: Duration) {
        let raised = lock(&self.raised);
        let (mut raised, _) = self
            .changed
            .wait_timeout_while(raised, timeout, |raised| !*raised)
            .unwrap_or_else(|e| e.into_inner());
        *raised = false;
    }
}

// a rewrite stopped because another install replaced one of its inputs
#[derive(Debug)]
struct Superseded;

impl std::fmt::Display for Superseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a rewrite's inputs were replaced")
    }
}

impl std::error::Error for Superseded {}

fn is_superseded(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|e| e.is::<Superseded>())
}

/// What a round of upkeep did
pub struct Maintained {
    /// dead ids no file holds any more, whose records can go; returned even if a step failed
    pub pruned: Vec<u64>,
    /// the first step that failed (the others still ran)
    pub error: Option<io::Error>,
}

// a term_stats result: distinct terms and the top STATS_TOP, from segment set `generation`
struct Stats {
    generation: u64,
    at: Instant,
    count: usize,
    top: Vec<(String, u32)>,
}

// sorted (term, value) streams for merge_terms
type TermSource<'a, V> = Box<dyn Iterator<Item = io::Result<(Vec<u8>, V)>> + 'a>;

pub struct Segments {
    // None in memory: the buffer holds everything and is never flushed
    dir: Option<PathBuf>,
    buffer: RwLock<Buffer>,
    state: RwLock<State>,
    dead: RwLock<Dead>,
    // dead ids whose segments must be rewritten now (#249); dropped once pruned
    erase: Mutex<HashSet<u64>>,
    next_gen: AtomicU64,
    flushed_through: AtomicU64,
    // replaced segments, erased once no search holds them
    retired: Mutex<Vec<Retired>>,
    // one merge cascade at a time
    merging: Mutex<()>,
    // flushes and erase rewrites, one at a time; never held by a merge
    writes: Mutex<()>,
    // an install's read-modify-write of the segment set and the manifest
    install: Mutex<()>,
    // erasing retired files, one pass at a time; apart from writes, so zeroing a big file
    // doesn't hold up flushes or erase rewrites
    retiring: Mutex<()>,
    // bumped by every install, so a running rewrite can cheaply tell when to check its inputs
    installs: AtomicU64,
    // wakes the upkeep thread (a full buffer, an erase) and the merge thread (an install)
    upkeep_due: Signal,
    merge_due: Signal,
    flush_postings: usize,
    flush_age: Duration,
    // bumped by every install: cached Stats from an older segment set are stale
    stats_gen: AtomicU64,
    // a full dictionary merge is too slow for every /status; held only to read or swap the value
    stats: Mutex<Option<Arc<Stats>>>,
    // one count at a time; only the merge thread and a first call with no value yet take it
    computing: Mutex<()>,
    // upgrades of older format files are skipped until then, after one failed
    upgrades_after: Mutex<Option<Instant>>,
    // files open() didn't read whole for their checksum, for verify_opened (v0.5.6)
    unverified: Mutex<Vec<PathBuf>>,
    #[cfg(test)]
    hook: Mutex<Option<HookFn>>,
    // the format version new files are written in; tests write v1 files to upgrade
    #[cfg(test)]
    write_version: std::sync::atomic::AtomicU8,
}

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

// test seams: where a hook set by set_hook runs
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Hook {
    StatsComputing,
    // in a merge cascade, before each merge or rewrite
    BeforeMerge,
    // in a rewrite, its output written, before it's installed
    BeforeInstall,
    // in a rewrite, each time it checks whether its inputs were replaced
    Checking,
    // an unused rewrite output, just before it's erased
    Discarding,
    // a retired file, just before it's erased
    Retiring,
    // in verify_opened, a file read whole, before its result is used
    Verified,
}

#[cfg(test)]
type HookFn = Arc<dyn Fn(&Segments, Hook) + Send + Sync>;

// a mutex guarding no data of its own (or data valid at every step), taken even if a panic
// poisoned it: a panicked upkeep round mustn't stop the next, or the flush at shutdown
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/**
 * A k-way merge of sorted term streams: calls `each` once per distinct term with its entries as
 * (source index, value). The heads are kept in a heap, so no term is copied per step.
 */
fn merge_terms<V>(
    mut sources: Vec<TermSource<'_, V>>,
    mut each: impl FnMut(&[u8], &[(usize, V)]) -> io::Result<()>,
) -> io::Result<()> {
    type Heads = BinaryHeap<Reverse<(Vec<u8>, usize)>>;
    fn advance<V>(
        sources: &mut [TermSource<'_, V>],
        i: usize,
        heads: &mut Heads,
        values: &mut [Option<V>],
    ) -> io::Result<()> {
        if let Some(entry) = sources[i].next() {
            let (term, value) = entry?;
            values[i] = Some(value);
            heads.push(Reverse((term, i)));
        }
        Ok(())
    }
    let mut values: Vec<Option<V>> = sources.iter().map(|_| None).collect();
    let mut heads = Heads::new();
    for i in 0..sources.len() {
        advance(&mut sources, i, &mut heads, &mut values)?;
    }
    let mut group = Vec::new();
    while let Some(Reverse((term, i))) = heads.pop() {
        group.push((i, values[i].take().unwrap()));
        advance(&mut sources, i, &mut heads, &mut values)?;
        while heads.peek().is_some_and(|Reverse((next, _))| *next == term) {
            let Reverse((_, j)) = heads.pop().unwrap();
            group.push((j, values[j].take().unwrap()));
            advance(&mut sources, j, &mut heads, &mut values)?;
        }
        each(&term, &group)?;
        group.clear();
    }
    Ok(())
}

/// Overwrites a file with zeros, syncs it, then removes it: unlinking alone leaves the text in
/// the freed blocks
pub fn erase_file(path: &Path) -> io::Result<()> {
    let len = fs::metadata(path)?.len();
    let mut file = OpenOptions::new().write(true).open(path)?;
    let zeros = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(zeros.len() as u64) as usize;
        file.write_all(&zeros[..n])?;
        left -= n as u64;
    }
    file.sync_all()?;
    drop(file);
    fs::remove_file(path)
}

// erases a segment written but never installed, so its page text doesn't outlive the attempt
fn discard(segment: Arc<Segment>) {
    let path = segment.path().to_path_buf();
    drop(segment);
    let _ = erase_file(&path);
}

// (next_gen, flushed_through, segment file names); None if there's no manifest yet
fn read_manifest(dir: &Path) -> io::Result<Option<(u64, u64, Vec<String>)>> {
    let text = match fs::read_to_string(dir.join("MANIFEST")) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "corrupt MANIFEST");
    let mut lines = text.lines();
    if lines.next() != Some("ipxman 1") {
        return Err(bad());
    }
    let mut value = |key: &str, radix: u32| -> io::Result<u64> {
        let line = lines.next().ok_or_else(bad)?;
        let rest = line.strip_prefix(key).ok_or_else(bad)?;
        u64::from_str_radix(rest.trim(), radix).map_err(|_| bad())
    };
    let next_gen = value("next_gen", 16)?;
    let flushed_through = value("flushed_through", 10)?;
    let names = lines
        .map(|line| line.split(' ').next().unwrap_or("").to_string())
        .collect();
    Ok(Some((next_gen, flushed_through, names)))
}

fn write_manifest(
    dir: &Path,
    next_gen: u64,
    flushed_through: u64,
    segments: &[Arc<Segment>],
) -> io::Result<()> {
    let mut text = format!(
        "ipxman 1\nnext_gen {:x}\nflushed_through {}\n",
        next_gen, flushed_through
    );
    for segment in segments {
        let info = segment.info();
        let name = segment.path().file_name().unwrap().to_string_lossy();
        text += &format!(
            "{} {} {} {} {}\n",
            name, info.min_id, info.max_id, info.docs, info.bytes
        );
    }
    let tmp = dir.join("MANIFEST.tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    // the rename is the commit point: once it succeeds the manifest names the new segments, so
    // errors past it must not be reported as a failed install (the caller would erase files
    // the manifest names). A failed directory sync only risks the rename across power loss.
    fs::rename(&tmp, dir.join("MANIFEST"))?;
    if let Err(err) = File::open(dir).and_then(|dir| dir.sync_all()) {
        log::warn!("Couldn't sync the segments directory: {}", err);
    }
    Ok(())
}

impl Segments {
    fn new(dir: Option<PathBuf>) -> Segments {
        Segments {
            dir,
            buffer: RwLock::new(Buffer::default()),
            state: RwLock::new(State::default()),
            dead: RwLock::new(Dead::default()),
            erase: Mutex::new(HashSet::new()),
            next_gen: AtomicU64::new(1),
            flushed_through: AtomicU64::new(0),
            retired: Mutex::new(Vec::new()),
            merging: Mutex::new(()),
            writes: Mutex::new(()),
            install: Mutex::new(()),
            retiring: Mutex::new(()),
            installs: AtomicU64::new(0),
            upkeep_due: Signal::default(),
            merge_due: Signal::default(),
            flush_postings: FLUSH_POSTINGS,
            flush_age: FLUSH_AGE,
            stats_gen: AtomicU64::new(0),
            stats: Mutex::new(None),
            computing: Mutex::new(()),
            upgrades_after: Mutex::new(None),
            unverified: Mutex::new(Vec::new()),
            #[cfg(test)]
            hook: Mutex::new(None),
            #[cfg(test)]
            write_version: std::sync::atomic::AtomicU8::new(crate::segment::VERSION),
        }
    }

    /**
     * Opens (or creates) the segments in `dir`; files the manifest doesn't name, left by a crash
     * mid-write, are erased. Each file's structure is checked, but not its checksum: reading
     * every file whole held the API up for 20 s or more on a big index (v0.5.4's timings), so
     * verify_opened does that once it's up (v0.5.6).
     */
    pub fn open(dir: &Path) -> io::Result<Segments> {
        Segments::open_with(dir, false)
    }

    /// Opens the segments in `dir` like open, each file read whole for its checksum first: for a
    /// start after damage was found (#266), so a bad file fails the open as corrupt
    pub fn open_verified(dir: &Path) -> io::Result<Segments> {
        Segments::open_with(dir, true)
    }

    fn open_with(dir: &Path, verify: bool) -> io::Result<Segments> {
        fs::create_dir_all(dir)?;
        let (next_gen, flushed_through, names) = read_manifest(dir)?.unwrap_or((1, 0, Vec::new()));
        // how long opening takes, for slow starts
        let start = Instant::now();
        let (mut live, mut bytes) = (Vec::new(), 0u64);
        for name in &names {
            let path = dir.join(name);
            bytes += fs::metadata(&path)?.len();
            let segment = if verify {
                Segment::open(&path)?
            } else {
                Segment::open_unverified(&path)?
            };
            live.push(Arc::new(segment));
        }
        log::info!(
            "Startup: {} {} segment files ({:.1} GB) in {:.1?}",
            if verify { "checked" } else { "opened" },
            live.len(),
            bytes as f64 / 1e9,
            start.elapsed()
        );
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name == "MANIFEST.tmp" || (name.ends_with(".seg") && !names.contains(&name)) {
                erase_file(&dir.join(&name))?;
            }
        }
        let segments = Segments::new(Some(dir.to_path_buf()));
        segments.next_gen.store(next_gen, Ordering::SeqCst);
        segments
            .flushed_through
            .store(flushed_through, Ordering::SeqCst);
        if !verify {
            *lock(&segments.unverified) = live.iter().map(|s| s.path().to_path_buf()).collect();
        }
        segments.state.write().unwrap().segments = Arc::new(live);
        Ok(segments)
    }

    /**
     * Checks the checksum of each file open() didn't (v0.5.6), once, skipping any a merge or
     * rewrite checked first (see rewrite); files written since are checked as they're written. For a thread of its own: it reads them whole, one at a time,
     * taking the state lock only to look each one up, so merges and searches go on. A file
     * merged away meanwhile is skipped, even mid-read: erasing it (#249) zeroes it, which would
     * look like damage. Returns the files and bytes checked, or the first file's error (a
     * mismatch is corrupt, see store::is_corrupt).
     */
    pub fn verify_opened(&self) -> io::Result<(usize, u64)> {
        let paths = std::mem::take(&mut *lock(&self.unverified));
        let (mut files, mut bytes) = (0, 0);
        for path in paths {
            let found = self
                .state
                .read()
                .unwrap()
                .segments
                .iter()
                .find(|s| s.path() == path)
                .cloned();
            let Some(segment) = found else {
                log::debug!("{} was merged away before it was checked", path.display());
                continue;
            };
            let verified = segment.verify();
            #[cfg(test)]
            self.hook(Hook::Verified);
            match verified {
                Ok(true) => {
                    files += 1;
                    bytes += segment.info().bytes;
                }
                // a merge or rewrite checked it first
                Ok(false) => {}
                Err(_) if !self.is_live(&segment) => {
                    log::debug!("{} was merged away while it was checked", path.display());
                }
                Err(err) => return Err(err),
            }
        }
        Ok((files, bytes))
    }

    // whether a segment is in the current set
    fn is_live(&self, segment: &Arc<Segment>) -> bool {
        self.state
            .read()
            .unwrap()
            .segments
            .iter()
            .any(|s| Arc::ptr_eq(s, segment))
    }

    pub fn in_memory() -> Segments {
        Segments::new(None)
    }

    /// The highest page id whose postings are in a segment file
    pub fn flushed_through(&self) -> u64 {
        self.flushed_through.load(Ordering::SeqCst)
    }

    /// Adds a page's postings to the write buffer; ids must ascend across calls
    pub fn add(&self, id: u64, length: u32, keywords: &HashMap<String, u32>) {
        let mut buffer = self.buffer.write().unwrap();
        for (term, &count) in keywords {
            buffer
                .postings
                .entry(term.clone())
                .or_default()
                .push((id, count));
        }
        buffer.lengths.insert(id, length);
        buffer.count += keywords.len();
        buffer.max_id = buffer.max_id.max(id);
        buffer.since.get_or_insert_with(Instant::now);
        let full = buffer.count >= self.flush_postings;
        drop(buffer);
        if full {
            self.upkeep_due.raise();
        }
    }

    /**
     * What a search reads (#248): the live segments, most pages first, and each term's postings
     * in the buffer and in one being flushed, as (id, count, page length), dead ids dropped.
     * Holding it keeps the segments' files from being erased.
     */
    pub fn view(&self, terms: &[&str]) -> View<'_> {
        let in_buffer = |buffer: &Buffer, term: &str, out: &mut Vec<(u64, u32, u32)>| {
            if let Some(list) = buffer.postings.get(term) {
                out.extend(
                    list.iter()
                        .map(|&(id, count)| (id, count, buffer.lengths[&id])),
                );
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

    /// The live pages a term is on, as (id, count, page length)
    #[cfg(test)]
    pub fn postings(&self, term: &str) -> io::Result<Vec<(u64, u32, u32)>> {
        let in_buffer = |buffer: &Buffer| -> Vec<(u64, u32, u32)> {
            buffer.postings.get(term).map_or(Vec::new(), |list| {
                list.iter()
                    .map(|&(id, count)| (id, count, buffer.lengths[&id]))
                    .collect()
            })
        };
        // the buffer and state together, so a flush moving postings between them isn't missed
        let (state, mut found) = {
            let buffer = self.buffer.read().unwrap();
            let state = self.state.read().unwrap().clone();
            (state, in_buffer(&buffer))
        };
        if let Some(frozen) = &state.frozen {
            found.extend(in_buffer(frozen));
        }
        for segment in state.segments.iter() {
            for (id, count) in segment.postings(term.as_bytes())? {
                found.push((id, count, segment.length(id).unwrap_or(0)));
            }
        }
        let dead = self.dead.read().unwrap();
        found.retain(|(id, _, _)| !dead.ids.contains(id));
        Ok(found)
    }

    /// The ids from `from` to `to`, inclusive, that a live segment holds, ascending
    pub fn ids_between(&self, from: u64, to: u64) -> Vec<u64> {
        let live = self.state.read().unwrap().segments.clone();
        let mut ids: Vec<u64> = live
            .iter()
            .flat_map(|segment| segment.ids_between(from, to).iter().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Pages search must no longer return: hidden at once, and dropped from the buffer
    pub fn set_dead(&self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        {
            let mut dead = self.dead.write().unwrap();
            let dead = &mut *dead;
            // the files that can hold a dead id; installs and retiring change them under the dead
            // lock, so this sees each file either listed here or listing the id itself
            let live = self.state.read().unwrap().segments.clone();
            let retired = lock(&self.retired);
            let logging = !dead.pins.is_empty();
            for &id in ids {
                if !dead.ids.insert(id) {
                    continue;
                }
                if logging {
                    dead.log.push(id);
                }
                let mut held = false;
                for segment in live.iter().chain(retired.iter().map(|r| &r.segment)) {
                    if segment.contains(id) {
                        match dead.held.get_mut(segment.path()) {
                            Some(list) => list.push(id),
                            None => {
                                dead.held.insert(segment.path().to_path_buf(), vec![id]);
                            }
                        }
                        held = true;
                    }
                }
                if !held {
                    dead.unheld.push(id);
                }
            }
        }
        let mut buffer = self.buffer.write().unwrap();
        let gone: HashSet<u64> = ids
            .iter()
            .copied()
            .filter(|id| buffer.lengths.remove(id).is_some())
            .collect();
        if gone.is_empty() {
            return;
        }
        let mut removed = 0;
        buffer.postings.retain(|_, list| {
            let before = list.len();
            list.retain(|(id, _)| !gone.contains(id));
            removed += before - list.len();
            !list.is_empty()
        });
        buffer.count -= removed;
    }

    /// Dead pages whose segment files must be rewritten without them promptly (#249)
    pub fn mark_erase(&self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        lock(&self.erase).extend(ids);
        // only a counter: invalidating never waits for a term_stats computation
        self.stats_gen.fetch_add(1, Ordering::SeqCst);
        self.upkeep_due.raise();
    }

    /// Blocks until there's upkeep to do (a full buffer, an erase) or a timeout; for the
    /// upkeep thread
    pub fn wait(&self) {
        // retired files are polled for until no search holds them
        let timeout = if lock(&self.retired).is_empty() {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(100)
        };
        self.upkeep_due.wait(timeout);
    }

    /// Blocks until a segment is installed, or a second passes; for the merge thread
    pub fn wait_for_merge(&self) {
        self.merge_due.wait(Duration::from_secs(1));
    }

    /// Writes the buffer to a segment now (a clean shutdown, tests); doesn't wait for a merge
    pub fn flush(&self) -> io::Result<()> {
        let _writes = lock(&self.writes);
        self.flush_locked()
    }

    /**
     * One round for the upkeep thread: flush the buffer if it's full or old, rewrite segments
     * holding pages to erase (#249), and erase retired files no search holds; then drop the dead
     * ids nothing holds any more. Merging is merge()'s.
     */
    pub fn upkeep(&self) -> Maintained {
        let error = self.upkeep_round();
        Maintained {
            pruned: self.prune_dead(),
            error,
        }
    }

    /**
     * upkeep() with merge() in between, in one call: at startup, and in tests. Files of an older
     * format aren't upgraded here: that's merge()'s, in the background, so a start isn't held up
     * rewriting them all (#248).
     */
    pub fn maintain(&self) -> Maintained {
        let mut error = self.upkeep_round();
        if let Err(err) = self.cascade(false) {
            error.get_or_insert(err);
        }
        // the merges' inputs, if no search holds them
        if let Some(err) = self.upkeep_round() {
            error.get_or_insert(err);
        }
        Maintained {
            pruned: self.prune_dead(),
            error,
        }
    }

    // writes, then retired files once the writes lock is free; the first error
    fn upkeep_round(&self) -> Option<io::Error> {
        let written = self.writes_round();
        let retired = self.retire();
        written.and(retired).err()
    }

    fn writes_round(&self) -> io::Result<()> {
        if self.dir.is_none() {
            return Ok(());
        }
        let _writes = lock(&self.writes);
        self.writes_locked()
    }

    // a due flush, then erasures, which run even if the flush failed. Under the writes lock.
    fn writes_locked(&self) -> io::Result<()> {
        let flushed = self.flush_if_due();
        let erased = self.erase_pending();
        flushed.and(erased)
    }

    fn flush_if_due(&self) -> io::Result<()> {
        if self.flush_due() {
            self.flush_locked()?;
        }
        Ok(())
    }

    fn flush_due(&self) -> bool {
        if self.state.read().unwrap().frozen.is_some() {
            return true; // a failed flush, retried
        }
        let buffer = self.buffer.read().unwrap();
        buffer.count >= self.flush_postings
            || buffer
                .since
                .is_some_and(|since| since.elapsed() >= self.flush_age)
    }

    /**
     * Merges segments until none are due, and upgrades files of an older format one at a time
     * (#248); for the merge thread. Between merges, erasures, a due flush and retired files go
     * first (unless the upkeep thread is already on them), so a long cascade doesn't starve them.
     */
    pub fn merge(&self) -> io::Result<()> {
        self.cascade(true)
    }

    // merge(), upgrading older files too if `upgrade`
    fn cascade(&self, upgrade: bool) -> io::Result<()> {
        if self.dir.is_none() {
            return Ok(());
        }
        let _merging = lock(&self.merging);
        let mut first = true;
        loop {
            if !first {
                let writes = match self.writes.try_lock() {
                    Ok(writes) => Some(writes),
                    Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
                    Err(TryLockError::WouldBlock) => None,
                };
                if let Some(_writes) = writes {
                    if let Err(err) = self.writes_locked() {
                        log::warn!("Segment upkeep between merges failed: {}", err);
                    }
                }
                let retiring = match self.retiring.try_lock() {
                    Ok(retiring) => Some(retiring),
                    Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
                    Err(TryLockError::WouldBlock) => None,
                };
                if let Some(_retiring) = retiring {
                    if let Err(err) = self.retire_locked() {
                        log::warn!("Erasing retired segments between merges failed: {}", err);
                    }
                }
            }
            first = false;
            let Some((inputs, upgrading)) = self.pick_merge(upgrade) else {
                return Ok(());
            };
            #[cfg(test)]
            self.hook(Hook::BeforeMerge);
            // false: another install took an input first; the next pick starts from what's live
            match self.rewrite(&inputs) {
                // a failed upgrade (a full disk, say) mustn't hold up the merges behind it; a
                // damaged file is reported (v0.5.6)
                Err(err) if upgrading && err.kind() != io::ErrorKind::InvalidData => {
                    log::warn!(
                        "Upgrading segment {} failed, retrying in {} minutes: {}",
                        inputs[0].path().display(),
                        UPGRADE_BACKOFF.as_secs() / 60,
                        err
                    );
                    *lock(&self.upgrades_after) = Some(Instant::now() + UPGRADE_BACKOFF);
                }
                rewritten => {
                    rewritten?;
                }
            }
        }
    }

    fn flush_locked(&self) -> io::Result<()> {
        if self.dir.is_none() {
            return Ok(());
        }
        let frozen = {
            let mut buffer = self.buffer.write().unwrap();
            let mut state = self.state.write().unwrap();
            match &state.frozen {
                Some(frozen) => frozen.clone(),
                None => {
                    if buffer.since.is_none() {
                        return Ok(());
                    }
                    let taken = Arc::new(std::mem::take(&mut *buffer));
                    state.frozen = Some(taken.clone());
                    taken
                }
            }
        };
        let pin = self.pin();
        let dead = self.dead.read().unwrap().ids.clone();
        let segment = self.write_segment(|writer| {
            for (&id, &length) in &frozen.lengths {
                if !dead.contains(&id) {
                    writer.add_doc(id, length)?;
                }
            }
            for (term, list) in &frozen.postings {
                let live: Vec<Posting> = list
                    .iter()
                    .copied()
                    .filter(|(id, _)| !dead.contains(id))
                    .collect();
                if !live.is_empty() {
                    writer.add_term(term.as_bytes(), &live)?;
                }
            }
            Ok(())
        })?;
        self.install(&[], segment, &pin, Some(frozen.max_id))
            .map(|_| ())
    }

    // a writer's place in the dead log: take it before reading the dead set
    fn pin(&self) -> Pin<'_> {
        let mut dead = self.dead.write().unwrap();
        let at = dead.log_start + dead.log.len() as u64;
        dead.pins.push(at);
        Pin { segments: self, at }
    }

    // writes a new segment file; a failed write's partial file is erased. None if it has no pages.
    fn write_segment(
        &self,
        fill: impl FnOnce(&mut SegmentWriter) -> io::Result<()>,
    ) -> io::Result<Option<Arc<Segment>>> {
        let dir = self.dir.as_ref().expect("segment files need a directory");
        let generation = self.next_gen.fetch_add(1, Ordering::SeqCst);
        let path = dir.join(format!("{:016x}.seg", generation));
        #[cfg(test)]
        let created =
            SegmentWriter::create_version(&path, self.write_version.load(Ordering::SeqCst));
        #[cfg(not(test))]
        let created = SegmentWriter::create(&path);
        let written = created.and_then(|mut writer| {
            fill(&mut writer)?;
            writer.finish()
        });
        let info = match written {
            Ok(info) => info,
            Err(err) => {
                if path.exists() {
                    let _ = erase_file(&path);
                }
                return Err(err);
            }
        };
        if info.docs == 0 {
            erase_file(&path)?;
            return Ok(None);
        }
        match Segment::open_written(&path) {
            Ok(segment) => Ok(Some(Arc::new(segment))),
            Err(err) => {
                let _ = erase_file(&path);
                Err(err)
            }
        }
    }

    /**
     * Swaps `inputs` for `output` in the segment set as it is now: the manifest first (the
     * commit point), then what searches see. A flush or rewrite installed since this writer
     * started is kept. If an input is gone (another rewrite replaced it), the output is erased
     * and false returned; the next round redoes the work from what's live. `flushed`: a flush's
     * highest id, which also clears the frozen buffer.
     */
    fn install(
        &self,
        inputs: &[Arc<Segment>],
        output: Option<Arc<Segment>>,
        pin: &Pin,
        flushed: Option<u64>,
    ) -> io::Result<bool> {
        let installed = {
            let _install = lock(&self.install);
            self.install_locked(inputs, &output, pin, flushed)
        };
        // an output not installed is zeroed once the install lock is free: it can be big
        if !matches!(installed, Ok(true)) {
            if let Some(output) = output {
                #[cfg(test)]
                self.hook(Hook::Discarding);
                discard(output);
            }
        }
        installed
    }

    fn install_locked(
        &self,
        inputs: &[Arc<Segment>],
        output: &Option<Arc<Segment>>,
        pin: &Pin,
        flushed: Option<u64>,
    ) -> io::Result<bool> {
        let dir = self.dir.as_ref().expect("segment files need a directory");
        // only installs change the list, and they hold the install lock
        let current = self.state.read().unwrap().segments.clone();
        let is_input = |s: &Arc<Segment>| inputs.iter().any(|input| Arc::ptr_eq(s, input));
        if current.iter().filter(|s| is_input(s)).count() != inputs.len() {
            return Ok(false);
        }
        let mut live: Vec<Arc<Segment>> =
            current.iter().filter(|s| !is_input(s)).cloned().collect();
        live.extend(output.iter().cloned());
        live.sort_by_key(|segment| segment.info().min_id);
        let flushed_through = self.flushed_through().max(flushed.unwrap_or(0));
        write_manifest(
            dir,
            self.next_gen.load(Ordering::SeqCst),
            flushed_through,
            &live,
        )?;
        self.flushed_through
            .store(flushed_through, Ordering::SeqCst);
        {
            let mut dead = self.dead.write().unwrap();
            if let Some(output) = output {
                // its dead ids are the ones that died after the writer read the dead set
                let from = (pin.at - dead.log_start) as usize;
                let held: Vec<u64> = dead.log[from..]
                    .iter()
                    .copied()
                    .filter(|&id| output.contains(id))
                    .collect();
                if !held.is_empty() {
                    dead.held.insert(output.path().to_path_buf(), held);
                }
            }
            let mut state = self.state.write().unwrap();
            state.segments = Arc::new(live);
            if flushed.is_some() {
                state.frozen = None;
            }
            drop(state);
            // still under the dead lock, so set_dead sees the inputs live or retired
            lock(&self.retired).extend(inputs.iter().map(|segment| Retired {
                segment: segment.clone(),
                failures: 0,
            }));
        }
        self.installs.fetch_add(1, Ordering::SeqCst);
        // after the swap, so a term_stats computing from the old set caches it as stale
        self.stats_gen.fetch_add(1, Ordering::SeqCst);
        self.merge_due.raise();
        Ok(true)
    }

    // whether every one of `segments` is still live
    fn all_live(&self, segments: &[Arc<Segment>]) -> bool {
        let live = self.state.read().unwrap().segments.clone();
        segments
            .iter()
            .all(|s| live.iter().any(|l| Arc::ptr_eq(l, s)))
    }

    /**
     * Drops dead ids nothing holds any more, and returns them. Only ids whose last file went (or
     * that never reached one) are checked, a chunk at a time, so the dead lock is held for
     * O(chunk x segments) rather than a scan of every dead id.
     */
    fn prune_dead(&self) -> Vec<u64> {
        let pending = std::mem::take(&mut self.dead.write().unwrap().unheld);
        let mut gone = Vec::new();
        for chunk in pending.chunks(PRUNE_CHUNK) {
            let mut dead = self.dead.write().unwrap();
            let dead = &mut *dead;
            let buffer = self.buffer.read().unwrap();
            let state = self.state.read().unwrap().clone();
            // a search may still read a retired file, so its pages stay dead until it's erased
            let retired = lock(&self.retired);
            for &id in chunk {
                if !dead.ids.contains(&id) {
                    continue; // listed twice, pruned already
                }
                let buffered = buffer.lengths.contains_key(&id)
                    || state
                        .frozen
                        .as_ref()
                        .is_some_and(|f| f.lengths.contains_key(&id));
                if buffered {
                    // a flush in progress may write it yet: checked again next time
                    dead.unheld.push(id);
                    continue;
                }
                let held = state
                    .segments
                    .iter()
                    .chain(retired.iter().map(|r| &r.segment))
                    .chain(dead.abandoned.iter())
                    .any(|s| s.contains(id));
                // a file holding it lists it, and unholds it once it's erased
                if !held {
                    dead.ids.remove(&id);
                    gone.push(id);
                }
            }
        }
        if !gone.is_empty() {
            let mut erase = lock(&self.erase);
            for id in &gone {
                erase.remove(id);
            }
        }
        gone
    }

    // rewrites every live segment holding a page marked for erasure (#249); a mark stays until
    // its id is pruned, so a merge output still holding it is caught in a later round
    fn erase_pending(&self) -> io::Result<()> {
        loop {
            let mut targets: Vec<u64> = lock(&self.erase).iter().copied().collect();
            // an id pruned already holds nothing, but a purge or expiry can mark it again from
            // a dead_pages row not yet deleted; such a mark would never be pruned
            let pruned: HashSet<u64> = {
                let dead = self.dead.read().unwrap();
                targets
                    .iter()
                    .copied()
                    .filter(|id| !dead.ids.contains(id))
                    .collect()
            };
            if !pruned.is_empty() {
                lock(&self.erase).retain(|id| !pruned.contains(id));
                targets.retain(|id| !pruned.contains(id));
            }
            if targets.is_empty() {
                return Ok(());
            }
            let holding: Vec<Arc<Segment>> = {
                let live = self.state.read().unwrap().segments.clone();
                live.iter()
                    .filter(|s| targets.iter().any(|&id| s.contains(id)))
                    .cloned()
                    .collect()
            };
            let mut replaced = false;
            for segment in holding {
                replaced |= !self.rewrite(std::slice::from_ref(&segment))?;
            }
            // a merge installed over one of them: its output may hold the page now
            if !replaced {
                return Ok(());
            }
        }
    }

    // a mostly-dead segment on its own, else (if `upgrade`, and none failed lately) an older
    // format file on its own, else TIER_FANOUT segments of one size tier; and whether it's an
    // upgrade
    fn pick_merge(&self, upgrade: bool) -> Option<(Vec<Arc<Segment>>, bool)> {
        let live = self.state.read().unwrap().segments.clone();
        {
            let dead = self.dead.read().unwrap();
            for segment in live.iter() {
                let n = dead.held.get(segment.path()).map_or(0, Vec::len) as u64;
                if n > 0 && n * 100 >= segment.info().docs * DEAD_PERCENT {
                    return Some((vec![segment.clone()], false));
                }
            }
        }
        // a file from before format v2 (#248), rewritten alone so its blocks get tight bounds;
        // merges never reach one of a gibibyte or more, so it'd stay v1 otherwise
        let backing_off = lock(&self.upgrades_after).is_some_and(|after| Instant::now() < after);
        let old = match upgrade && !backing_off {
            true => live.iter().find(|s| s.version() < crate::segment::VERSION),
            false => None,
        };
        if let Some(old) = old {
            log::info!(
                "Upgrading segment {} to format v{}",
                old.path().display(),
                crate::segment::VERSION
            );
            return Some((vec![old.clone()], true));
        }
        let sizes: Vec<u64> = live.iter().map(|s| s.info().bytes).collect();
        pick_tier(&sizes)
            .map(|picked| (picked.into_iter().map(|i| live[i].clone()).collect(), false))
    }

    /**
     * Replaces `inputs` with one segment of their live pages; the inputs are retired. False if
     * another install replaced an input first: nothing changes then. It stops early if that
     * happens while it runs, so a merge can't keep a taken-down file from being erased.
     */
    fn rewrite(&self, inputs: &[Arc<Segment>]) -> io::Result<bool> {
        let pin = self.pin();
        let dead = self.dead.read().unwrap().ids.clone();
        let mut seen = self.installs.load(Ordering::SeqCst);
        // replaced between the pick and `seen`: no later install would prompt a check
        if !self.all_live(inputs) {
            return Ok(false);
        }
        // a file opened at startup is checked before it's copied (v0.5.6): else damage the
        // background check hadn't reached would go on in a file with a sound checksum, and the
        // damaged one be erased unreported. A mismatch fails the rewrite, as corrupt.
        for input in inputs {
            if let Err(err) = input.verify() {
                // held here, so not erased under the read; but replaced, it no longer matters
                if !self.all_live(inputs) {
                    return Ok(false);
                }
                return Err(err);
            }
        }
        let mut replaced = || {
            #[cfg(test)]
            self.hook(Hook::Checking);
            let installs = self.installs.load(Ordering::SeqCst);
            if installs == seen {
                return Ok(());
            }
            seen = installs;
            match self.all_live(inputs) {
                true => Ok(()),
                false => Err(io::Error::other(Superseded)),
            }
        };
        let written = self.write_segment(|writer| {
            // page ids never repeat across segments, but their ranges can interleave
            let mut docs: Vec<_> = inputs.iter().map(|s| s.docs().peekable()).collect();
            let mut step = 0;
            loop {
                step += 1;
                if step % CHECK_EVERY == 0 {
                    replaced()?;
                }
                let next = docs
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(i, d)| d.peek().map(|&(id, _)| (id, i)))
                    .min();
                let Some((_, i)) = next else { break };
                let (id, length) = docs[i].next().unwrap();
                if !dead.contains(&id) {
                    writer.add_doc(id, length)?;
                }
            }
            let mut sources: Vec<TermSource<'_, (u64, u64)>> = Vec::new();
            for input in inputs {
                let terms = input.terms()?;
                sources.push(Box::new(
                    terms.map(|t| t.map(|(term, df, at)| (term, (df, at)))),
                ));
            }
            merge_terms(sources, |term, group| {
                step += 1;
                if step % CHECK_EVERY == 0 {
                    replaced()?;
                }
                let mut list = Vec::new();
                for &(i, (df, offset)) in group {
                    let postings = inputs[i].postings_at(df, offset)?;
                    list.extend(postings.into_iter().filter(|(id, _)| !dead.contains(id)));
                }
                if !list.is_empty() {
                    list.sort_unstable();
                    writer.add_term(term, &list)?;
                }
                Ok(())
            })
        });
        let output = match written {
            Ok(output) => output,
            Err(err) if is_superseded(&err) => return Ok(false),
            Err(err) => return Err(err),
        };
        #[cfg(test)]
        self.hook(Hook::BeforeInstall);
        self.install(inputs, output, &pin, None)
    }

    /**
     * Erases retired files no search holds any more, under the retiring lock alone, so zeroing a
     * big file holds up neither flushes nor erase rewrites. A file whose erase fails ERASE_TRIES
     * times is given
     * up on (logged); what it holds stays dead, and it's retried at the next start, as a file
     * the manifest doesn't name.
     */
    fn retire(&self) -> io::Result<()> {
        let _retiring = lock(&self.retiring);
        self.retire_locked()
    }

    fn retire_locked(&self) -> io::Result<()> {
        let ready: Vec<Arc<Segment>> = lock(&self.retired)
            .iter()
            .filter(|r| Arc::strong_count(&r.segment) == 1)
            .map(|r| r.segment.clone())
            .collect();
        let mut result = Ok(());
        for segment in ready {
            // no search can take it again: it's in no segment set
            #[cfg(test)]
            self.hook(Hook::Retiring);
            let erased = erase_file(segment.path());
            {
                let mut dead = self.dead.write().unwrap();
                let mut retired = lock(&self.retired);
                let Some(i) = retired
                    .iter()
                    .position(|r| Arc::ptr_eq(&r.segment, &segment))
                else {
                    continue;
                };
                match erased {
                    Ok(()) => {
                        retired.swap_remove(i);
                        // its dead ids may be in no file now
                        if let Some(ids) = dead.held.remove(segment.path()) {
                            dead.unheld.extend(ids);
                        }
                    }
                    Err(err) => {
                        retired[i].failures += 1;
                        if retired[i].failures >= ERASE_TRIES {
                            log::error!(
                                "Giving up erasing {} after {} tries: {}. Its pages stay dead; \
                                 it's retried at the next start.",
                                segment.path().display(),
                                ERASE_TRIES,
                                err
                            );
                            dead.held.remove(segment.path());
                            dead.abandoned.push(retired.swap_remove(i).segment);
                        }
                        if result.is_ok() {
                            result = Err(err);
                        }
                    }
                }
            }
        }
        result
    }

    /**
     * Distinct terms, and the n on the most pages (df descending, then term), over the segments
     * and the buffer. Like Lucene's docFreq this counts dead pages not merged away yet, so it's
     * for /status and /keywords, not ranking. On disk it's the cached value, however stale:
     * counting takes seconds on a big index, so the merge thread refreshes it (refresh_stats)
     * and only the first call after startup waits for a count.
     */
    pub fn term_stats(&self, n: usize) -> io::Result<(usize, Vec<(String, u32)>)> {
        if self.dir.is_none() || n > STATS_TOP {
            return self.compute_term_stats(n);
        }
        let take = |stats: &Stats| (stats.count, stats.top.iter().take(n).cloned().collect());
        if let Some(stats) = lock(&self.stats).clone() {
            return Ok(take(&stats));
        }
        // no value yet: one caller counts, the others wait for it
        let _computing = lock(&self.computing);
        if let Some(stats) = lock(&self.stats).clone() {
            return Ok(take(&stats));
        }
        let stats = self.compute_stats()?;
        Ok(take(&stats))
    }

    /// Recounts the cached term stats if they're at least min_age old and the segment set has
    /// changed since, or they're over STATS_TTL old (the buffer has changed). For the merge
    /// thread, with min_age STATS_TTL, so a busy crawl costs one count a minute.
    pub fn refresh_stats(&self, min_age: Duration) -> io::Result<()> {
        if self.dir.is_none() {
            return Ok(());
        }
        let _computing = lock(&self.computing);
        if let Some(stats) = lock(&self.stats).clone() {
            let age = stats.at.elapsed();
            let changed = stats.generation != self.stats_gen.load(Ordering::SeqCst);
            if age < min_age || !(changed || age >= STATS_TTL) {
                return Ok(());
            }
        }
        self.compute_stats().map(|_| ())
    }

    // counts and caches the stats; the caller holds computing
    fn compute_stats(&self) -> io::Result<Arc<Stats>> {
        // read before the snapshot: an install after it leaves this value stale
        let generation = self.stats_gen.load(Ordering::SeqCst);
        let (count, top) = self.compute_term_stats(STATS_TOP)?;
        let stats = Arc::new(Stats {
            generation,
            at: Instant::now(),
            count,
            top,
        });
        *lock(&self.stats) = Some(stats.clone());
        Ok(stats)
    }

    fn compute_term_stats(&self, n: usize) -> io::Result<(usize, Vec<(String, u32)>)> {
        let in_buffer = |buffer: &Buffer| -> TermSource<'_, u64> {
            let terms: Vec<_> = buffer
                .postings
                .iter()
                .map(|(t, list)| Ok((t.as_bytes().to_vec(), list.len() as u64)))
                .collect();
            Box::new(terms.into_iter())
        };
        let (state, mut sources) = {
            let buffer = self.buffer.read().unwrap();
            let state = self.state.read().unwrap().clone();
            (state, vec![in_buffer(&buffer)])
        };
        if let Some(frozen) = &state.frozen {
            sources.push(in_buffer(frozen));
        }
        #[cfg(test)]
        self.hook(Hook::StatsComputing);
        for segment in state.segments.iter() {
            let terms = segment.terms()?.map(|t| t.map(|(term, df, _)| (term, df)));
            sources.push(Box::new(terms));
        }
        let mut count = 0;
        // the best n so far, smallest first: (df, then the earlier term ranks higher)
        let mut top: BinaryHeap<Reverse<(u64, Reverse<Vec<u8>>)>> = BinaryHeap::new();
        merge_terms(sources, |term, group| {
            count += 1;
            let df: u64 = group.iter().map(|&(_, df)| df).sum();
            // copied only if it makes the top n
            let enters = n > 0
                && (top.len() < n
                    || top.peek().is_some_and(|Reverse((low, Reverse(low_term)))| {
                        df > *low || (df == *low && term < low_term.as_slice())
                    }));
            if enters {
                top.push(Reverse((df, Reverse(term.to_vec()))));
                if top.len() > n {
                    top.pop();
                }
            }
            Ok(())
        })?;
        let mut ranked: Vec<(String, u32)> = top
            .into_iter()
            .map(|Reverse((df, Reverse(term)))| {
                (String::from_utf8_lossy(&term).into_owned(), df as u32)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok((count, ranked))
    }
}

// TIER_FANOUT segments (by index) of the smallest tier with that many; tiers from
// MAX_MERGE_TIER up are left alone
fn pick_tier(sizes: &[u64]) -> Option<Vec<usize>> {
    let mut tiers: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, &bytes) in sizes.iter().enumerate() {
        let tier = (bytes >> 20).max(1).ilog10();
        if tier < MAX_MERGE_TIER {
            tiers.entry(tier).or_default().push(i);
        }
    }
    tiers.into_values().find_map(|mut tier| {
        (tier.len() >= TIER_FANOUT).then(|| {
            tier.truncate(TIER_FANOUT);
            tier
        })
    })
}

#[cfg(test)]
impl Segments {
    pub(crate) fn set_write_version(&self, version: u8) {
        self.write_version.store(version, Ordering::SeqCst);
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.state.read().unwrap().segments.len()
    }

    pub(crate) fn snapshot(&self) -> Arc<Vec<Arc<Segment>>> {
        self.state.read().unwrap().segments.clone()
    }

    // takes a file out of the set and erases it, as a merge does, minus the merge
    pub(crate) fn remove_for_test(&self, path: &Path) {
        let mut state = self.state.write().unwrap();
        let kept = state
            .segments
            .iter()
            .filter(|s| s.path() != path)
            .cloned()
            .collect();
        state.segments = Arc::new(kept);
        drop(state);
        erase_file(path).unwrap();
    }

    pub(crate) fn set_hook(&self, hook: impl Fn(&Segments, Hook) + Send + Sync + 'static) {
        *self.hook.lock().unwrap() = Some(Arc::new(hook));
    }

    fn hook(&self, at: Hook) {
        // cloned out, so a hook can run code that reaches another hook
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(self, at);
        }
    }

    pub(crate) fn set_flush_limits(&mut self, postings: usize, age: Duration) {
        self.flush_postings = postings;
        self.flush_age = age;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::tests::temp_dir;

    pub(crate) fn words(pairs: &[(&str, u32)]) -> HashMap<String, u32> {
        pairs.iter().map(|(w, c)| (w.to_string(), *c)).collect()
    }

    // one maintain(), which must not fail; the ids it pruned
    fn maintained(segments: &Segments) -> Vec<u64> {
        let done = segments.maintain();
        assert!(done.error.is_none(), "{:?}", done.error);
        let mut pruned = done.pruned;
        pruned.sort();
        pruned
    }

    // true if any file under dir (recursively) contains needle
    pub(crate) fn dir_has(dir: &Path, needle: &str) -> bool {
        fs::read_dir(dir).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dir_has(&path, needle)
            } else {
                let bytes = fs::read(&path).unwrap();
                bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
            }
        })
    }

    #[test]
    fn buffered_postings_are_searchable_and_survive_a_flush_and_reopen() {
        let dir = temp_dir("segments");
        let segments = Segments::open(&dir).unwrap();
        segments.add(1, 3, &words(&[("alpha", 2), ("beta", 1)]));
        segments.add(2, 5, &words(&[("alpha", 5)]));
        segments.add(3, 0, &words(&[])); // a page with no terms
        let mut alpha = segments.postings("alpha").unwrap();
        alpha.sort();
        assert_eq!(alpha, vec![(1, 2, 3), (2, 5, 5)]);
        assert_eq!(segments.segment_count(), 0);

        segments.flush().unwrap();
        assert_eq!(segments.segment_count(), 1);
        assert_eq!(segments.flushed_through(), 3);
        assert_eq!(segments.postings("beta").unwrap(), vec![(1, 1, 3)]);
        drop(segments);

        let reopened = Segments::open(&dir).unwrap();
        assert_eq!(reopened.flushed_through(), 3);
        let mut alpha = reopened.postings("alpha").unwrap();
        alpha.sort();
        assert_eq!(alpha, vec![(1, 2, 3), (2, 5, 5)]);
    }

    #[test]
    fn dead_pages_are_hidden_at_once_and_never_flushed() {
        let dir = temp_dir("segments-dead");
        let segments = Segments::open(&dir).unwrap();
        segments.add(1, 1, &words(&[("zzqxdeadterm", 1)]));
        segments.add(2, 1, &words(&[("alpha", 1)]));
        segments.set_dead(&[1]);
        assert!(segments.postings("zzqxdeadterm").unwrap().is_empty());
        segments.flush().unwrap();
        assert!(
            !dir_has(&dir, "zzqxdeadterm"),
            "dropped from the buffer, not written"
        );
        assert_eq!(
            maintained(&segments),
            vec![1],
            "no segment holds it: pruned"
        );
    }

    #[test]
    fn a_flushed_page_marked_dead_is_hidden() {
        let segments = Segments::open(&temp_dir("segments-dead-flushed")).unwrap();
        // six pages: one dead page stays under the share that triggers a rewrite (Task 3)
        for id in 1..=6 {
            segments.add(id, 1, &words(&[("alpha", 1)]));
        }
        segments.flush().unwrap();
        segments.set_dead(&[1]);
        assert_eq!(segments.postings("alpha").unwrap().len(), 5);
        assert!(maintained(&segments).is_empty(), "still in a segment");
    }

    #[test]
    fn flushes_when_the_buffer_is_full_or_old() {
        let mut segments = Segments::open(&temp_dir("segments-due")).unwrap();
        segments.set_flush_limits(2, Duration::from_secs(3600));
        segments.add(1, 1, &words(&[("a", 1)]));
        maintained(&segments);
        assert_eq!(segments.segment_count(), 0, "not due");
        segments.add(2, 1, &words(&[("b", 1)]));
        maintained(&segments);
        assert_eq!(segments.segment_count(), 1, "full");
        segments.set_flush_limits(1000, Duration::ZERO);
        segments.add(3, 1, &words(&[("c", 1)]));
        maintained(&segments);
        assert_eq!(segments.segment_count(), 2, "old");
        maintained(&segments);
        assert_eq!(segments.segment_count(), 2, "nothing to flush");
    }

    #[test]
    fn unnamed_segment_files_are_erased_on_open() {
        let dir = temp_dir("segments-orphans");
        fs::write(dir.join("00000000000000ff.seg"), "zzqxorphanterm").unwrap();
        fs::write(dir.join("MANIFEST.tmp"), "zzqxorphanterm").unwrap();
        Segments::open(&dir).unwrap();
        assert!(!dir_has(&dir, "zzqxorphanterm"));
        assert!(!dir.join("00000000000000ff.seg").exists());
    }

    #[test]
    fn a_corrupt_named_segment_fails_open() {
        let dir = temp_dir("segments-corrupt");
        let segments = Segments::open(&dir).unwrap();
        segments.add(1, 1, &words(&[("a", 1)]));
        segments.flush().unwrap();
        let file = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "seg"))
            .unwrap();
        fs::write(&file, b"garbage").unwrap();
        assert!(Segments::open(&dir).is_err());
    }

    #[test]
    fn a_failed_write_leaves_no_partial_file() {
        let dir = temp_dir("segments-failed");
        let segments = Segments::open(&dir).unwrap();
        let result = segments.write_segment(|w| {
            w.add_doc(1, 1)?;
            w.add_term(b"zzqxpartialterm", &[(1, 1)])?;
            Err(io::Error::other("disk full"))
        });
        assert!(result.is_err());
        assert!(!dir_has(&dir, "zzqxpartialterm"));
    }

    #[test]
    fn a_segment_whose_manifest_write_fails_is_erased() {
        let dir = temp_dir("segments-install");
        let segments = Segments::open(&dir).unwrap();
        segments.add(1, 1, &words(&[("zzqxinstallterm", 1)]));
        // rename(2) of a file over a directory fails on Linux, so the manifest install fails
        fs::create_dir(dir.join("MANIFEST")).unwrap();
        assert!(segments.flush().is_err());
        assert!(
            !dir_has(&dir, "zzqxinstallterm"),
            "the unlisted segment's text is erased"
        );
        assert_eq!(segments.segment_count(), 0);
    }

    #[test]
    fn in_memory_never_flushes() {
        let segments = Segments::in_memory();
        segments.add(1, 1, &words(&[("a", 1)]));
        segments.flush().unwrap();
        maintained(&segments);
        assert_eq!(segments.segment_count(), 0);
        assert_eq!(segments.postings("a").unwrap(), vec![(1, 1, 1)]);
    }

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
        let total: u64 = view.segments.iter().map(|s| s.df(b"common").unwrap()).sum();
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

        // startup's maintain leaves it to the merge thread
        maintained(&segments);
        assert_eq!(
            segments.snapshot()[0].path(),
            old,
            "not upgraded at startup"
        );
        assert_eq!(segments.postings("alpha").unwrap(), before);

        segments.merge().unwrap();
        maintained(&segments); // the v1 file, retired, is erased
        let live = segments.snapshot();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].version(), crate::segment::VERSION);
        assert!(!old.exists(), "the v1 file was retired and erased");
        assert_eq!(segments.postings("alpha").unwrap(), before);
        segments.merge().unwrap();
        assert_eq!(
            segments.snapshot()[0].path(),
            live[0].path(),
            "upgraded once"
        );
    }

    #[test]
    fn a_failed_upgrade_doesnt_hold_up_a_tier_merge() {
        let dir = temp_dir("segments-upgrade-fails");
        let segments = Segments::open(&dir).unwrap();
        // ten v2 segments of a tier, then a v1 one the tier merge won't take
        for id in 1..=10 {
            flushed(&segments, &[(id, &[("alpha", 1)])]);
        }
        segments.set_write_version(1);
        flushed(&segments, &[(11, &[("alpha", 1)])]);
        segments.set_write_version(crate::segment::VERSION);
        let old = segments.snapshot()[10].path().to_path_buf();
        // the upgrade, picked first, would write the next file name: taken, so it fails
        let next = segments.next_gen.load(Ordering::SeqCst);
        fs::write(dir.join(format!("{:016x}.seg", next)), b"taken").unwrap();
        let mut before = segments.postings("alpha").unwrap();
        before.sort();
        let paths = |segments: &Segments| -> Vec<PathBuf> {
            let live = segments.snapshot();
            live.iter().map(|s| s.path().to_path_buf()).collect()
        };

        segments.merge().unwrap();
        let live = paths(&segments);
        assert_eq!(live.len(), 2, "the ten v2 segments merged anyway");
        assert!(live.contains(&old), "the v1 file is still live");
        assert!(lock(&segments.upgrades_after).is_some(), "backing off");
        segments.merge().unwrap();
        assert_eq!(paths(&segments), live, "not retried at once");
        let mut after = segments.postings("alpha").unwrap();
        after.sort();
        assert_eq!(after, before);

        // once the backoff passes, it's retried
        *lock(&segments.upgrades_after) = Some(Instant::now());
        segments.merge().unwrap();
        assert!(segments
            .snapshot()
            .iter()
            .all(|s| s.version() == crate::segment::VERSION));
    }

    // one segment per page, flushed one at a time
    fn flushed(segments: &Segments, pages: &[(u64, &[(&str, u32)])]) {
        for &(id, pairs) in pages {
            segments.add(id, pairs.iter().map(|p| p.1).sum(), &words(pairs));
            segments.flush().unwrap();
        }
    }

    #[test]
    fn a_takedown_rewrites_the_segment_and_leaves_no_bytes_on_disk() {
        let dir = temp_dir("segments-erase");
        let segments = Segments::open(&dir).unwrap();
        // six pages in one segment: one dead page is under the 20% that triggers a rewrite alone
        for id in 1..=6 {
            let term = if id == 3 { "zzqxerasedterm" } else { "alpha" };
            segments.add(id, 1, &words(&[(term, 1)]));
        }
        segments.flush().unwrap();
        assert!(dir_has(&dir, "zzqxerasedterm"));

        segments.set_dead(&[3]);
        segments.mark_erase(&[3]);
        assert_eq!(maintained(&segments), vec![3]);
        assert!(!dir_has(&dir, "zzqxerasedterm"));
        assert_eq!(segments.postings("alpha").unwrap().len(), 5);
        assert_eq!(segments.segment_count(), 1);
    }

    #[test]
    fn without_an_erase_a_lightly_dead_segment_keeps_its_bytes() {
        // control: proves the assertion above can fail
        let dir = temp_dir("segments-erase-control");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=6 {
            let term = if id == 3 { "zzqxkeptterm" } else { "alpha" };
            segments.add(id, 1, &words(&[(term, 1)]));
        }
        segments.flush().unwrap();
        segments.set_dead(&[3]);
        assert!(maintained(&segments).is_empty());
        assert!(dir_has(&dir, "zzqxkeptterm"));
    }

    #[test]
    fn a_mostly_dead_segment_is_rewritten_alone() {
        let dir = temp_dir("segments-mostly-dead");
        let segments = Segments::open(&dir).unwrap();
        flushed(&segments, &[(1, &[("zzqxsupersededterm", 1)])]);
        segments.set_dead(&[1]);
        assert_eq!(maintained(&segments), vec![1]);
        assert_eq!(segments.segment_count(), 0, "nothing left in it");
        assert!(!dir_has(&dir, "zzqxsupersededterm"));
    }

    #[test]
    fn ten_segments_in_a_tier_merge_into_one() {
        let dir = temp_dir("segments-merge");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=10 {
            flushed(&segments, &[(id, &[("common", id as u32), ("only", 1)])]);
        }
        assert_eq!(segments.segment_count(), 10);
        let before = segments.postings("common").unwrap();
        maintained(&segments);
        assert_eq!(segments.segment_count(), 1);
        assert_eq!(segments.postings("common").unwrap(), before);
        assert_eq!(
            fs::read_dir(&dir)
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "seg"))
                .count(),
            1,
            "the inputs were retired and removed"
        );
    }

    #[test]
    fn a_search_in_progress_keeps_retired_files_until_it_finishes() {
        let dir = temp_dir("segments-retire");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=6 {
            let term = if id == 3 { "zzqxheldterm" } else { "alpha" };
            segments.add(id, 1, &words(&[(term, 1)]));
        }
        segments.flush().unwrap();
        let snapshot = segments.snapshot();
        segments.set_dead(&[3]);
        segments.mark_erase(&[3]);
        maintained(&segments);
        assert_eq!(
            snapshot[0].postings(b"alpha").unwrap().len(),
            5,
            "still readable, not zeroed under the reader"
        );
        drop(snapshot);
        maintained(&segments);
        assert!(!dir_has(&dir, "zzqxheldterm"));
    }

    #[test]
    fn term_stats_count_and_rank_across_segments_and_the_buffer() {
        let segments = Segments::open(&temp_dir("segments-stats")).unwrap();
        flushed(&segments, &[(1, &[("common", 1), ("rare", 1)])]);
        flushed(&segments, &[(2, &[("common", 1), ("middle", 1)])]);
        segments.add(3, 2, &words(&[("common", 1), ("middle", 1)]));
        assert_eq!(
            segments.term_stats(2).unwrap(),
            (
                3,
                vec![("common".to_string(), 3), ("middle".to_string(), 2)]
            )
        );
        assert_eq!(segments.term_stats(0).unwrap().0, 3);
        // cached until the next manifest swap
        segments.add(4, 1, &words(&[("fresh", 1)]));
        assert_eq!(segments.term_stats(0).unwrap().0, 3);
        // a manifest swap doesn't recount on the caller's time (/status), only a refresh does
        segments.flush().unwrap();
        assert_eq!(segments.term_stats(0).unwrap().0, 3);
        segments.refresh_stats(STATS_TTL).unwrap(); // too soon
        assert_eq!(segments.term_stats(0).unwrap().0, 3);
        segments.refresh_stats(Duration::ZERO).unwrap();
        assert_eq!(segments.term_stats(0).unwrap().0, 4);
    }

    #[test]
    fn term_stats_break_df_ties_by_term_across_sources() {
        let segments = Segments::open(&temp_dir("segments-stats-ties")).unwrap();
        flushed(&segments, &[(1, &[("delta", 1), ("bravo", 1)])]);
        flushed(
            &segments,
            &[(2, &[("charlie", 1), ("echo", 1), ("bravo", 1)])],
        );
        segments.add(3, 2, &words(&[("alpha", 1), ("echo", 1)]));
        let top = |n| {
            let (count, top) = segments.compute_term_stats(n).unwrap();
            let terms: Vec<String> = top.into_iter().map(|(t, _)| t).collect();
            (count, terms)
        };
        assert_eq!(
            top(3),
            (5, vec!["bravo".into(), "echo".into(), "alpha".into()])
        );
        assert_eq!(
            top(10).1,
            vec!["bravo", "echo", "alpha", "charlie", "delta"]
        );
        assert_eq!(top(0), (5, vec![]));
    }

    #[test]
    fn a_taken_down_page_stays_dead_while_a_search_holds_its_retired_file() {
        let dir = temp_dir("segments-retired-dead");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=6 {
            segments.add(id, 1, &words(&[("alpha", 1)]));
        }
        segments.flush().unwrap();
        let snapshot = segments.snapshot();
        segments.set_dead(&[3]);
        segments.mark_erase(&[3]);
        assert!(
            maintained(&segments).is_empty(),
            "the old file still holds it"
        );
        assert!(segments.postings("alpha").unwrap().iter().all(|p| p.0 != 3));
        drop(snapshot);
        assert_eq!(maintained(&segments), vec![3]);
    }

    #[test]
    fn an_install_never_waits_for_a_stats_refresh_and_callers_get_the_stale_value() {
        use std::sync::mpsc::channel;
        let wait = Duration::from_secs(2);
        let segments = Arc::new(Segments::open(&temp_dir("segments-stats-wait")).unwrap());
        flushed(&segments, &[(1, &[("common", 1)])]);
        assert_eq!(segments.term_stats(1).unwrap().0, 1);
        segments.add(2, 1, &words(&[("fresh", 1)]));
        segments.flush().unwrap(); // the cached value is stale now
                                   // the next computation stops in the middle until released
        let (entered, in_compute) = channel();
        let (release, released) = channel::<()>();
        let (entered, released) = (Mutex::new(Some(entered)), Mutex::new(released));
        segments.set_hook(move |_, at| {
            if at == Hook::StatsComputing {
                if let Some(entered) = entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    released.lock().unwrap().recv().unwrap();
                }
            }
        });
        let computing = segments.clone();
        let first = std::thread::spawn(move || computing.refresh_stats(Duration::ZERO).unwrap());
        in_compute.recv_timeout(wait).unwrap();

        let (done, flushed_in_time) = channel();
        let installer = segments.clone();
        segments.add(3, 1, &words(&[("later", 1)]));
        std::thread::spawn(move || {
            installer.flush().unwrap();
            done.send(()).unwrap();
        });
        assert!(
            flushed_in_time.recv_timeout(wait).is_ok(),
            "an install doesn't wait for the computation"
        );
        let (sent, stale) = channel();
        let caller = segments.clone();
        std::thread::spawn(move || sent.send(caller.term_stats(1).unwrap()).unwrap());
        assert_eq!(
            stale.recv_timeout(wait),
            Ok((1, vec![("common".to_string(), 1)])),
            "another caller gets the stale value at once"
        );
        release.send(()).unwrap();
        first.join().unwrap();
        // invalidated during that computation: the next refresh counts again
        assert_eq!(segments.term_stats(0).unwrap().0, 2);
        segments.refresh_stats(Duration::ZERO).unwrap();
        assert_eq!(segments.term_stats(0).unwrap().0, 3);
    }

    #[test]
    fn picking_and_pruning_dont_scan_every_dead_id() {
        let segments = Segments::open(&temp_dir("segments-dead-scale")).unwrap();
        // 200k pages over ten segments, 150k of them dead and still held
        for part in 0..10u64 {
            for id in part * 20_000 + 1..=(part + 1) * 20_000 {
                segments.add(id, 1, &words(&[("common", 1)]));
            }
            segments.flush().unwrap();
        }
        let dead: Vec<u64> = (1..=200_000).filter(|id| id % 4 != 0).collect();
        segments.set_dead(&dead);
        // what every round of upkeep does, with nothing to prune and a segment 75% dead
        let start = Instant::now();
        for _ in 0..10 {
            assert!(segments.prune_dead().is_empty());
            assert_eq!(
                segments.pick_merge(true).map(|picked| picked.0.len()),
                Some(1)
            );
        }
        let took = start.elapsed();
        println!(
            "ten rounds of prune and pick with 150k dead ids: {:?}",
            took
        );
        assert!(
            took < Duration::from_millis(50),
            "ten rounds took {:?}",
            took
        );
        // every segment is 75% dead: each is rewritten, and once the old files go, so do the ids
        let start = Instant::now();
        assert_eq!(maintained(&segments), dead);
        println!("rewriting them and pruning 150k ids: {:?}", start.elapsed());
        assert_eq!(segments.postings("common").unwrap().len(), 50_000);
        assert!(segments.dead.read().unwrap().ids.is_empty());
    }

    #[test]
    fn a_takedown_during_a_merge_cascade_is_erased_before_the_next_merge() {
        let dir = temp_dir("segments-cascade-erase");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=20 {
            let term = if id == 15 { "zzqxcascadeterm" } else { "alpha" };
            flushed(&segments, &[(id, &[(term, 1)])]);
        }
        // before each merge: whether the term is still on disk; the takedown comes at the first
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (log, at_dir) = (seen.clone(), dir.clone());
        segments.set_hook(move |segments, at| {
            if at != Hook::BeforeMerge {
                return;
            }
            let mut log = log.lock().unwrap();
            if log.is_empty() {
                segments.set_dead(&[15]);
                segments.mark_erase(&[15]);
            }
            log.push(dir_has(&at_dir, "zzqxcascadeterm"));
        });
        assert_eq!(maintained(&segments), vec![15]);
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.len() >= 2,
            "a cascade of two merges or more: {:?}",
            seen
        );
        assert!(
            seen[1..].iter().all(|&on_disk| !on_disk),
            "erased before the second merge started: {:?}",
            seen
        );
    }

    #[test]
    fn a_flush_during_a_merge_survives_the_merges_install() {
        let dir = temp_dir("segments-flush-mid-merge");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=10 {
            flushed(&segments, &[(id, &[("common", 1)])]);
        }
        let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        segments.set_hook(move |segments, at| {
            if at == Hook::BeforeInstall && !once.swap(true, Ordering::SeqCst) {
                // the merge is written but not installed: a flush gets in first
                segments.add(11, 1, &words(&[("zzqxlateterm", 1)]));
                segments.flush().unwrap();
            }
        });
        maintained(&segments);
        assert_eq!(segments.postings("zzqxlateterm").unwrap(), vec![(11, 1, 1)]);
        assert_eq!(
            segments.segment_count(),
            2,
            "the merged one and the flushed one"
        );
        assert_eq!(segments.postings("common").unwrap().len(), 10);
        drop(segments);
        let reopened = Segments::open(&dir).unwrap();
        assert_eq!(reopened.flushed_through(), 11);
        assert_eq!(reopened.postings("zzqxlateterm").unwrap(), vec![(11, 1, 1)]);
    }

    #[test]
    fn a_merge_whose_input_was_replaced_meanwhile_is_discarded() {
        let dir = temp_dir("segments-merge-replaced");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=10 {
            let term = if id == 4 {
                "zzqxreplacedterm"
            } else {
                "common"
            };
            flushed(&segments, &[(id, &[(term, 1)])]);
        }
        let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        segments.set_hook(move |segments, at| {
            if at == Hook::BeforeInstall && !once.swap(true, Ordering::SeqCst) {
                // a takedown's rewrite replaces one of the merge's inputs first
                segments.set_dead(&[4]);
                segments.mark_erase(&[4]);
                let _writes = lock(&segments.writes);
                segments.erase_pending().unwrap();
            }
        });
        assert_eq!(maintained(&segments), vec![4]);
        assert!(
            !dir_has(&dir, "zzqxreplacedterm"),
            "neither the merge kept it"
        );
        assert_eq!(segments.postings("common").unwrap().len(), 9);
        let files = fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "seg")
            })
            .count();
        assert_eq!(
            files,
            segments.segment_count(),
            "the discarded output was erased"
        );
        drop(segments);
        assert_eq!(
            Segments::open(&dir)
                .unwrap()
                .postings("common")
                .unwrap()
                .len(),
            9
        );
    }

    #[test]
    fn pages_that_die_during_a_merge_count_toward_its_output() {
        let segments = Segments::open(&temp_dir("segments-died-mid-merge")).unwrap();
        for id in 1..=10 {
            flushed(&segments, &[(id, &[("common", 1)])]);
        }
        let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        segments.set_hook(move |segments, at| {
            if at == Hook::BeforeInstall && !once.swap(true, Ordering::SeqCst) {
                // after the merge read the dead set, so it kept them
                segments.set_dead(&[1, 2, 3]);
            }
        });
        // 3 of its 10 pages are dead, so the merge's output is rewritten on its own
        assert_eq!(maintained(&segments), vec![1, 2, 3]);
        assert_eq!(segments.segment_count(), 1);
        assert_eq!(segments.snapshot()[0].info().docs, 7);
        assert_eq!(segments.postings("common").unwrap().len(), 7);
    }

    #[test]
    fn segments_of_a_gibibyte_or_more_are_never_merged() {
        const MIB: u64 = 1 << 20;
        assert_eq!(pick_tier(&[MIB; 9]), None);
        assert_eq!(pick_tier(&[MIB; 11]), Some((0..10).collect()));
        // tier 2 (100 MiB up) still merges; the output can reach ~10 GiB
        assert_eq!(pick_tier(&[200 * MIB; 10]), Some((0..10).collect()));
        assert_eq!(pick_tier(&[1024 * MIB; 10]), None);
        assert_eq!(pick_tier(&[5000 * MIB; 30]), None);
        // the smallest due tier first
        let mut mixed = vec![200 * MIB; 10];
        mixed.extend([MIB; 10]);
        assert_eq!(pick_tier(&mixed), Some((10..20).collect()));
    }

    #[test]
    fn pruned_ids_are_returned_even_when_a_step_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("segments-prune-on-error");
        let segments = Segments::open(&dir).unwrap();
        for id in 1..=6 {
            segments.add(id, 1, &words(&[("alpha", 1)]));
        }
        segments.flush().unwrap();
        let file = segments.snapshot()[0].path().to_path_buf();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        if OpenOptions::new().write(true).open(&file).is_ok() {
            return; // root ignores the permission; the erase can't be made to fail
        }
        segments.set_dead(&[3]);
        segments.mark_erase(&[3]);
        segments.add(7, 1, &words(&[("beta", 1)]));
        segments.set_dead(&[7]); // only ever buffered: nothing holds it
                                 // one round of the upkeep thread's: one try at erasing each retired file
        let done = segments.upkeep();
        assert!(done.error.is_some(), "the old file can't be erased");
        assert_eq!(done.pruned, vec![7]);
        assert_eq!(lock(&segments.retired).len(), 1, "retried");

        // after ERASE_TRIES failures it's given up on, but its page stays dead
        for _ in 1..ERASE_TRIES {
            assert!(segments.upkeep().error.is_some());
        }
        assert!(lock(&segments.retired).is_empty());
        let done = segments.upkeep();
        assert!(done.error.is_none(), "not retried");
        assert!(done.pruned.is_empty());
        assert!(segments.dead.read().unwrap().ids.contains(&3));
        // nor does a page dying later free a page id the abandoned file holds
        segments.set_dead(&[5]);
        segments.mark_erase(&[5]);
        maintained(&segments);
        assert!(segments.dead.read().unwrap().ids.contains(&5));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        drop(segments);
        Segments::open(&dir).unwrap();
        assert!(!file.exists(), "erased at the next start");
    }

    #[test]
    fn a_panicked_round_doesnt_stop_the_next_or_the_flush() {
        let dir = temp_dir("segments-panic");
        let segments = Arc::new(Segments::open(&dir).unwrap());
        for id in 1..=10 {
            flushed(&segments, &[(id, &[("common", 1)])]);
        }
        let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        segments.set_hook(move |_, at| {
            if at == Hook::BeforeInstall && !once.swap(true, Ordering::SeqCst) {
                panic!("a bug mid-merge");
            }
        });
        // poisons the merging lock, as a panic on the merge thread would
        let merging = segments.clone();
        assert!(std::thread::spawn(move || merging.merge()).join().is_err());
        assert!(segments.merging.is_poisoned());
        segments.add(11, 1, &words(&[("zzqxshutdownterm", 1)]));
        segments.flush().unwrap();
        assert_eq!(maintained(&segments), Vec::<u64>::new());
        assert_eq!(
            segments.segment_count(),
            2,
            "merged this time, and the flush kept"
        );
        assert_eq!(segments.postings("common").unwrap().len(), 10);
        assert!(
            segments.dead.read().unwrap().pins.is_empty(),
            "unpinned while unwinding"
        );
    }

    #[test]
    fn a_merge_stops_when_a_takedown_replaces_an_input_under_it() {
        let dir = temp_dir("segments-merge-cancel");
        let segments = Segments::open(&dir).unwrap();
        // ten segments of 500 pages: the merge checks its inputs after 4096 pages
        for part in 0..10u64 {
            for id in part * 500 + 1..=(part + 1) * 500 {
                let term = if id == 2 { "zzqxcancelterm" } else { "common" };
                segments.add(id, 1, &words(&[(term, 1)]));
            }
            segments.flush().unwrap();
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let (log, at_dir) = (events.clone(), dir.clone());
        segments.set_hook(move |segments, at| {
            let mut log = log.lock().unwrap();
            match at {
                Hook::Checking if log.is_empty() => {
                    log.push("checking".to_string());
                    drop(log);
                    // the upkeep thread erases a taken-down page mid-merge
                    segments.set_dead(&[2]);
                    segments.mark_erase(&[2]);
                    let _writes = lock(&segments.writes);
                    segments.erase_pending().unwrap();
                }
                Hook::BeforeInstall => log.push(format!(
                    "install, on disk: {}",
                    dir_has(&at_dir, "zzqxcancelterm")
                )),
                _ => {}
            }
        });
        assert_eq!(maintained(&segments), vec![2]);
        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                "checking",
                // the erase rewrite's own install (its old file is still held by the merge)
                "install, on disk: true",
                // the merge stopped, the old file went, then the merge ran again
                "install, on disk: false",
            ]
        );
        assert_eq!(segments.segment_count(), 1);
        assert_eq!(segments.postings("common").unwrap().len(), 4999);
    }

    #[test]
    fn a_discarded_merge_output_is_erased_after_the_install_lock_is_released() {
        let segments = Segments::open(&temp_dir("segments-discard-unlocked")).unwrap();
        for id in 1..=10 {
            flushed(&segments, &[(id, &[("common", 1)])]);
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (log, once) = (
            seen.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        segments.set_hook(move |segments, at| match at {
            Hook::BeforeInstall if !once.swap(true, Ordering::SeqCst) => {
                // an erase rewrite replaces an input, so the merge's output is discarded
                segments.set_dead(&[4]);
                segments.mark_erase(&[4]);
                let _writes = lock(&segments.writes);
                segments.erase_pending().unwrap();
            }
            // zeroing it mustn't hold up another install, such as a flush
            Hook::Discarding => log
                .lock()
                .unwrap()
                .push(segments.install.try_lock().is_ok()),
            _ => {}
        });
        assert_eq!(maintained(&segments), vec![4]);
        assert_eq!(*seen.lock().unwrap(), vec![true]);
    }

    #[test]
    fn a_rewrite_of_an_input_replaced_before_it_started_writes_nothing() {
        let segments = Segments::open(&temp_dir("segments-stale-pick")).unwrap();
        for id in 1..=6 {
            segments.add(id, 1, &words(&[("alpha", 1)]));
        }
        segments.flush().unwrap();
        // picked, then replaced by a takedown's rewrite before the rewrite began
        let picked = segments.snapshot().to_vec();
        segments.set_dead(&[3]);
        segments.mark_erase(&[3]);
        segments.upkeep();
        let generation = segments.next_gen.load(Ordering::SeqCst);
        assert!(!segments.rewrite(&picked).unwrap());
        assert_eq!(
            segments.next_gen.load(Ordering::SeqCst),
            generation,
            "no file written"
        );
    }

    #[test]
    fn an_erase_mark_for_an_id_already_pruned_is_dropped() {
        let segments = Segments::open(&temp_dir("segments-stale-mark")).unwrap();
        segments.add(1, 1, &words(&[("alpha", 1)]));
        segments.set_dead(&[1]);
        assert_eq!(maintained(&segments), vec![1]);
        // a purge or expiry marking it from a dead_pages row not deleted yet
        segments.mark_erase(&[1]);
        maintained(&segments);
        assert!(lock(&segments.erase).is_empty());
    }

    #[test]
    fn retiring_doesnt_hold_up_flushes() {
        let segments = Segments::open(&temp_dir("segments-retire-unlocked")).unwrap();
        for id in 1..=6 {
            segments.add(id, 1, &words(&[("alpha", 1)]));
        }
        segments.flush().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        segments.set_hook(move |segments, at| {
            if at == Hook::Retiring {
                log.lock().unwrap().push(segments.writes.try_lock().is_ok());
            }
        });
        segments.set_dead(&[3]);
        segments.mark_erase(&[3]);
        let done = segments.upkeep();
        assert!(done.error.is_none());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![true],
            "erasing it, writes is free"
        );
    }

    // the segment files in dir, by name
    pub(crate) fn segment_files(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "seg"))
            .collect();
        files.sort();
        files
    }

    #[test]
    fn opening_skips_checksums_and_verify_opened_checks_each_file_then_none() {
        let dir = temp_dir("segments-verify");
        let segments = Segments::open(&dir).unwrap();
        flushed(&segments, &[(1, &[("apple", 1)]), (2, &[("zebra", 1)])]);
        assert_eq!(segments.verify_opened().unwrap(), (0, 0), "written here");
        drop(segments);
        let files = segment_files(&dir);
        crate::segment::damage_postings(&files[1]);

        let segments = Segments::open(&dir).expect("opening doesn't read the postings");
        assert_eq!(segments.postings("apple").unwrap(), vec![(1, 1, 1)]);
        let locked = Arc::new(Mutex::new(Vec::new()));
        let seen = locked.clone();
        segments.set_hook(move |segments, at| {
            if at == Hook::Verified {
                seen.lock()
                    .unwrap()
                    .push(segments.state.try_write().is_ok());
            }
        });
        let err = segments.verify_opened().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let name = files[1].file_name().unwrap().to_string_lossy().into_owned();
        assert!(err.to_string().contains(&name), "{}", err);
        assert!(
            lock(&locked).iter().all(|&free| free),
            "files are read without the state lock"
        );
        assert_eq!(
            segments.postings("apple").unwrap(),
            vec![(1, 1, 1)],
            "still served"
        );
        assert_eq!(segments.verify_opened().unwrap(), (0, 0), "checked once");
        assert!(
            Segments::open_verified(&dir).is_err(),
            "a verifying open fails it"
        );

        // sound files check out, counted
        let dir = temp_dir("segments-verify-ok");
        let segments = Segments::open(&dir).unwrap();
        flushed(&segments, &[(1, &[("apple", 1)]), (2, &[("zebra", 1)])]);
        drop(segments);
        let bytes_ok: u64 = segment_files(&dir)
            .iter()
            .map(|f| fs::metadata(f).unwrap().len())
            .sum();
        assert_eq!(
            Segments::open(&dir).unwrap().verify_opened().unwrap(),
            (2, bytes_ok)
        );
    }

    #[test]
    fn an_upgrade_of_a_damaged_unchecked_file_fails_as_corrupt_and_installs_nothing() {
        let dir = temp_dir("segments-upgrade-damaged");
        let segments = Segments::open(&dir).unwrap();
        segments.set_write_version(1);
        segments.add(1, 1, &words(&[("alpha", 1)]));
        segments.flush().unwrap();
        drop(segments);
        let files = segment_files(&dir);
        crate::segment::damage_postings(&files[0]);

        let segments = Segments::open(&dir).unwrap();
        let err = segments.merge().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{}", err);
        assert_eq!(segments.snapshot()[0].path(), files[0]);
        assert_eq!(segment_files(&dir), files, "no output left");
    }

    #[test]
    fn a_damaged_file_that_leaves_the_live_set_during_the_check_is_skipped() {
        let dir = temp_dir("segments-verify-gone");
        let segments = Segments::open(&dir).unwrap();
        flushed(&segments, &[(1, &[("apple", 1)]), (2, &[("zebra", 1)])]);
        drop(segments);
        let files = segment_files(&dir);
        for file in &files {
            crate::segment::damage_postings(file);
        }
        let segments = Segments::open(&dir).unwrap();
        // the first left before the check reached it, the second while it was read
        segments.remove_for_test(&files[0]);
        let second = files[1].clone();
        segments.set_hook(move |segments, at| {
            if at == Hook::Verified {
                segments.remove_for_test(&second);
            }
        });
        assert_eq!(segments.verify_opened().unwrap(), (0, 0));
    }
}
