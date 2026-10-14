/*!
 * The segment set (#247): the segment files named by MANIFEST, a write buffer of recent
 * postings, and the dead page ids that search filters out (#249). maintain(), run on a background
 * thread, flushes the buffer to a new segment, rewrites segments holding pages to erase, merges
 * segments, and zeroes and removes retired files once no search still reads them.
 */
use crate::segment::{Posting, Segment, SegmentWriter};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

const FLUSH_POSTINGS: usize = 1_000_000;
const FLUSH_AGE: Duration = Duration::from_secs(60);

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

pub struct Segments {
    // None in memory: the buffer holds everything and is never flushed
    dir: Option<PathBuf>,
    // lock order: buffer, then state, then dead
    buffer: RwLock<Buffer>,
    state: RwLock<State>,
    dead: RwLock<HashSet<u64>>,
    // dead ids whose segments must be rewritten now (#249)
    erase: Mutex<HashSet<u64>>,
    next_gen: AtomicU64,
    flushed_through: AtomicU64,
    // replaced segments, erased once no search holds them
    retired: Mutex<Vec<Arc<Segment>>>,
    // one maintain() (or bulk_load) at a time
    maintenance: Mutex<()>,
    woken: Mutex<bool>,
    wake: Condvar,
    flush_postings: usize,
    flush_age: Duration,
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
            dead: RwLock::new(HashSet::new()),
            erase: Mutex::new(HashSet::new()),
            next_gen: AtomicU64::new(1),
            flushed_through: AtomicU64::new(0),
            retired: Mutex::new(Vec::new()),
            maintenance: Mutex::new(()),
            woken: Mutex::new(false),
            wake: Condvar::new(),
            flush_postings: FLUSH_POSTINGS,
            flush_age: FLUSH_AGE,
        }
    }

    /// Opens (or creates) the segments in `dir`; files the manifest doesn't name, left by a crash
    /// mid-write, are erased
    pub fn open(dir: &Path) -> io::Result<Segments> {
        fs::create_dir_all(dir)?;
        let (next_gen, flushed_through, names) = read_manifest(dir)?.unwrap_or((1, 0, Vec::new()));
        let mut live = Vec::new();
        for name in &names {
            live.push(Arc::new(Segment::open(&dir.join(name))?));
        }
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
        segments.state.write().unwrap().segments = Arc::new(live);
        Ok(segments)
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
            self.wake();
        }
    }

    /// The live pages a term is on, as (id, count, page length)
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
        found.retain(|(id, _, _)| !dead.contains(id));
        Ok(found)
    }

    /// Pages search must no longer return: hidden at once, and dropped from the buffer
    pub fn set_dead(&self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        self.dead.write().unwrap().extend(ids);
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
        self.erase.lock().unwrap().extend(ids);
        self.wake();
    }

    fn wake(&self) {
        *self.woken.lock().unwrap() = true;
        self.wake.notify_one();
    }

    /// Blocks until there's work (a full buffer, an erase) or a timeout; for the upkeep thread
    pub fn wait(&self) {
        // retired files are polled for until no search holds them
        let timeout = if self.retired.lock().unwrap().is_empty() {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(100)
        };
        let woken = self.woken.lock().unwrap();
        let (mut woken, _) = self
            .wake
            .wait_timeout_while(woken, timeout, |woken| !*woken)
            .unwrap();
        *woken = false;
    }

    /// Writes the buffer to a segment now (a clean shutdown, tests)
    pub fn flush(&self) -> io::Result<()> {
        let _maintenance = self.maintenance.lock().unwrap();
        self.flush_locked()
    }

    /**
     * One round of upkeep: flush the buffer if it's full or old. Returns the dead ids that no
     * segment (or buffer) holds any more, whose records the caller can drop.
     */
    pub fn maintain(&self) -> io::Result<Vec<u64>> {
        let _maintenance = self.maintenance.lock().unwrap();
        if self.flush_due() {
            self.flush_locked()?;
        }
        Ok(self.prune_dead())
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
        let dead = self.dead.read().unwrap().clone();
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
        let mut live: Vec<Arc<Segment>> = self.state.read().unwrap().segments.to_vec();
        let added = segment.clone();
        live.extend(segment);
        let flushed_through = self.flushed_through().max(frozen.max_id);
        match self.install(live, flushed_through, true) {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Some(added) = added {
                    discard(added);
                }
                Err(err)
            }
        }
    }

    // writes a new segment file; a failed write's partial file is erased. None if it has no pages.
    fn write_segment(
        &self,
        fill: impl FnOnce(&mut SegmentWriter) -> io::Result<()>,
    ) -> io::Result<Option<Arc<Segment>>> {
        let dir = self.dir.as_ref().expect("segment files need a directory");
        let generation = self.next_gen.fetch_add(1, Ordering::SeqCst);
        let path = dir.join(format!("{:016x}.seg", generation));
        let written = SegmentWriter::create(&path).and_then(|mut writer| {
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
        match Segment::open(&path) {
            Ok(segment) => Ok(Some(Arc::new(segment))),
            Err(err) => {
                let _ = erase_file(&path);
                Err(err)
            }
        }
    }

    // makes `live` the segment list: the manifest first, then what searches see
    fn install(
        &self,
        mut live: Vec<Arc<Segment>>,
        flushed_through: u64,
        clear_frozen: bool,
    ) -> io::Result<()> {
        live.sort_by_key(|segment| segment.info().min_id);
        let dir = self.dir.as_ref().expect("segment files need a directory");
        write_manifest(
            dir,
            self.next_gen.load(Ordering::SeqCst),
            flushed_through,
            &live,
        )?;
        self.flushed_through
            .store(flushed_through, Ordering::SeqCst);
        let mut state = self.state.write().unwrap();
        state.segments = Arc::new(live);
        if clear_frozen {
            state.frozen = None;
        }
        Ok(())
    }

    // drops dead ids that nothing holds any more, and returns them
    fn prune_dead(&self) -> Vec<u64> {
        let buffer = self.buffer.read().unwrap();
        let state = self.state.read().unwrap().clone();
        let mut dead = self.dead.write().unwrap();
        let gone: Vec<u64> = dead
            .iter()
            .copied()
            .filter(|&id| {
                !buffer.lengths.contains_key(&id)
                    && !state
                        .frozen
                        .as_ref()
                        .is_some_and(|f| f.lengths.contains_key(&id))
                    && !state.segments.iter().any(|s| s.contains(id))
            })
            .collect();
        for id in &gone {
            dead.remove(id);
        }
        gone
    }

    /// Replaces every segment with one written by `fill`, for the #247 migration. What was
    /// there (from an interrupted earlier attempt) is erased.
    /// Only at startup, before any search can hold a segment: the old files are erased at once,
    /// not retired, so a search holding one would read zeros.
    pub fn bulk_load(
        &self,
        flushed_through: u64,
        fill: impl FnOnce(&mut SegmentWriter) -> io::Result<()>,
    ) -> io::Result<()> {
        let _maintenance = self.maintenance.lock().unwrap();
        let old: Vec<Arc<Segment>> = self.state.read().unwrap().segments.to_vec();
        let segment = self.write_segment(fill)?;
        let added = segment.clone();
        if let Err(err) = self.install(segment.into_iter().collect(), flushed_through, false) {
            if let Some(added) = added {
                discard(added);
            }
            return Err(err);
        }
        for segment in old {
            let path = segment.path().to_path_buf();
            drop(segment);
            erase_file(&path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
impl Segments {
    pub(crate) fn segment_count(&self) -> usize {
        self.state.read().unwrap().segments.len()
    }

    pub(crate) fn snapshot(&self) -> Arc<Vec<Arc<Segment>>> {
        self.state.read().unwrap().segments.clone()
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
            segments.maintain().unwrap(),
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
        assert!(
            segments.maintain().unwrap().is_empty(),
            "still in a segment"
        );
    }

    #[test]
    fn flushes_when_the_buffer_is_full_or_old() {
        let mut segments = Segments::open(&temp_dir("segments-due")).unwrap();
        segments.set_flush_limits(2, Duration::from_secs(3600));
        segments.add(1, 1, &words(&[("a", 1)]));
        segments.maintain().unwrap();
        assert_eq!(segments.segment_count(), 0, "not due");
        segments.add(2, 1, &words(&[("b", 1)]));
        segments.maintain().unwrap();
        assert_eq!(segments.segment_count(), 1, "full");
        segments.set_flush_limits(1000, Duration::ZERO);
        segments.add(3, 1, &words(&[("c", 1)]));
        segments.maintain().unwrap();
        assert_eq!(segments.segment_count(), 2, "old");
        segments.maintain().unwrap();
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
        segments.maintain().unwrap();
        assert_eq!(segments.segment_count(), 0);
        assert_eq!(segments.postings("a").unwrap(), vec![(1, 1, 1)]);
    }
}
