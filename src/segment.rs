/*!
 * One immutable segment file of the inverted index (#247). Version 1 layout, integers as LEB128
 * varints unless marked:
 *
 *   header    "ipxseg", version u8
 *   docs      (id delta, length) per page, ascending by id
 *   postings  per term, blocks of up to BLOCK postings: n, last id, body bytes, max count (for
 *             #248's block-max pruning, unused so far); then the id deltas, then the counts
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
const VERSION: u8 = 1;
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
}

impl SegmentWriter {
    /// Starts a new segment file; fails if `path` exists
    pub fn create(path: &Path) -> io::Result<SegmentWriter> {
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
        };
        writer.write(MAGIC)?;
        writer.write(&[VERSION])?;
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
            put_varint(&mut head, block.iter().map(|p| p.1).max().unwrap() as u64);
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
}

impl Segment {
    /// Opens and verifies (checksum, then structure) a segment file
    pub fn open(path: &Path) -> io::Result<Segment> {
        Segment::read(path, true)
    }

    /// Opens a segment file this process just wrote and fsynced: its structure is checked, but
    /// not its checksum, which would re-read the whole file (#247). Open checks it at startup.
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
        if verify {
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
            if hash != field(7) {
                return Err(corrupt());
            }
        }
        let header = read_at(&file, 0, MAGIC.len() as u64 + 1)?;
        if &header[..MAGIC.len()] != MAGIC {
            return Err(corrupt());
        }
        if header[MAGIC.len()] != VERSION {
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
        })
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
            // a block header is four varints, at most 40 bytes
            let head = read_at(
                &self.file,
                offset,
                40.min(self.dict_off.saturating_sub(offset)),
            )?;
            let mut pos = 0;
            let n = get_varint(&head, &mut pos)? as usize;
            let _last = get_varint(&head, &mut pos)?;
            let body_len = get_varint(&head, &mut pos)?;
            let _max_count = get_varint(&head, &mut pos)?;
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
}
