/*!
 * One immutable segment file of the inverted index (#247). Version 2 layout, integers as LEB128
 * varints unless marked:
 *
 *   header    "ipxseg", version u8
 *   docs      (id delta, length) per page, ascending by id
 *   postings  per term, blocks of up to BLOCK postings: n, last id, body bytes, then (v2) its
 *             impacts: k, and k (count, page length) pairs, count and length each ascending,
 *             such that every posting has at most one's count on a page at least as long; the
 *             most a block's pages can score, for #248's pruning. Then the id deltas, then the
 *             counts. A v1 block has max count where the impacts are, and no lengths.
 *   dict      per term, ascending bytewise: shared prefix, suffix length, suffix, df, offset of
 *             its first block; prefixes restart every SPARSE terms
 *   sparse    every SPARSE-th term: length, term, its dict entry's offset within dict
 *   footer    docs, postings, dict and sparse offsets, page count, min id, max id (u64 LE each),
 *             FNV-1a 64 of everything before it (u64 LE), "ipxend"
 */
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// (page id, how often the term appears on it)
pub type Posting = (u64, u32);

const BLOCK: usize = 128;
const SPARSE: u64 = 64;
const MAGIC: &[u8] = b"ipxseg";
pub(crate) const VERSION: u8 = 2;
// a v2 block's (count, length) pairs at most: the rest are merged into these
const IMPACTS_MAX: usize = 8;
// a block header is at most this many bytes: four varints, then two per impact
const HEAD_MAX: u64 = (4 + 2 * IMPACTS_MAX as u64) * 10;
// how much a cursor reads at once
const WINDOW: u64 = 64 * 1024;

// blocks decoded by cursors on this thread, so tests can see what pruning skipped
#[cfg(test)]
thread_local! {
    pub(crate) static DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // window reads for a range that began inside the window but ran past its end
    pub(crate) static STRADDLED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
const END: &[u8] = b"ipxend";
const FOOTER: u64 = 7 * 8 + 8 + 6;
const FNV_OFFSET: u64 = 0xcbf29ce484222325;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentInfo {
    pub docs: u64,
    pub min_id: u64,
    pub max_id: u64,
    pub bytes: u64,
}

fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "corrupt segment file")
}

fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(hash, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

// FNV-1a 64 of a segment file of `bytes` bytes, up to the footer's checksum: the whole file
// read, a chunk at a time
fn checksum(file: &File, bytes: u64) -> io::Result<u64> {
    let checked = bytes - 8 - END.len() as u64;
    let mut hash = FNV_OFFSET;
    let mut chunk = vec![0u8; 1 << 20];
    let mut offset = 0;
    while offset < checked {
        let n = (checked - offset).min(chunk.len() as u64) as usize;
        file.read_exact_at(&mut chunk[..n], offset)?;
        hash = fnv1a(hash, &chunk[..n]);
        offset += n as u64;
    }
    Ok(hash)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn get_varint(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *buf.get(*pos).ok_or_else(corrupt)?;
        *pos += 1;
        if shift > 63 {
            return Err(corrupt());
        }
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn read_varint(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let mut byte = [0u8];
        reader.read_exact(&mut byte)?;
        if shift > 63 {
            return Err(corrupt());
        }
        value |= ((byte[0] & 0x7f) as u64) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn read_at(file: &File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; len as usize];
    file.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

/**
 * A block's impacts (#248): its postings' (count, page length) pairs that no other beats on both
 * (a count at least as high on a page at most as long), count and length ascending. Past
 * IMPACTS_MAX, runs of neighbours are merged into (their highest count, their shortest page):
 * looser, but still at least every posting it stands for.
 */
fn impacts(mut pairs: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    // highest count first, then shortest page: each pair kept is shorter than all before it
    pairs.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut frontier: Vec<(u32, u32)> = Vec::new();
    for pair in pairs {
        if frontier.last().is_none_or(|last| pair.1 < last.1) {
            frontier.push(pair);
        }
    }
    frontier.reverse();
    if frontier.len() <= IMPACTS_MAX {
        return frontier;
    }
    // IMPACTS_MAX runs, as even as can be; in each run, the last count is the highest and the
    // first length the shortest
    let (n, runs) = (frontier.len(), IMPACTS_MAX);
    (0..runs)
        .map(|r| {
            let (start, end) = (r * n / runs, (r + 1) * n / runs);
            (frontier[end - 1].0, frontier[start].1)
        })
        .collect()
}

// a block's impacts from its header; a v1 block's one: (max count, the segment's shortest page)
fn read_impacts(head: &[u8], pos: &mut usize, version: u8, shortest: u32) -> io::Result<Impacts> {
    let mut impacts = Impacts {
        k: 1,
        pairs: [(0, 0); IMPACTS_MAX],
    };
    if version == 1 {
        impacts.pairs[0] = (get_varint(head, pos)? as u32, shortest);
        return Ok(impacts);
    }
    impacts.k = get_varint(head, pos)? as usize;
    if impacts.k == 0 || impacts.k > IMPACTS_MAX {
        return Err(corrupt());
    }
    for pair in impacts.pairs[..impacts.k].iter_mut() {
        *pair = (get_varint(head, pos)? as u32, get_varint(head, pos)? as u32);
    }
    Ok(impacts)
}

// a block's (count, page length) pairs, the first k used
#[derive(Clone, Copy)]
struct Impacts {
    k: usize,
    pairs: [(u32, u32); IMPACTS_MAX],
}

impl Impacts {
    fn pairs(&self) -> &[(u32, u32)] {
        &self.pairs[..self.k]
    }
}

pub struct SegmentWriter {
    out: BufWriter<File>,
    pos: u64,
    hash: u64,
    docs: u64,
    min_id: u64,
    max_id: u64,
    postings_off: Option<u64>,
    dict: Vec<u8>,
    sparse: Vec<u8>,
    terms: u64,
    last_term: Vec<u8>,
    version: u8,
    // the pages written so far and their lengths, for each block's impacts (v2)
    ids: Vec<u64>,
    lengths: Vec<u32>,
}

impl SegmentWriter {
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

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.hash = fnv1a(self.hash, bytes);
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// A page and its length; ids ascending, all before the first term
    pub fn add_doc(&mut self, id: u64, length: u32) -> io::Result<()> {
        assert!(self.postings_off.is_none(), "pages come before terms");
        assert!(self.docs == 0 || id > self.max_id, "page ids must ascend");
        let mut buf = Vec::new();
        put_varint(&mut buf, id - if self.docs == 0 { 0 } else { self.max_id });
        put_varint(&mut buf, length as u64);
        self.write(&buf)?;
        self.ids.push(id);
        self.lengths.push(length);
        if self.docs == 0 {
            self.min_id = id;
        }
        self.max_id = id;
        self.docs += 1;
        Ok(())
    }

    /// A term and the pages it's on; terms ascending bytewise, postings ascending by id
    pub fn add_term(&mut self, term: &[u8], postings: &[Posting]) -> io::Result<()> {
        assert!(!postings.is_empty(), "a term is on at least one page");
        assert!(
            self.terms == 0 || term > self.last_term.as_slice(),
            "terms must ascend"
        );
        if self.postings_off.is_none() {
            self.postings_off = Some(self.pos);
        }
        let offset = self.pos;
        let mut prev = 0u64;
        for block in postings.chunks(BLOCK) {
            let mut body = Vec::new();
            for &(id, _) in block {
                put_varint(&mut body, id - prev);
                prev = id;
            }
            for &(_, count) in block {
                put_varint(&mut body, count as u64);
            }
            let mut head = Vec::new();
            put_varint(&mut head, block.len() as u64);
            put_varint(&mut head, prev);
            put_varint(&mut head, body.len() as u64);
            if self.version >= 2 {
                // a posting whose page wasn't added gets 0: the loosest bound, never a wrong one
                let pairs: Vec<(u32, u32)> = block
                    .iter()
                    .map(|&(id, count)| match self.ids.binary_search(&id) {
                        Ok(i) => (count, self.lengths[i]),
                        Err(_) => (count, 0),
                    })
                    .collect();
                let impacts = impacts(pairs);
                put_varint(&mut head, impacts.len() as u64);
                for (count, length) in impacts {
                    put_varint(&mut head, count as u64);
                    put_varint(&mut head, length as u64);
                }
            } else {
                put_varint(&mut head, block.iter().map(|p| p.1).max().unwrap() as u64);
            }
            self.write(&head)?;
            self.write(&body)?;
        }
        // every SPARSE-th entry is written whole and indexed, so a lookup can start there
        let shared = if self.terms % SPARSE == 0 {
            put_varint(&mut self.sparse, term.len() as u64);
            self.sparse.extend_from_slice(term);
            put_varint(&mut self.sparse, self.dict.len() as u64);
            0
        } else {
            self.last_term
                .iter()
                .zip(term)
                .take_while(|(a, b)| a == b)
                .count()
        };
        put_varint(&mut self.dict, shared as u64);
        put_varint(&mut self.dict, (term.len() - shared) as u64);
        self.dict.extend_from_slice(&term[shared..]);
        put_varint(&mut self.dict, postings.len() as u64);
        put_varint(&mut self.dict, offset);
        self.last_term = term.to_vec();
        self.terms += 1;
        Ok(())
    }

    /// Writes the dictionary and footer and fsyncs the file
    pub fn finish(mut self) -> io::Result<SegmentInfo> {
        let docs_off = (MAGIC.len() + 1) as u64;
        let postings_off = self.postings_off.unwrap_or(self.pos);
        let dict_off = self.pos;
        let dict = std::mem::take(&mut self.dict);
        self.write(&dict)?;
        let sparse_off = self.pos;
        let sparse = std::mem::take(&mut self.sparse);
        self.write(&sparse)?;
        let mut footer = Vec::new();
        for value in [
            docs_off,
            postings_off,
            dict_off,
            sparse_off,
            self.docs,
            self.min_id,
            self.max_id,
        ] {
            footer.extend_from_slice(&value.to_le_bytes());
        }
        self.write(&footer)?;
        let hash = self.hash;
        self.write(&hash.to_le_bytes())?;
        self.write(END)?;
        let file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        Ok(SegmentInfo {
            docs: self.docs,
            min_id: self.min_id,
            max_id: self.max_id,
            bytes: self.pos,
        })
    }
}

/// An open segment file. Page ids and lengths and the sparse term index are held in memory;
/// dictionary entries and postings are read on demand.
pub struct Segment {
    file: File,
    path: PathBuf,
    info: SegmentInfo,
    dict_off: u64,
    sparse_off: u64,
    // every SPARSE-th term and the absolute offset of its dict entry
    sparse: Vec<(Vec<u8>, u64)>,
    ids: Vec<u64>,
    lengths: Vec<u32>,
    version: u8,
    // the shortest page: a v1 block's bound's length, having no impacts
    shortest: u32,
    // the footer's checksum
    sum: u64,
}

impl Segment {
    /// Opens and verifies (checksum, then structure) a segment file
    pub fn open(path: &Path) -> io::Result<Segment> {
        Segment::read(path, true)
    }

    /// Opens a segment file with its structure checked, but not its checksum, which would
    /// re-read the whole file (#247): one this process just wrote and fsynced, or one opened at
    /// startup, checked by verify after the API is up (v0.5.6)
    pub fn open_written(path: &Path) -> io::Result<Segment> {
        Segment::read(path, false)
    }

    fn read(path: &Path, verify: bool) -> io::Result<Segment> {
        let file = File::open(path)?;
        let bytes = file.metadata()?.len();
        if bytes < MAGIC.len() as u64 + 1 + FOOTER {
            return Err(corrupt());
        }
        let footer = read_at(&file, bytes - FOOTER, FOOTER)?;
        if &footer[footer.len() - END.len()..] != END {
            return Err(corrupt());
        }
        let field = |i: usize| u64::from_le_bytes(footer[i * 8..i * 8 + 8].try_into().unwrap());
        let sum = field(7);
        if verify && checksum(&file, bytes)? != sum {
            return Err(corrupt());
        }
        let header = read_at(&file, 0, MAGIC.len() as u64 + 1)?;
        if &header[..MAGIC.len()] != MAGIC {
            return Err(corrupt());
        }
        let version = header[MAGIC.len()];
        if version != 1 && version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported segment version",
            ));
        }
        let (docs_off, postings_off, dict_off, sparse_off) =
            (field(0), field(1), field(2), field(3));
        if !(docs_off <= postings_off
            && postings_off <= dict_off
            && dict_off <= sparse_off
            && sparse_off <= bytes - FOOTER)
        {
            return Err(corrupt());
        }
        let info = SegmentInfo {
            docs: field(4),
            min_id: field(5),
            max_id: field(6),
            bytes,
        };

        let raw = read_at(&file, docs_off, postings_off - docs_off)?;
        let (mut ids, mut lengths) = (Vec::new(), Vec::new());
        let (mut pos, mut id) = (0, 0u64);
        while pos < raw.len() {
            id += get_varint(&raw, &mut pos)?;
            ids.push(id);
            lengths.push(get_varint(&raw, &mut pos)? as u32);
        }
        if ids.len() as u64 != info.docs {
            return Err(corrupt());
        }

        let shortest = lengths.iter().copied().min().unwrap_or(0);

        let raw = read_at(&file, sparse_off, bytes - FOOTER - sparse_off)?;
        let mut sparse = Vec::new();
        let mut pos = 0;
        while pos < raw.len() {
            let len = get_varint(&raw, &mut pos)? as usize;
            let term = raw.get(pos..pos + len).ok_or_else(corrupt)?.to_vec();
            pos += len;
            sparse.push((term, dict_off + get_varint(&raw, &mut pos)?));
        }
        Ok(Segment {
            file,
            path: path.to_path_buf(),
            info,
            dict_off,
            sparse_off,
            sparse,
            ids,
            lengths,
            version,
            shortest,
            sum,
        })
    }

    /// Checks the checksum open_written skipped, reading the whole file: for the check after
    /// startup (v0.5.6). A mismatch is a corrupt error naming the file.
    pub fn verify(&self) -> io::Result<()> {
        if checksum(&self.file, self.info.bytes)? != self.sum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "corrupt segment file {}: checksum doesn't match",
                    self.path.display()
                ),
            ));
        }
        Ok(())
    }

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

    pub fn info(&self) -> SegmentInfo {
        self.info
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn contains(&self, id: u64) -> bool {
        self.index(id).is_some()
    }

    /// The page's length in terms, if it's in this segment
    pub fn length(&self, id: u64) -> Option<u32> {
        self.index(id).map(|i| self.lengths[i])
    }

    // the range check first: most ids asked about are in another segment
    fn index(&self, id: u64) -> Option<usize> {
        if id < self.info.min_id || id > self.info.max_id {
            return None;
        }
        self.ids.binary_search(&id).ok()
    }

    /// The ids of its pages from `from` to `to`, inclusive
    pub fn ids_between(&self, from: u64, to: u64) -> &[u64] {
        let start = self.ids.partition_point(|&id| id < from);
        let end = self.ids.partition_point(|&id| id <= to);
        &self.ids[start..end.max(start)]
    }

    /// (page id, length), ascending by id
    pub fn docs(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.ids.iter().copied().zip(self.lengths.iter().copied())
    }

    /// The postings for a term, or none if it isn't in this segment
    #[cfg(test)]
    pub fn postings(&self, term: &[u8]) -> io::Result<Vec<Posting>> {
        match self.find(term)? {
            Some((df, offset)) => self.postings_at(df, offset),
            None => Ok(Vec::new()),
        }
    }

    // (df, postings offset) of a term: binary search the sparse index, then scan one chunk
    fn find(&self, term: &[u8]) -> io::Result<Option<(u64, u64)>> {
        let i = match self
            .sparse
            .binary_search_by(|(t, _)| t.as_slice().cmp(term))
        {
            Ok(i) => i,
            Err(0) => return Ok(None),
            Err(i) => i - 1,
        };
        let start = self.sparse[i].1;
        let end = self.sparse.get(i + 1).map_or(self.sparse_off, |s| s.1);
        let chunk = read_at(&self.file, start, end - start)?;
        let (mut pos, mut current) = (0, Vec::new());
        while pos < chunk.len() {
            let shared = get_varint(&chunk, &mut pos)? as usize;
            let len = get_varint(&chunk, &mut pos)? as usize;
            if shared > current.len() {
                return Err(corrupt());
            }
            current.truncate(shared);
            current.extend_from_slice(chunk.get(pos..pos + len).ok_or_else(corrupt)?);
            pos += len;
            let df = get_varint(&chunk, &mut pos)?;
            let offset = get_varint(&chunk, &mut pos)?;
            match current.as_slice().cmp(term) {
                std::cmp::Ordering::Equal => return Ok(Some((df, offset))),
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Less => {}
            }
        }
        Ok(None)
    }

    /// Reads `df` postings starting at a dictionary entry's offset
    pub fn postings_at(&self, df: u64, mut offset: u64) -> io::Result<Vec<Posting>> {
        let mut out = Vec::with_capacity(df as usize);
        let mut prev = 0u64;
        while (out.len() as u64) < df {
            let head = read_at(
                &self.file,
                offset,
                HEAD_MAX.min(self.dict_off.saturating_sub(offset)),
            )?;
            let mut pos = 0;
            let n = get_varint(&head, &mut pos)? as usize;
            let _last = get_varint(&head, &mut pos)?;
            let body_len = get_varint(&head, &mut pos)?;
            read_impacts(&head, &mut pos, self.version, self.shortest)?;
            if n == 0 || n > BLOCK {
                return Err(corrupt());
            }
            let body = read_at(&self.file, offset + pos as u64, body_len)?;
            let mut bpos = 0;
            let mut ids = Vec::with_capacity(n);
            for _ in 0..n {
                prev += get_varint(&body, &mut bpos)?;
                ids.push(prev);
            }
            for id in ids {
                out.push((id, get_varint(&body, &mut bpos)? as u32));
            }
            offset += pos as u64 + body_len;
        }
        Ok(out)
    }

    /// Every term in order, as (term, df, postings offset), streamed from the file
    pub fn terms(&self) -> io::Result<Terms> {
        // its own handle: a cloned one would share (and move) this one's file offset
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.dict_off))?;
        Ok(Terms {
            reader: BufReader::new(file).take(self.sparse_off - self.dict_off),
            term: Vec::new(),
        })
    }
}

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
    impacts: Impacts,
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
            #[cfg(test)]
            if offset >= self.window_at && offset < held {
                STRADDLED.with(|s| s.set(s.get() + 1));
            }
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
        let impacts = read_impacts(head, &mut pos, version, segment_shortest)?;
        if n == 0 || n > BLOCK || last < base || (self.passed > 0 && last == base) {
            return Err(corrupt());
        }
        let body_at = at.checked_add(pos as u64).ok_or_else(corrupt)?;
        self.next = body_at.checked_add(body_len).ok_or_else(corrupt)?;
        self.block = Some(Block {
            n,
            base,
            last,
            impacts,
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

    /// The current block's last id; none once done
    pub fn block_last(&self) -> Option<u64> {
        self.block.as_ref().map(|b| b.last)
    }

    /**
     * The most `weight` comes to for any posting in the current block, given a weight that
     * grows with count and falls with page length: its highest over the block's impacts. 0 once
     * done.
     */
    pub fn block_bound(&self, weight: impl Fn(u32, u32) -> f64) -> f64 {
        self.block.as_ref().map_or(0.0, |b| {
            b.impacts
                .pairs()
                .iter()
                .map(|&(count, length)| weight(count, length))
                .fold(f64::NEG_INFINITY, f64::max)
        })
    }

    /**
     * The most `weight` comes to for a posting in the current block with `count`, given a weight
     * that grows with count and falls with page length (#271): its weight at the shortest length
     * over every impact whose count reaches `count`. It no longer relies on the impacts being
     * sorted, so a garbled header can't make it fall below a weight. Infinite if no impact
     * reaches it, so nothing is skipped on it; 0 once done.
     */
    pub fn count_bound(&self, count: u32, weight: impl Fn(u32, u32) -> f64) -> f64 {
        self.block.as_ref().map_or(0.0, |b| {
            match (b.impacts.pairs().iter())
                .filter(|&&(c, _)| c >= count)
                .map(|&(_, length)| length)
                .min()
            {
                Some(length) => weight(count, length),
                None => f64::INFINITY,
            }
        })
    }

    /// The current block's (count, page length) impacts; none once done
    #[cfg(test)]
    pub fn block_impacts(&self) -> Option<Vec<(u32, u32)>> {
        self.block.as_ref().map(|b| b.impacts.pairs().to_vec())
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
    #[cfg(test)]
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

pub struct Terms {
    reader: io::Take<BufReader<File>>,
    term: Vec<u8>,
}

impl Terms {
    fn entry(&mut self) -> io::Result<(Vec<u8>, u64, u64)> {
        let shared = read_varint(&mut self.reader)? as usize;
        let len = read_varint(&mut self.reader)? as usize;
        if shared > self.term.len() {
            return Err(corrupt());
        }
        self.term.truncate(shared);
        let mut suffix = vec![0; len];
        self.reader.read_exact(&mut suffix)?;
        self.term.extend_from_slice(&suffix);
        let df = read_varint(&mut self.reader)?;
        let offset = read_varint(&mut self.reader)?;
        Ok((self.term.clone(), df, offset))
    }
}

impl Iterator for Terms {
    type Item = io::Result<(Vec<u8>, u64, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.reader.limit() == 0 {
            return None;
        }
        let entry = self.entry();
        if entry.is_err() {
            // stop after an error rather than reading garbage
            self.reader.set_limit(0);
        }
        Some(entry)
    }
}

/// Flips a bit in a file's last posting, which opening doesn't read: only its checksum notices
#[cfg(test)]
pub(crate) fn damage_postings(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let footer = bytes.len() - FOOTER as usize;
    let field = |i: usize| {
        u64::from_le_bytes(
            bytes[footer + i * 8..footer + i * 8 + 8]
                .try_into()
                .unwrap(),
        ) as usize
    };
    let (postings, dict) = (field(1), field(2));
    assert!(postings < dict, "{} has no postings", path.display());
    // a varint's last byte: its low bit changes a count, not how the block decodes
    bytes[dict - 1] ^= 1;
    std::fs::write(path, bytes).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::temp_dir;

    fn write(path: &Path, docs: &[(u64, u32)], terms: &[(&str, Vec<Posting>)]) -> SegmentInfo {
        let mut writer = SegmentWriter::create(path).unwrap();
        for &(id, length) in docs {
            writer.add_doc(id, length).unwrap();
        }
        for (term, postings) in terms {
            writer.add_term(term.as_bytes(), postings).unwrap();
        }
        writer.finish().unwrap()
    }

    #[test]
    fn round_trips_docs_terms_and_postings() {
        let path = temp_dir("segment").join("1.seg");
        // 200 terms: crosses several sparse-index restarts; shared prefixes; one multi-block list
        let long: Vec<Posting> = (1..=300).map(|id| (id, (id % 7 + 1) as u32)).collect();
        let mut terms: Vec<(String, Vec<Posting>)> = (0..200)
            .map(|i| (format!("term{:04}", i), vec![(i as u64 + 1, 2)]))
            .collect();
        terms.push(("alpha".into(), vec![(1, 2), (300, 1)]));
        terms.push(("alphabet".into(), vec![(5, 1)]));
        terms.push(("beta".into(), long.clone()));
        // non-ASCII sorts bytewise, as BTreeMap<String> and SQLite's BINARY collation do
        terms.push(("café".into(), vec![(5, 3)]));
        terms.push(("日本".into(), vec![(300, 4)]));
        terms.sort();
        let borrowed: Vec<(&str, Vec<Posting>)> =
            terms.iter().map(|(t, p)| (t.as_str(), p.clone())).collect();
        let docs: Vec<(u64, u32)> = (1..=300).map(|id| (id, id as u32 * 2)).collect();
        let info = write(&path, &docs, &borrowed);
        assert_eq!((info.docs, info.min_id, info.max_id), (300, 1, 300));

        let segment = Segment::open(&path).unwrap();
        assert_eq!(segment.info(), info);
        for (term, postings) in &terms {
            assert_eq!(
                &segment.postings(term.as_bytes()).unwrap(),
                postings,
                "{}",
                term
            );
        }
        for absent in ["", "aaa", "alph", "alphabets", "term0000x", "zzz", "日"] {
            assert!(
                segment.postings(absent.as_bytes()).unwrap().is_empty(),
                "{}",
                absent
            );
        }
        let listed: Vec<(Vec<u8>, u64)> = segment
            .terms()
            .unwrap()
            .map(|t| t.map(|(term, df, _)| (term, df)).unwrap())
            .collect();
        let expected: Vec<(Vec<u8>, u64)> = terms
            .iter()
            .map(|(t, p)| (t.as_bytes().to_vec(), p.len() as u64))
            .collect();
        assert_eq!(listed, expected);
        assert!(segment.contains(300) && !segment.contains(301));
        assert_eq!(segment.length(150), Some(300));
        assert_eq!(segment.docs().count(), 300);
        assert_eq!(segment.ids_between(298, 400), &[298, 299, 300]);
        assert!(segment.ids_between(301, 400).is_empty());
        assert!(segment.ids_between(5, 4).is_empty());
    }

    #[test]
    fn a_segment_with_pages_but_no_terms_is_valid() {
        let path = temp_dir("segment-empty").join("1.seg");
        write(&path, &[(7, 0)], &[]);
        let segment = Segment::open(&path).unwrap();
        assert!(segment.contains(7));
        assert!(segment.postings(b"any").unwrap().is_empty());
        assert_eq!(segment.terms().unwrap().count(), 0);
    }

    #[test]
    fn refuses_a_corrupted_or_truncated_file() {
        let path = temp_dir("segment-corrupt").join("1.seg");
        write(&path, &[(1, 1)], &[("word", vec![(1, 1)])]);
        let good = std::fs::read(&path).unwrap();
        let mut flipped = good.clone();
        flipped[10] ^= 0xff;
        std::fs::write(&path, &flipped).unwrap();
        assert!(Segment::open(&path).is_err(), "checksum");
        assert!(
            Segment::open_written(&path).is_ok(),
            "only open checks the checksum"
        );
        std::fs::write(&path, &good[..good.len() - 1]).unwrap();
        assert!(Segment::open(&path).is_err(), "truncated");
        std::fs::write(&path, b"").unwrap();
        assert!(Segment::open(&path).is_err(), "empty");
    }

    #[test]
    fn a_damaged_posting_opens_and_serves_and_only_verify_notices() {
        let path = temp_dir("segment-verify").join("1.seg");
        write(
            &path,
            &[(1, 1), (2, 1)],
            &[("apple", vec![(1, 1)]), ("zebra", vec![(2, 1)])],
        );
        Segment::open_written(&path).unwrap().verify().unwrap();
        damage_postings(&path);
        let segment = Segment::open_written(&path).expect("opening doesn't read the postings");
        assert_eq!(segment.postings(b"apple").unwrap(), vec![(1, 1)]);
        let err = segment.verify().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("1.seg"), "{}", err);
        assert!(Segment::open(&path).is_err(), "a verifying open fails it");
    }

    // the (count, length) pairs no other beats on both, by brute force, ascending
    fn frontier(pairs: &[(u32, u32)]) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = pairs
            .iter()
            .copied()
            .filter(|&p| !pairs.iter().any(|&q| q != p && q.0 >= p.0 && q.1 <= p.1))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn every_posting_is_within_its_blocks_bounds_in_v1_and_v2() {
        for version in [1u8, 2] {
            let path = temp_dir(&format!("segment-bounds-v{}", version)).join("1.seg");
            // lengths 1..=500, each once in every 500 pages
            let docs: Vec<(u64, u32)> = (1..=1000)
                .map(|id| (id, (id * 37 % 500 + 1) as u32))
                .collect();
            let postings: Vec<Posting> = (1..=1000u64)
                .filter(|id| id % 3 != 0)
                .map(|id| (id, (id * 7 % 9 + 1) as u32))
                .collect();
            // one block of 128 whose counts rise with page length: all 128 on the frontier
            let mut by_length: Vec<(u64, u32)> = docs[..128].to_vec();
            by_length.sort_by_key(|d| d.1);
            let mut climb: Vec<Posting> = by_length
                .iter()
                .enumerate()
                .map(|(rank, &(id, _))| (id, rank as u32 + 1))
                .collect();
            climb.sort();
            let mut writer = SegmentWriter::create_version(&path, version).unwrap();
            for &(id, length) in &docs {
                writer.add_doc(id, length).unwrap();
            }
            writer.add_term(b"climb", &climb).unwrap();
            writer.add_term(b"word", &postings).unwrap();
            writer.finish().unwrap();

            let segment = Segment::open(&path).unwrap();
            assert_eq!(segment.version(), version);
            assert_eq!(segment.df(b"word").unwrap(), postings.len() as u64);
            assert_eq!(segment.df(b"absent").unwrap(), 0);
            for (term, list) in [("word", &postings), ("climb", &climb)] {
                let mut cursor = segment.cursor(term.as_bytes()).unwrap().unwrap();
                // per block: (last id, its impacts, its postings' (count, length))
                let mut blocks: Vec<(u64, Vec<(u32, u32)>, Vec<(u32, u32)>)> = Vec::new();
                let mut seen = Vec::new();
                let mut at = cursor.current().unwrap();
                while let Some((id, count)) = at {
                    let last = cursor.block_last().unwrap();
                    let impacts = cursor.block_impacts().unwrap();
                    let length = segment.length(id).unwrap();
                    assert!(id <= last, "{} v{}", id, version);
                    assert!(
                        impacts.iter().any(|&(c, l)| count <= c && length >= l),
                        "{} ({}, {}) not under {:?} v{}",
                        id,
                        count,
                        length,
                        impacts,
                        version
                    );
                    // grows with count, falls with length
                    let weight = |c: u32, l: u32| c as f64 / (c as f64 + 1.0 + l as f64 / 50.0);
                    assert!(
                        cursor.count_bound(count, weight) >= weight(count, length),
                        "{} ({}, {}) over its count bound v{}",
                        id,
                        count,
                        length,
                        version
                    );
                    if blocks.last().map(|b| b.0) != Some(last) {
                        blocks.push((last, impacts, Vec::new()));
                    }
                    blocks.last_mut().unwrap().2.push((count, length));
                    seen.push((id, count));
                    at = cursor.next().unwrap();
                }
                assert!(cursor.done());
                assert_eq!(cursor.block_bound(|_, _| 1.0), 0.0, "none once done");
                assert_eq!(&seen, list);
                for (_, impacts, pairs) in &blocks {
                    if version == 1 {
                        let max = pairs.iter().map(|p| p.0).max().unwrap();
                        assert_eq!(impacts, &vec![(max, 1)], "the segment's shortest page");
                        continue;
                    }
                    assert!(impacts
                        .windows(2)
                        .all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
                    let exact = frontier(pairs);
                    if exact.len() <= IMPACTS_MAX {
                        // tight: each is a posting's own
                        assert_eq!(impacts, &exact);
                    } else {
                        assert_eq!(impacts.len(), IMPACTS_MAX, "coarsened to fit");
                    }
                }
            }
            if version == 2 {
                let mut cursor = segment.cursor(b"climb").unwrap().unwrap();
                assert_eq!(cursor.block_impacts().unwrap().len(), IMPACTS_MAX);
                // the bound is the highest weight over the impacts
                let weight = |count: u32, length: u32| count as f64 / (length as f64 + 10.0);
                let best = climb
                    .iter()
                    .map(|&(id, count)| weight(count, segment.length(id).unwrap()))
                    .fold(0.0, f64::max);
                assert!(cursor.block_bound(weight) >= best);
                assert!(cursor.current().unwrap().is_some());
            }
        }
    }

    #[test]
    fn a_posting_whose_page_has_no_length_is_under_its_count_bound() {
        let path = temp_dir("segment-orphan").join("1.seg");
        let mut writer = SegmentWriter::create(&path).unwrap();
        writer.add_doc(1, 300).unwrap();
        writer.add_doc(3, 300).unwrap();
        // page 2 was never added: its posting counts as length 0
        writer.add_term(b"word", &[(1, 2), (2, 2), (3, 2)]).unwrap();
        writer.finish().unwrap();
        let segment = Segment::open(&path).unwrap();
        assert_eq!(segment.length(2), None);
        let cursor = segment.cursor(b"word").unwrap().unwrap();
        let weight = |c: u32, l: u32| c as f64 / (c as f64 + 1.0 + l as f64 / 50.0);
        assert_eq!(cursor.count_bound(2, weight), weight(2, 0));
        assert_eq!(cursor.count_bound(1, weight), weight(1, 0));
    }

    #[test]
    fn a_cursor_seeks_forward_to_exact_ids_gaps_block_edges_and_the_end() {
        let path = temp_dir("segment-cursor").join("1.seg");
        let docs: Vec<(u64, u32)> = (1..=1000).map(|id| (id, 5)).collect();
        // 500 postings: blocks 2..=256, 258..=512, 514..=768, 770..=1000
        let even: Vec<Posting> = (1..=1000u64)
            .filter(|id| id % 2 == 0)
            .map(|id| (id, 1))
            .collect();
        write(&path, &docs, &[("even", even)]);
        let segment = Segment::open(&path).unwrap();
        assert!(segment.cursor(b"odd").unwrap().is_none());

        let mut c = segment.cursor(b"even").unwrap().unwrap();
        assert_eq!(c.df(), 500);
        assert_eq!(c.seek(10).unwrap(), Some((10, 1)), "exact");
        assert_eq!(c.seek(11).unwrap(), Some((12, 1)), "between postings");
        assert_eq!(c.seek(5).unwrap(), Some((12, 1)), "never backwards");
        assert_eq!(
            c.seek(256).unwrap(),
            Some((256, 1)),
            "last of the first block"
        );
        assert_eq!(c.seek(257).unwrap(), Some((258, 1)), "first of the second");
        assert_eq!(c.next().unwrap(), Some((260, 1)));

        let mut skipping = segment.cursor(b"even").unwrap().unwrap();
        let before = DECODED.with(|d| d.get());
        skipping.seek_block(900).unwrap();
        assert_eq!(DECODED.with(|d| d.get()), before, "headers only");
        assert_eq!(skipping.block_last(), Some(1000));
        assert_eq!(skipping.seek(1001).unwrap(), None);
        assert!(skipping.done());
        assert_eq!(skipping.block_last(), None);
    }

    #[test]
    fn a_list_over_the_read_ahead_window_reads_the_same_by_next_and_by_seek() {
        let path = temp_dir("segment-window").join("1.seg");
        // 40,000 postings at scattered ids, with two-byte counts: several windows' worth
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut random = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut id = 0;
        let postings: Vec<Posting> = (0..40_000)
            .map(|_| {
                id += 1 + random(300);
                (id, 128 + random(10_000) as u32)
            })
            .collect();
        let docs: Vec<(u64, u32)> = postings.iter().map(|&(id, _)| (id, 7)).collect();
        write(&path, &docs, &[("big", postings.clone())]);
        let segment = Segment::open(&path).unwrap();
        assert!(segment.dict_off > 3 * WINDOW, "{} bytes", segment.dict_off);
        assert_eq!(segment.postings(b"big").unwrap(), postings);

        let straddled = STRADDLED.with(|s| s.get());
        let mut cursor = segment.cursor(b"big").unwrap().unwrap();
        let mut walked = Vec::new();
        let mut at = cursor.current().unwrap();
        while let Some(posting) = at {
            walked.push(posting);
            at = cursor.next().unwrap();
        }
        assert_eq!(walked, postings);
        assert!(
            STRADDLED.with(|s| s.get()) > straddled,
            "a header or body ran past the window's end"
        );

        // seeks to ids on, between and past postings, in steps of up to several blocks
        let mut cursor = segment.cursor(b"big").unwrap().unwrap();
        let mut target = 0;
        while target <= id + 1 {
            let i = postings.partition_point(|p| p.0 < target);
            assert_eq!(cursor.seek(target).unwrap(), postings.get(i).copied());
            target += 1 + random(2_000);
        }
        assert_eq!(cursor.seek(id + 1).unwrap(), None);
        assert!(cursor.done());
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
}
