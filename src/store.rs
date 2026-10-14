use crate::index_queue::normalize;
use crate::index_result::IndexResult;
use crate::segment::Posting;
use crate::segments::{Maintained, Segments};
use log::info;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
// record_orphans checks at most this many ids
const MAX_ORPHAN_RANGE: u64 = 10_000_000;

/**
 * The persisted index (#106): one SQLite file, `index.sqlite`, under DATA_DIR. secure_delete
 * zeroes deleted rows in the file. The rollback journal is kept in memory: an on-disk journal
 * (DELETE or WAL mode) would write plaintext copies of pages and later only unlink them. The
 * trade-off is that a crash mid-write can corrupt the file; the index is rebuildable, so
 * recovery is the documented wipe.
 *
 * The inverted index is in segment files under DATA_DIR/segments (#247, see segments.rs);
 * SQLite keeps pages, the queue and mirrors. Page ids are never reused, and a re-crawl gets a
 * new one: `dead_pages` lists ids that may still be in a segment, so search can drop them at once
 * and a takedown can find every segment holding a version of the page (#249).
 *
 * IPFS has many mirrors of the same content (#253): a page whose text hashes the same as one
 * already indexed under another item isn't indexed again, only recorded in `mirrors`.
 */
pub struct Store {
    conn: Mutex<Connection>,
    // search's own read-only connection, so it doesn't wait behind the crawl's writes for the
    // connection lock: it only waits out a commit. None in memory, where conn is used instead.
    reader: Option<Mutex<Connection>>,
    // None for an in-memory store (no DATA_DIR)
    path: Option<PathBuf>,
    // the inverted index (#247)
    segments: Segments,
    // rows in `queue`, kept here since counting them for every /status is a full scan
    queued: AtomicUsize,
    // rows in `pages` and the sum of their lengths, for BM25's average page length (#251)
    pages: AtomicUsize,
    lengths: AtomicU64,
}

pub type OpenError = Box<dyn std::error::Error + Send + Sync>;

/// What upsert did with a page
#[derive(Debug, PartialEq)]
pub enum Upserted {
    Indexed,
    /// not indexed: the item given has the same text
    Mirror(String),
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS pages (
        -- AUTOINCREMENT: an id is never reused, or a dead one still in a segment would come back
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        item       TEXT NOT NULL UNIQUE,
        cid        TEXT NOT NULL,
        title      TEXT NOT NULL,
        excerpt    TEXT NOT NULL,
        indexed_at INTEGER NOT NULL,
        -- how many terms the page has (the sum of its postings' counts), for BM25 (#251)
        length     INTEGER NOT NULL DEFAULT 0,
        -- SHA-256 of the extracted text (#253); NULL for pages indexed before it was kept
        text_hash  BLOB
    );
    -- items with the same text as an indexed page (#253), deleted along with that page
    CREATE TABLE IF NOT EXISTS mirrors (
        item TEXT PRIMARY KEY,
        page INTEGER NOT NULL
    );
    -- page ids that may still be in a segment file (#247, #249): deleted or superseded pages
    CREATE TABLE IF NOT EXISTS dead_pages (
        id      INTEGER PRIMARY KEY,
        item    TEXT NOT NULL,
        cid     TEXT NOT NULL,
        died_at INTEGER NOT NULL,
        -- 1: rewrite the segments holding it now
        erase   INTEGER NOT NULL
    );
    -- the crawl frontier (#224): items waiting to be fetched, in insertion order
    CREATE TABLE IF NOT EXISTS queue (
        item TEXT PRIMARY KEY
    );";

// the inverted index before #247; only the migrations from older schemas create it
const LEGACY_POSTINGS: &str = "
    CREATE TABLE IF NOT EXISTS postings (
        term  TEXT NOT NULL,
        page  INTEGER NOT NULL,
        count INTEGER NOT NULL,
        PRIMARY KEY (term, page)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS terms (
        term  TEXT PRIMARY KEY,
        pages INTEGER NOT NULL
    ) WITHOUT ROWID;";

impl Store {
    /// Opens DATA_DIR's index; SQLite keeps at most cache_mb of it in memory
    pub fn open(dir: &Path, cache_mb: u32) -> Result<Store, OpenError> {
        let path = dir.join("index.sqlite");
        let conn = Connection::open(&path)?;
        let segments = Segments::open(&dir.join("segments"))?;
        let mut store = Store::init(conn, Some(path.clone()), cache_mb, segments)?;
        // default cache_size, not cache_mb: SQLite drops a connection's cache whenever another
        // one commits, which the crawl does constantly, so a big one here would rarely be warm
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        reader.busy_timeout(BUSY_TIMEOUT)?;
        store.reader = Some(Mutex::new(reader));
        // erasures a crash interrupted finish before anything is crawled or served (#249)
        store.maintain();
        Ok(store)
    }

    /// For running without DATA_DIR: nothing persists, and the cache cap doesn't apply
    pub fn open_in_memory() -> Result<Store, OpenError> {
        Store::init(
            Connection::open_in_memory()?,
            None,
            0,
            Segments::in_memory(),
        )
    }

    fn init(
        mut conn: Connection,
        path: Option<PathBuf>,
        cache_mb: u32,
        segments: Segments,
    ) -> Result<Store, OpenError> {
        conn.pragma_update(None, "secure_delete", true)?;
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
        // commits go to the OS page cache instead of waiting on fsync: on spinning disks each
        // page's scattered postings writes held the lock for seconds, timing out searches. The
        // memory journal already means a crash can corrupt the file (rebuild); this extends that
        // to power loss.
        conn.pragma_update(None, "synchronous", "OFF")?;
        // sorts and VACUUM's copy stay in RAM: a temp file would hold plaintext
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // a commit waits for the reader's queries to finish rather than failing
        conn.busy_timeout(BUSY_TIMEOUT)?;
        if cache_mb > 0 {
            // negative: in KiB rather than pages
            conn.pragma_update(None, "cache_size", -(cache_mb as i64) * 1024)?;
        }
        migrate_keywords_column(&mut conn)?;
        conn.execute_batch(SCHEMA)?;
        let has_text_hash: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('pages') WHERE name = 'text_hash'")?
            .exists([])?;
        if !has_text_hash {
            conn.execute_batch("ALTER TABLE pages ADD COLUMN text_hash BLOB")?;
        }
        migrate_page_lengths(&mut conn)?;
        migrate_postings_to_segments(&mut conn, &segments)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS pages_text_hash ON pages (text_hash);
             CREATE INDEX IF NOT EXISTS mirrors_page ON mirrors (page);
             CREATE INDEX IF NOT EXISTS dead_pages_item ON dead_pages (item);",
        )?;
        recover_unflushed(&mut conn, segments.flushed_through())?;
        // ids in the segments must never be handed out again, even if this file fell behind them
        // (rolled back by a power loss, or wiped while segments/ was kept)
        let floor = segments.flushed_through() as i64;
        // (sqlite_sequence has no unique key on name: update the row, only inserting if absent)
        let raised = conn.execute(
            "UPDATE sqlite_sequence SET seq = max(seq, ?1) WHERE name = 'pages'",
            [floor],
        )?;
        if raised == 0 && floor > 0 {
            conn.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES ('pages', ?1)",
                [floor],
            )?;
        }
        record_orphans(&mut conn, &segments)?;
        let dead: Vec<u64> = conn
            .prepare("SELECT id FROM dead_pages")?
            .query_map([], |r| r.get::<_, i64>(0))?
            .map(|id| id.map(|id| id as u64))
            .collect::<rusqlite::Result<_>>()?;
        segments.set_dead(&dead);
        let erase: Vec<u64> = conn
            .prepare("SELECT id FROM dead_pages WHERE erase = 1")?
            .query_map([], |r| r.get::<_, i64>(0))?
            .map(|id| id.map(|id| id as u64))
            .collect::<rusqlite::Result<_>>()?;
        segments.mark_erase(&erase);
        let queued: i64 = conn.query_row("SELECT count(*) FROM queue", [], |r| r.get(0))?;
        let (pages, lengths): (i64, i64) = conn.query_row(
            "SELECT count(*), coalesce(sum(length), 0) FROM pages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
            reader: None,
            path,
            segments,
            queued: AtomicUsize::new(queued as usize),
            pages: AtomicUsize::new(pages as usize),
            lengths: AtomicU64::new(lengths as u64),
        })
    }

    fn read_conn(&self) -> MutexGuard<'_, Connection> {
        self.reader.as_ref().unwrap_or(&self.conn).lock().unwrap()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /**
     * Adds or replaces a page: its row here, its postings (from result.keywords) in the segment
     * write buffer. A re-crawl gets a new id; the old one goes to `dead_pages`. If another item
     * already has the same text (result.text_hash), the page isn't indexed: it's recorded as a
     * mirror of that one instead.
     */
    pub fn upsert(
        &self,
        item: &str,
        result: &IndexResult,
        indexed_at: i64,
    ) -> rusqlite::Result<Upserted> {
        // timed (with queue_add and queue_pop) to tell lock contention from slow disk: waited is
        // for the connection lock, took is the transaction itself
        let start = Instant::now();
        let mut conn = self.conn.lock().unwrap();
        let waited = start.elapsed();
        let tx = conn.transaction()?;
        if let Some(hash) = &result.text_hash {
            let original: Option<(i64, String)> = tx
                .prepare_cached("SELECT id, item FROM pages WHERE text_hash = ?1 AND item != ?2")?
                .query_row(params![hash, item], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if let Some((page, original)) = original {
                // indexed under this item before its text matched: drop it, and point its own
                // mirrors at the original instead
                let removed = self.remove_page(&tx, item)?;
                if let Some((old, _)) = removed {
                    tx.prepare_cached("UPDATE mirrors SET page = ?2 WHERE page = ?1")?
                        .execute(params![old, page])?;
                }
                tx.prepare_cached(
                    "INSERT INTO mirrors (item, page) VALUES (?1, ?2)
                     ON CONFLICT (item) DO UPDATE SET page = excluded.page",
                )?
                .execute(params![item, page])?;
                tx.commit()?;
                drop(conn);
                if let Some((old, length)) = removed {
                    self.segments.set_dead(&[old as u64]);
                    self.adjust_lengths(1, length);
                }
                info!(
                    "Store upsert {}: mirror of {}, waited {:?}, took {:?}",
                    item,
                    original,
                    waited,
                    start.elapsed() - waited
                );
                return Ok(Upserted::Mirror(original));
            }
        }
        tx.prepare_cached("DELETE FROM mirrors WHERE item = ?1")?
            .execute([item])?;
        let length: i64 = result.keywords.values().map(|&count| count as i64).sum();
        // a re-crawl gets a new id: the old version's postings are in a segment, which can't change
        let old = self.remove_page(&tx, item)?;
        let id: i64 = tx.query_row(
            "INSERT INTO pages (item, cid, title, excerpt, indexed_at, length, text_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
            params![
                item,
                result.cid,
                result.title,
                result.excerpt,
                indexed_at,
                length,
                result.text_hash
            ],
            |r| r.get(0),
        )?;
        if let Some((old, _)) = old {
            tx.prepare_cached("UPDATE mirrors SET page = ?2 WHERE page = ?1")?
                .execute(params![old, id])?;
        }
        tx.commit()?;
        // still under the connection lock, so ids reach the buffer in ascending order
        self.segments
            .add(id as u64, length as u32, &result.keywords);
        drop(conn);
        if let Some((old, old_length)) = old {
            self.segments.set_dead(&[old as u64]);
            self.adjust_lengths(1, old_length);
        }
        self.pages.fetch_add(1, Ordering::Relaxed);
        self.lengths.fetch_add(length as u64, Ordering::Relaxed);
        info!(
            "Store upsert {}: {} terms, waited {:?}, took {:?}",
            item,
            result.keywords.len(),
            waited,
            start.elapsed() - waited
        );
        Ok(Upserted::Indexed)
    }

    // deletes an item's page, recording its id in dead_pages; returns the id and its length
    fn remove_page(&self, tx: &Transaction, item: &str) -> rusqlite::Result<Option<(i64, u64)>> {
        let row: Option<(i64, i64, String)> = tx
            .prepare_cached("DELETE FROM pages WHERE item = ?1 RETURNING id, length, cid")?
            .query_row([item], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((id, length, cid)) = row else {
            return Ok(None);
        };
        tx.prepare_cached(
            "INSERT INTO dead_pages (id, item, cid, died_at, erase) VALUES (?1, ?2, ?3, ?4, 0)",
        )?
        .execute(params![id, item, cid, now_secs()])?;
        Ok(Some((id, length as u64)))
    }

    // removes pages from the running page count and length total
    fn adjust_lengths(&self, pages: usize, length: u64) {
        self.pages.fetch_sub(pages, Ordering::Relaxed);
        self.lengths.fetch_sub(length, Ordering::Relaxed);
    }

    /**
     * Deletes pages and their mirrors in one transaction (items that are mirrors themselves are
     * dropped too); returns how many pages went. They leave search at once. With `erase`
     * (takedowns, expiry), the segment files holding them or any earlier version of them are
     * rewritten without them promptly (#249).
     */
    pub fn delete(&self, items: &[String], erase: bool) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (mut dead, mut erased, mut length) = (Vec::new(), Vec::new(), 0);
        {
            let mut mirror = tx.prepare("DELETE FROM mirrors WHERE item = ?1")?;
            let mut mirrors_of = tx.prepare("DELETE FROM mirrors WHERE page = ?1")?;
            let mut versions =
                tx.prepare("UPDATE dead_pages SET erase = 1 WHERE item = ?1 RETURNING id")?;
            for item in items {
                mirror.execute([item])?;
                if let Some((id, page_length)) = self.remove_page(&tx, item)? {
                    dead.push(id as u64);
                    length += page_length;
                    mirrors_of.execute([id])?;
                }
                if erase {
                    // this page and every superseded version of it
                    for id in versions.query_map([item], |r| r.get::<_, i64>(0))? {
                        erased.push(id? as u64);
                    }
                }
            }
        }
        tx.commit()?;
        drop(conn);
        self.segments.set_dead(&dead);
        self.segments.mark_erase(&erased);
        self.adjust_lengths(dead.len(), length);
        Ok(dead.len())
    }

    /// Every dead page version still recorded, as (id, item, cid, when it died)
    pub fn dead_versions(&self) -> rusqlite::Result<Vec<(u64, String, String, i64)>> {
        // a full scan: on the read connection, so upserts don't wait behind it
        let conn = self.read_conn();
        let mut stmt = conn.prepare("SELECT id, item, cid, died_at FROM dead_pages")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect()
    }

    /// Dead versions to erase from the segment files promptly (#249)
    pub fn erase_ids(&self, ids: &[u64]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE dead_pages SET erase = 1 WHERE id = ?1")?;
            for &id in ids {
                stmt.execute([id as i64])?;
            }
        }
        tx.commit()?;
        drop(conn);
        self.segments.mark_erase(ids);
        Ok(())
    }

    /// Moves a page to a new item (and cid), keeping its postings; false if `to` exists already
    pub fn rekey(&self, from: &str, to: &str, cid: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        match conn.execute(
            "UPDATE pages SET item = ?2, cid = ?3 WHERE item = ?1",
            params![from, to, cid],
        ) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    /// Every page, without keyword counts (those stay on disk)
    pub fn load_all(&self) -> rusqlite::Result<Vec<(String, IndexResult, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item, cid, title, excerpt, indexed_at FROM pages")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                IndexResult::new(r.get(1)?, r.get(2)?, r.get(3)?, HashMap::new()),
                r.get(4)?,
            ))
        })?;
        rows.collect()
    }

    /// Every mirror, as (item, the item of the page it has the same text as)
    pub fn mirrors_load(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT mirrors.item, pages.item FROM mirrors JOIN pages ON pages.id = mirrors.page",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// The live pages a term is on, as (page id, how often it appears there, the page's length)
    pub fn postings(&self, term: &str) -> std::io::Result<Vec<(u64, u32, u32)>> {
        self.segments.postings(term)
    }

    /// The items of page ids; ids no longer in `pages` are left out
    pub fn items(&self, ids: &[u64]) -> rusqlite::Result<HashMap<u64, String>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare_cached("SELECT item FROM pages WHERE id = ?1")?;
        let mut items = HashMap::new();
        for &id in ids {
            if let Some(item) = stmt.query_row([id as i64], |r| r.get(0)).optional()? {
                items.insert(id, item);
            }
        }
        Ok(items)
    }

    /// The mean page length in terms, for BM25; 0 with no pages
    pub fn average_length(&self) -> f64 {
        let pages = self.pages.load(Ordering::Relaxed);
        if pages == 0 {
            return 0.0;
        }
        self.lengths.load(Ordering::Relaxed) as f64 / pages as f64
    }

    pub fn term_count(&self) -> usize {
        self.segments.term_stats(0).map_or_else(
            |err| {
                log::warn!("Couldn't count terms: {}", err);
                0
            },
            |(count, _)| count,
        )
    }

    /// The n terms on the most pages
    pub fn top_terms(&self, n: usize) -> std::io::Result<Vec<(String, u32)>> {
        Ok(self.segments.term_stats(n)?.1)
    }

    /// All segment upkeep, merges included, in one call (startup, tests; see Segments::maintain);
    /// drops dead_pages rows no segment needs any more
    pub fn maintain(&self) {
        self.maintained(self.segments.maintain());
    }

    /// One round for the upkeep thread (see Segments::upkeep): flush, erasures, retired files
    pub fn upkeep(&self) {
        self.maintained(self.segments.upkeep());
    }

    /// Merges segments until none are due; for the merge thread (see Segments::merge)
    pub fn merge(&self) {
        if let Err(err) = self.segments.merge() {
            log::warn!("Segment merge failed: {}", err);
        }
    }

    // the dead ids pruned go from dead_pages even if a step failed
    fn maintained(&self, done: Maintained) {
        if let Some(err) = &done.error {
            log::warn!("Segment upkeep failed: {}", err);
        }
        let dropped = done.pruned;
        if dropped.is_empty() {
            return;
        }
        let mut conn = self.conn.lock().unwrap();
        let deleted = conn.transaction().and_then(|tx| {
            {
                let mut stmt = tx.prepare_cached("DELETE FROM dead_pages WHERE id = ?1")?;
                for &id in &dropped {
                    stmt.execute([id as i64])?;
                }
            }
            tx.commit()
        });
        if let Err(err) = deleted {
            log::warn!("Couldn't drop {} dead page ids: {}", dropped.len(), err);
        }
    }

    /// Blocks until there's segment upkeep to do, or a second passes
    pub fn wait_for_maintenance(&self) {
        self.segments.wait();
    }

    /// Blocks until a segment is installed (a merge may be due), or a second passes
    pub fn wait_for_merge(&self) {
        self.segments.wait_for_merge();
    }

    /// Writes buffered postings to a segment now
    pub fn flush(&self) -> std::io::Result<()> {
        self.segments.flush()
    }

    /// Adds items to the frontier in one transaction; ones already there are ignored. Returns
    /// how many were new.
    pub fn queue_add(&self, items: &[String]) -> rusqlite::Result<usize> {
        let start = Instant::now();
        let mut conn = self.conn.lock().unwrap();
        let waited = start.elapsed();
        let tx = conn.transaction()?;
        let mut added = 0;
        {
            let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO queue (item) VALUES (?1)")?;
            for item in items {
                added += stmt.execute([item])?;
            }
        }
        tx.commit()?;
        self.queued.fetch_add(added, Ordering::Relaxed);
        info!(
            "Store queue_add: {} of {} new, waited {:?}, took {:?}",
            added,
            items.len(),
            waited,
            start.elapsed() - waited
        );
        Ok(added)
    }

    pub fn queue_remove(&self, items: &[String]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut removed = 0;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM queue WHERE item = ?1")?;
            for item in items {
                removed += stmt.execute([item])?;
            }
        }
        tx.commit()?;
        self.queued.fetch_sub(removed, Ordering::Relaxed);
        Ok(())
    }

    /// Takes the oldest item off the frontier, or None if it's empty
    pub fn queue_pop(&self) -> rusqlite::Result<Option<String>> {
        let start = Instant::now();
        let conn = self.conn.lock().unwrap();
        let waited = start.elapsed();
        let item = conn
            .prepare_cached(
                "DELETE FROM queue WHERE rowid = (SELECT min(rowid) FROM queue) RETURNING item",
            )?
            .query_row([], |r| r.get(0))
            .optional()?;
        // not on empty polls: idle workers poll every 200ms
        if item.is_some() {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            info!(
                "Store queue_pop: waited {:?}, took {:?}",
                waited,
                start.elapsed() - waited
            );
        }
        Ok(item)
    }

    pub fn queue_len(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    /// Queued items per root CID (the first segment of "cid/path"). On the reader: a full scan
    /// that would otherwise hold up the crawl
    pub fn queue_roots(&self) -> rusqlite::Result<Vec<(String, usize)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT substr(item, 1, instr(item || '/', '/') - 1) AS root, count(*)
             FROM queue GROUP BY root",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)))?;
        rows.collect()
    }

    /// The frontier in the order it was added
    pub fn queue_load(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item FROM queue ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    pub fn items_older_than(&self, cutoff: i64) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item FROM pages WHERE indexed_at < ?1")?;
        let rows = stmt.query_map([cutoff], |r| r.get(0))?;
        rows.collect()
    }
}

// a clean stop keeps what's buffered; a crash loses it, and recover_unflushed re-queues it
impl Drop for Store {
    fn drop(&mut self) {
        if let Err(err) = self.segments.flush() {
            log::warn!("Couldn't flush the index on close: {}", err);
        }
    }
}

#[cfg(test)]
impl Store {
    // makes every following read/write fail, to exercise callers' error handling
    pub(crate) fn break_for_test(&self) {
        self.conn
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE pages")
            .unwrap();
    }
}

/**
 * #231: `pages` used to carry each page's keyword counts as text, and the inverted index was
 * rebuilt in RAM from them at every start. Moves them into `postings`/`terms` (once, in one
 * transaction) and gives pages an explicit id for postings to refer to. Counts saved before
 * #209 were keyed by raw words ("Files."); they're merged under normalized terms on the way.
 */
fn migrate_keywords_column(conn: &mut Connection) -> rusqlite::Result<()> {
    let has_keywords: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('pages') WHERE name = 'keywords'")?
        .exists([])?;
    if !has_keywords {
        return Ok(());
    }
    log::info!("Moving keyword counts into the postings table (one-time, #231)");
    let tx = conn.transaction()?;
    tx.execute_batch(
        "ALTER TABLE pages RENAME TO pages_old;
         CREATE TEMP TABLE staged (term TEXT NOT NULL, page INTEGER NOT NULL, count INTEGER NOT NULL);",
    )?;
    tx.execute_batch(SCHEMA)?;
    tx.execute_batch(LEGACY_POSTINGS)?;
    {
        let mut old = tx.prepare(
            "SELECT item, cid, title, excerpt, keywords, indexed_at FROM pages_old ORDER BY rowid",
        )?;
        let mut page = tx.prepare(
            "INSERT INTO pages (item, cid, title, excerpt, indexed_at) VALUES (?1, ?2, ?3, ?4, ?5)
             RETURNING id",
        )?;
        let mut stage = tx.prepare("INSERT INTO staged (term, page, count) VALUES (?1, ?2, ?3)")?;
        let mut rows = old.query([])?;
        while let Some(row) = rows.next()? {
            let keywords: String = row.get(4)?;
            let id: i64 = page.query_row(
                params![
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(5)?
                ],
                |r| r.get(0),
            )?;
            for (word, count) in decode_keywords(&keywords) {
                for term in normalize(&word) {
                    stage.execute(params![term, id, count])?;
                }
            }
        }
    }
    // inserted in key order: far faster than random inserts into the b-tree
    tx.execute_batch(
        "INSERT INTO postings (term, page, count)
           SELECT term, page, sum(count) FROM staged GROUP BY term, page ORDER BY term, page;
         INSERT INTO terms (term, pages) SELECT term, count(*) FROM postings GROUP BY term;
         UPDATE pages SET length = (SELECT coalesce(sum(count), 0) FROM staged
                                    WHERE staged.page = pages.id);
         DROP TABLE staged;
         DROP TABLE pages_old;",
    )?;
    tx.commit()
}

/**
 * #251: pages stored before BM25 have no length. Adds the column and fills it from their
 * postings, once.
 */
fn migrate_page_lengths(conn: &mut Connection) -> rusqlite::Result<()> {
    let has_length: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('pages') WHERE name = 'length'")?
        .exists([])?;
    if has_length {
        return Ok(());
    }
    let has_postings: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'postings'")?
        .exists([])?;
    if !has_postings {
        conn.execute_batch("ALTER TABLE pages ADD COLUMN length INTEGER NOT NULL DEFAULT 0")?;
        return Ok(());
    }
    log::info!("Computing page lengths from their postings (one-time, #251)");
    let tx = conn.transaction()?;
    tx.execute_batch(
        "ALTER TABLE pages ADD COLUMN length INTEGER NOT NULL DEFAULT 0;
         UPDATE pages SET length = (SELECT coalesce(sum(count), 0) FROM postings
                                    WHERE postings.page = pages.id);",
    )?;
    tx.commit()
}

/**
 * Pages whose postings were still in the write buffer when the process stopped (ids past the
 * last flush) aren't searchable: their rows go, and they're queued to be crawled again.
 */
fn recover_unflushed(conn: &mut Connection, flushed_through: u64) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let items: Vec<String> = tx
        .prepare("SELECT item FROM pages WHERE id > ?1 ORDER BY id")?
        .query_map([flushed_through as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if items.is_empty() {
        return Ok(());
    }
    log::warn!(
        "{} pages weren't flushed to a segment before the last stop; queueing them again",
        items.len()
    );
    // their mirrors are crawled again too: the page they point at is going
    tx.execute(
        "INSERT OR IGNORE INTO queue (item) SELECT item FROM mirrors WHERE page > ?1",
        [flushed_through as i64],
    )?;
    tx.execute(
        "DELETE FROM mirrors WHERE page IN (SELECT id FROM pages WHERE id > ?1)",
        [flushed_through as i64],
    )?;
    tx.execute("DELETE FROM pages WHERE id > ?1", [flushed_through as i64])?;
    {
        let mut queue = tx.prepare("INSERT OR IGNORE INTO queue (item) VALUES (?1)")?;
        for item in &items {
            queue.execute([item])?;
        }
    }
    tx.commit()
}

/**
 * Ids in the segments past every row here, left when this file was rolled back behind them (a
 * power loss with synchronous off): no page or dead_pages row refers to them, so nothing would
 * ever erase their text. Recorded as dead and to erase (#249), with no item or cid.
 */
fn record_orphans(conn: &mut Connection, segments: &Segments) -> rusqlite::Result<()> {
    let known: i64 = conn.query_row(
        "SELECT max((SELECT coalesce(max(id), 0) FROM pages),
                    (SELECT coalesce(max(id), 0) FROM dead_pages))",
        [],
        |r| r.get(0),
    )?;
    let (from, to) = (known as u64 + 1, segments.flushed_through());
    if from > to {
        return Ok(());
    }
    if to - from >= MAX_ORPHAN_RANGE {
        log::warn!(
            "Page ids {} to {} are in the segments but not the index file; too many to check, \
             so their text stays until a wipe",
            from,
            to
        );
        return Ok(());
    }
    let orphans = segments.ids_between(from, to);
    if orphans.is_empty() {
        return Ok(());
    }
    log::warn!(
        "{} pages in the segments have no record in the index file (rolled back?); erasing them",
        orphans.len()
    );
    let tx = conn.transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO dead_pages (id, item, cid, died_at, erase) VALUES (?1, '', '', ?2, 1)",
        )?;
        for id in orphans {
            insert.execute(params![id as i64, now_secs()])?;
        }
    }
    tx.commit()
}

/**
 * #247: postings move from SQLite into segment files. Streams the postings table (in term order
 * already) into one segment, then drops it, rebuilds `pages` with AUTOINCREMENT keeping its ids,
 * and vacuums. If this is interrupted the postings table is still there, and the next start
 * redoes it, erasing the segment the earlier attempt wrote.
 */
fn migrate_postings_to_segments(
    conn: &mut Connection,
    segments: &Segments,
) -> Result<(), OpenError> {
    let has_postings: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'postings'")?
        .exists([])?;
    if !has_postings {
        return Ok(());
    }
    info!("Moving postings into segment files (one-time, #247)");
    let max_id: i64 = conn.query_row("SELECT coalesce(max(id), 0) FROM pages", [], |r| r.get(0))?;
    let sql = std::io::Error::other;
    segments.bulk_load(max_id as u64, |writer| {
        let mut docs = conn
            .prepare("SELECT id, length FROM pages ORDER BY id")
            .map_err(sql)?;
        let mut rows = docs.query([]).map_err(sql)?;
        while let Some(row) = rows.next().map_err(sql)? {
            let id: i64 = row.get(0).map_err(sql)?;
            let length: i64 = row.get(1).map_err(sql)?;
            writer.add_doc(id as u64, length as u32)?;
        }
        // BINARY collation is bytewise, the order segments need
        let mut postings = conn
            .prepare(
                "SELECT postings.term, postings.page, postings.count FROM postings
                 JOIN pages ON pages.id = postings.page ORDER BY postings.term, postings.page",
            )
            .map_err(sql)?;
        let mut rows = postings.query([]).map_err(sql)?;
        let (mut term, mut list): (Option<String>, Vec<Posting>) = (None, Vec::new());
        while let Some(row) = rows.next().map_err(sql)? {
            let next: String = row.get(0).map_err(sql)?;
            if term.as_deref() != Some(next.as_str()) {
                if let Some(done) = term.replace(next) {
                    writer.add_term(done.as_bytes(), &list)?;
                    list.clear();
                }
            }
            let page: i64 = row.get(1).map_err(sql)?;
            let count: i64 = row.get(2).map_err(sql)?;
            list.push((page as u64, count as u32));
        }
        if let Some(done) = term {
            writer.add_term(done.as_bytes(), &list)?;
        }
        Ok(())
    })?;
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE pages_new (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             item       TEXT NOT NULL UNIQUE,
             cid        TEXT NOT NULL,
             title      TEXT NOT NULL,
             excerpt    TEXT NOT NULL,
             indexed_at INTEGER NOT NULL,
             length     INTEGER NOT NULL DEFAULT 0,
             text_hash  BLOB
         );
         INSERT INTO pages_new (id, item, cid, title, excerpt, indexed_at, length, text_hash)
           SELECT id, item, cid, title, excerpt, indexed_at, length, text_hash FROM pages;
         DROP TABLE pages;
         ALTER TABLE pages_new RENAME TO pages;
         DROP TABLE postings;
         DROP TABLE IF EXISTS terms;",
    )?;
    tx.commit()?;
    // secure_delete zeroed the dropped tables' pages; this gives the space back
    if let Err(err) = conn.execute_batch("VACUUM") {
        log::warn!("Couldn't vacuum after moving the postings: {}", err);
    }
    Ok(())
}

// the old keywords column: "word count" per line
fn decode_keywords(text: &str) -> HashMap<String, u32> {
    text.lines()
        .filter_map(|line| {
            let (word, count) = line.split_once(' ')?;
            Some((word.to_string(), count.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    pub(crate) use crate::segments::tests::dir_has;

    // a fresh, unique directory under the system temp dir
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ipfs_indexer_{}_{}_{}",
            name,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn page(cid: &str, title: &str, words: &[(&str, u32)]) -> IndexResult {
        let keywords = words.iter().map(|(w, c)| (w.to_string(), *c)).collect();
        IndexResult::new(
            cid.to_string(),
            title.to_string(),
            "an excerpt".to_string(),
            keywords,
        )
    }

    // postings as (item, count, length), sorted by item, like the SQL-backed postings returned
    pub(crate) fn postings_by_item(store: &Store, term: &str) -> Vec<(String, u32, u32)> {
        let found = store.postings(term).unwrap();
        let ids: Vec<u64> = found.iter().map(|p| p.0).collect();
        let items = store.items(&ids).unwrap();
        let mut out: Vec<_> = found
            .into_iter()
            .map(|(id, count, length)| (items[&id].clone(), count, length))
            .collect();
        out.sort();
        out
    }

    fn dir_has_table(dir: &Path, table: &str) -> bool {
        Connection::open(dir.join("index.sqlite"))
            .unwrap()
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .unwrap()
            .exists([table])
            .unwrap()
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    #[test]
    fn round_trips_pages() {
        let dir = temp_dir("roundtrip");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert(
                "bafyA/x",
                &page("bafyA/x", "Title A", &[("alpha", 2), ("beta", 1)]),
                100,
            )
            .unwrap();
        store
            .upsert(
                "bafyA/x",
                &page("bafyA/x", "Title A2", &[("alpha", 3)]),
                200,
            )
            .unwrap();
        drop(store);

        let rows = Store::open(&dir, 0).unwrap().load_all().unwrap();
        assert_eq!(rows.len(), 1, "upsert replaces");
        let (item, result, at) = &rows[0];
        assert_eq!(
            (item.as_str(), result.title.as_str(), *at),
            ("bafyA/x", "Title A2", 200)
        );
        assert_eq!(result.excerpt, "an excerpt");
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(
            postings_by_item(&store, "alpha"),
            vec![("bafyA/x".to_string(), 3, 3)]
        );
        assert!(
            store.postings("beta").unwrap().is_empty(),
            "replaced, not merged"
        );
        assert_eq!(store.term_count(), 1, "counted again on open");
    }

    #[test]
    fn delete_erases_the_text_from_the_file() {
        let dir = temp_dir("erase");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("bafyB", &page("bafyB", "ZZQX-takedown-title-77", &[]), 1)
            .unwrap();
        assert!(contains(
            &std::fs::read(store.path().unwrap()).unwrap(),
            "ZZQX-takedown-title-77"
        ));

        assert_eq!(store.delete(&["bafyB".to_string()], true).unwrap(), 1);
        assert!(store.load_all().unwrap().is_empty());
        assert!(
            !contains(
                &std::fs::read(store.path().unwrap()).unwrap(),
                "ZZQX-takedown-title-77"
            ),
            "secure_delete must zero the deleted row in the file"
        );
    }

    #[test]
    fn without_secure_delete_the_text_would_remain() {
        // control: proves the assertion above can fail
        let dir = temp_dir("control");
        let store = Store::open(&dir, 0).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .pragma_update(None, "secure_delete", false)
            .unwrap();
        store
            .upsert("bafyC", &page("bafyC", "ZZQX-control-title-88", &[]), 1)
            .unwrap();
        store.delete(&["bafyC".to_string()], true).unwrap();
        assert!(contains(
            &std::fs::read(store.path().unwrap()).unwrap(),
            "ZZQX-control-title-88"
        ));
    }

    #[test]
    fn rollback_journal_never_touches_disk() {
        // review #2: an on-disk journal holds plaintext copies of pages, then is merely unlinked
        let dir = temp_dir("journal");
        let store = Store::open(&dir, 0).unwrap();
        let mode: String = store
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "memory");
        let sync: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 0, "OFF");
        store.upsert("bafyJ", &page("bafyJ", "j", &[]), 1).unwrap();
        store.delete(&["bafyJ".to_string()], true).unwrap();
        assert!(!dir.join("index.sqlite-journal").exists());
        assert!(!dir.join("index.sqlite-wal").exists());
    }

    #[test]
    fn search_reads_while_a_write_is_in_progress() {
        let store = Store::open(&temp_dir("reader"), 0).unwrap();
        store
            .upsert("bafyR", &page("bafyR", "r", &[("word", 2)]), 1)
            .unwrap();
        // a crawl write mid-transaction, holding the write connection
        let mut writer = store.conn.lock().unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute("DELETE FROM pages", []).unwrap();
        assert_eq!(
            postings_by_item(&store, "word"),
            vec![("bafyR".to_string(), 2, 2)],
            "the last committed state, without waiting"
        );
    }

    #[test]
    fn dead_versions_are_read_while_a_write_is_in_progress() {
        let store = std::sync::Arc::new(Store::open(&temp_dir("reader-dead"), 0).unwrap());
        store.upsert("bafyD", &page("bafyD", "d", &[]), 1).unwrap();
        store.upsert("bafyD", &page("bafyD", "d", &[]), 2).unwrap();
        // a crawl write holding the write connection
        let writer = store.conn.lock().unwrap();
        let (sent, got) = std::sync::mpsc::channel();
        let reader = store.clone();
        std::thread::spawn(move || sent.send(reader.dead_versions().unwrap().len()));
        let found = got.recv_timeout(Duration::from_secs(2));
        drop(writer);
        assert_eq!(found, Ok(1), "the purge's scan doesn't wait for the crawl");
    }

    #[test]
    fn finds_items_older_than_a_cutoff() {
        let store = Store::open(&temp_dir("age"), 0).unwrap();
        store.upsert("old", &page("old", "o", &[]), 10).unwrap();
        store.upsert("new", &page("new", "n", &[]), 1000).unwrap();
        assert_eq!(
            store.items_older_than(500).unwrap(),
            vec!["old".to_string()]
        );
    }

    #[test]
    fn open_fails_on_a_file_or_garbage() {
        let dir = temp_dir("bad");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(Store::open(&file, 0).is_err(), "DATA_DIR that is a file");

        std::fs::write(
            dir.join("index.sqlite"),
            b"this is not a sqlite database at all, 32+ bytes",
        )
        .unwrap();
        assert!(Store::open(&dir, 0).is_err(), "corrupt index.sqlite");
    }

    #[test]
    fn queue_round_trips_in_order_and_ignores_duplicates() {
        let dir = temp_dir("queue");
        let store = Store::open(&dir, 0).unwrap();
        store
            .queue_add(&["b/1".to_string(), "a/2".to_string(), "b/1".to_string()])
            .unwrap();
        store.queue_add(&["c/3".to_string()]).unwrap();
        drop(store);
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(
            store.queue_load().unwrap(),
            vec!["b/1", "a/2", "c/3"],
            "FIFO, no duplicates"
        );
        store.queue_remove(&["a/2".to_string()]).unwrap();
        assert_eq!(store.queue_load().unwrap(), vec!["b/1", "c/3"]);
        assert_eq!(store.queue_len(), 2);
        assert_eq!(store.queue_pop().unwrap().as_deref(), Some("b/1"));
        assert_eq!(store.queue_pop().unwrap().as_deref(), Some("c/3"));
        assert_eq!(store.queue_pop().unwrap(), None);
        assert_eq!(store.queue_len(), 0);
    }

    fn hashed(item: &str, words: &[(&str, u32)], hash: &[u8]) -> IndexResult {
        page(item, item, words).with_text_hash(hash.to_vec())
    }

    #[test]
    fn a_page_with_the_same_text_is_recorded_as_a_mirror() {
        let dir = temp_dir("mirror");
        let store = Store::open(&dir, 0).unwrap();
        let up = |item: &str, words: &[(&str, u32)], hash: &[u8]| {
            store.upsert(item, &hashed(item, words, hash), 1).unwrap()
        };
        assert_eq!(up("bafyA/x", &[("alpha", 1)], b"h1"), Upserted::Indexed);
        assert_eq!(
            up("bafyB/x", &[("alpha", 1), ("beta", 1)], b"h1"),
            Upserted::Mirror("bafyA/x".to_string())
        );
        assert_eq!(
            up("bafyA/x", &[("alpha", 2)], b"h1"),
            Upserted::Indexed,
            "itself"
        );
        assert_eq!(up("bafyC", &[], b"h2"), Upserted::Indexed);
        // pages without a hash (indexed before #253) never match
        store.upsert("bafyD", &page("bafyD", "d", &[]), 1).unwrap();
        assert_eq!(
            store.upsert("bafyE", &page("bafyE", "e", &[]), 1).unwrap(),
            Upserted::Indexed
        );

        assert_eq!(
            store.load_all().unwrap().len(),
            4,
            "the mirror isn't indexed"
        );
        assert_eq!(
            postings_by_item(&store, "alpha"),
            vec![("bafyA/x".to_string(), 2, 2)]
        );
        assert_eq!(store.term_count(), 1, "nor are its terms");
        assert_eq!(
            store.mirrors_load().unwrap(),
            vec![("bafyB/x".to_string(), "bafyA/x".to_string())]
        );
        drop(store);

        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(store.mirrors_load().unwrap().len(), 1, "persisted");
        assert_eq!(store.delete(&["bafyA/x".to_string()], true).unwrap(), 1);
        assert!(
            store.mirrors_load().unwrap().is_empty(),
            "deleted with its page"
        );
        // with the original gone, the same text is indexed again
        assert_eq!(
            store
                .upsert("bafyB/x", &hashed("bafyB/x", &[], b"h1"), 1)
                .unwrap(),
            Upserted::Indexed
        );
    }

    #[test]
    fn a_page_whose_text_comes_to_match_another_becomes_a_mirror() {
        let store = Store::open(&temp_dir("remirror"), 0).unwrap();
        let up = |item: &str, words: &[(&str, u32)], hash: &[u8]| {
            store.upsert(item, &hashed(item, words, hash), 1).unwrap()
        };
        up("bafyA", &[("alpha", 1)], b"old");
        up("bafyM", &[], b"old"); // a mirror of A
        up("bafyB", &[("beta", 3)], b"new");
        // A is re-crawled and now has B's text
        assert_eq!(
            up("bafyA", &[("beta", 3)], b"new"),
            Upserted::Mirror("bafyB".to_string())
        );
        assert_eq!(store.load_all().unwrap().len(), 1);
        assert_eq!(store.average_length(), 3.0, "A's length went with it");
        assert!(store.postings("alpha").unwrap().is_empty());
        assert_eq!(store.term_count(), 1);
        let mut mirrors = store.mirrors_load().unwrap();
        mirrors.sort();
        assert_eq!(
            mirrors,
            vec![
                ("bafyA".to_string(), "bafyB".to_string()),
                ("bafyM".to_string(), "bafyB".to_string())
            ]
        );
        // and a mirror whose text changes is indexed, no longer a mirror
        assert_eq!(up("bafyM", &[], b"other"), Upserted::Indexed);
        assert_eq!(store.mirrors_load().unwrap().len(), 1);
        assert_eq!(
            store.delete(&["bafyA".to_string()], true).unwrap(),
            0,
            "only a mirror"
        );
        assert!(store.mirrors_load().unwrap().is_empty());
    }

    #[test]
    fn adds_the_text_hash_column_to_an_existing_index() {
        // the schema between #231 and #253
        let dir = temp_dir("texthash");
        Connection::open(dir.join("index.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE pages (id INTEGER PRIMARY KEY, item TEXT NOT NULL UNIQUE,
                   cid TEXT NOT NULL, title TEXT NOT NULL, excerpt TEXT NOT NULL,
                   indexed_at INTEGER NOT NULL);
                 CREATE TABLE postings (term TEXT NOT NULL, page INTEGER NOT NULL,
                   count INTEGER NOT NULL, PRIMARY KEY (term, page)) WITHOUT ROWID;
                 INSERT INTO pages VALUES (1, 'bafyA', 'bafyA', 'A', 'e', 5);",
            )
            .unwrap();
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(store.load_all().unwrap().len(), 1);
        assert_eq!(
            store
                .upsert("bafyB", &hashed("bafyB", &[], b"h"), 1)
                .unwrap(),
            Upserted::Indexed
        );
        drop(store);
        assert_eq!(Store::open(&dir, 0).unwrap().load_all().unwrap().len(), 2);
    }

    #[test]
    fn tracks_page_lengths_across_upserts_deletes_and_reopens() {
        let dir = temp_dir("lengths");
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(store.average_length(), 0.0);
        store
            .upsert("a", &page("a", "a", &[("x", 2), ("y", 4)]), 1)
            .unwrap();
        store.upsert("b", &page("b", "b", &[("x", 2)]), 1).unwrap();
        assert_eq!(store.average_length(), 4.0, "(6 + 2) / 2");
        assert_eq!(
            postings_by_item(&store, "x")
                .into_iter()
                .find(|p| p.0 == "a"),
            Some(("a".to_string(), 2, 6))
        );
        // a re-crawl replaces the length rather than adding a page
        store.upsert("a", &page("a", "a", &[("x", 1)]), 2).unwrap();
        assert_eq!(store.average_length(), 1.5, "(1 + 2) / 2");
        store.delete(&["b".to_string()], true).unwrap();
        assert_eq!(store.average_length(), 1.0);
        drop(store);
        assert_eq!(Store::open(&dir, 0).unwrap().average_length(), 1.0);
    }

    #[test]
    fn migrates_pages_without_a_length() {
        // the schema between #231 and #251
        let dir = temp_dir("migrate-length");
        let conn = Connection::open(dir.join("index.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE pages (id INTEGER PRIMARY KEY, item TEXT NOT NULL UNIQUE,
               cid TEXT NOT NULL, title TEXT NOT NULL, excerpt TEXT NOT NULL,
               indexed_at INTEGER NOT NULL);
             CREATE TABLE postings (term TEXT NOT NULL, page INTEGER NOT NULL,
               count INTEGER NOT NULL, PRIMARY KEY (term, page)) WITHOUT ROWID;
             INSERT INTO pages VALUES (1, 'bafyA', 'bafyA', 'A', 'e', 5);
             INSERT INTO pages VALUES (2, 'bafyB', 'bafyB', 'B', 'e', 6);
             INSERT INTO postings VALUES ('alpha', 1, 2), ('beta', 1, 3), ('alpha', 2, 1);",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&dir, 0).unwrap();
        let mut alpha = postings_by_item(&store, "alpha");
        alpha.sort();
        assert_eq!(
            alpha,
            vec![("bafyA".to_string(), 2, 5), ("bafyB".to_string(), 1, 1)]
        );
        assert_eq!(store.average_length(), 3.0);
        assert!(!dir_has_table(&dir, "postings"));
    }

    #[test]
    fn decode_skips_malformed_keyword_lines() {
        let decoded = decode_keywords("good 3\nno-count\nbad x\n\nalso 1");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.get("good"), Some(&3));
    }

    #[test]
    fn migrates_the_old_keywords_column_into_postings() {
        // the schema before #231, with counts saved before #209 normalization
        let dir = temp_dir("migrate");
        let conn = Connection::open(dir.join("index.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE pages (item TEXT PRIMARY KEY, cid TEXT NOT NULL, title TEXT NOT NULL,
               excerpt TEXT NOT NULL, keywords TEXT NOT NULL, indexed_at INTEGER NOT NULL);
             CREATE TABLE queue (item TEXT PRIMARY KEY);
             INSERT INTO pages VALUES ('bafyA', 'bafyA', 'A', 'e', 'files. 2\nFiles 1\nalpha 1\n', 5);
             INSERT INTO pages VALUES ('bafyB', 'bafyB', 'B', 'e', 'alpha 4\n', 6);
             INSERT INTO queue VALUES ('bafyQ');",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(
            postings_by_item(&store, "files"),
            vec![("bafyA".to_string(), 3, 4)]
        );
        let mut alpha = postings_by_item(&store, "alpha");
        alpha.sort();
        assert_eq!(
            alpha,
            vec![("bafyA".to_string(), 1, 4), ("bafyB".to_string(), 4, 4)]
        );
        assert_eq!(store.term_count(), 2);
        assert_eq!(store.average_length(), 4.0);
        assert_eq!(store.top_terms(1).unwrap(), vec![("alpha".to_string(), 2)]);
        assert_eq!(store.load_all().unwrap().len(), 2);
        assert_eq!(store.queue_load().unwrap(), vec!["bafyQ"]);
        assert!(!dir_has_table(&dir, "postings"));
        // and re-opening doesn't migrate again
        drop(store);
        assert_eq!(Store::open(&dir, 0).unwrap().term_count(), 2);
    }

    #[test]
    fn a_recrawl_gets_a_new_id_and_the_old_one_is_dead() {
        let store = Store::open(&temp_dir("recrawl-id"), 0).unwrap();
        store
            .upsert("a", &page("a", "a", &[("old", 1)]), 1)
            .unwrap();
        let first = store.postings("old").unwrap()[0].0;
        store
            .upsert("a", &page("a", "a", &[("new", 1)]), 2)
            .unwrap();
        let second = store.postings("new").unwrap()[0].0;
        assert!(second > first, "ids are never reused");
        assert!(store.postings("old").unwrap().is_empty());
        let dead: Vec<u64> = store.dead_versions().unwrap().iter().map(|v| v.0).collect();
        assert_eq!(dead, vec![first]);
    }

    #[test]
    fn a_clean_reopen_keeps_buffered_pages_and_a_crash_requeues_them() {
        let dir = temp_dir("unflushed");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("kept", &page("kept", "k", &[("alpha", 1)]), 1)
            .unwrap();
        drop(store); // flushes
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(
            postings_by_item(&store, "alpha"),
            vec![("kept".into(), 1, 1)]
        );

        store
            .upsert("lost", &page("lost", "l", &[("beta", 1)]), 1)
            .unwrap();
        std::mem::forget(store); // a crash: the buffer never reaches disk
        let store = Store::open(&dir, 0).unwrap();
        assert!(store.postings("beta").unwrap().is_empty());
        let items: Vec<String> = store.load_all().unwrap().into_iter().map(|r| r.0).collect();
        assert_eq!(items, vec!["kept".to_string()], "its row went too");
        assert_eq!(
            store.queue_load().unwrap(),
            vec!["lost".to_string()],
            "to be crawled again"
        );
    }

    // a page with ZZQX... in its own segment of six, then taken down; nothing may remain on disk
    fn assert_takedown_erases(name: &str, setup: impl Fn(&Store)) {
        let dir = temp_dir(name);
        let store = Store::open(&dir, 0).unwrap();
        setup(&store);
        store.delete(&["victim".to_string()], true).unwrap();
        assert!(
            store.postings("zzqxsegmentterm91").unwrap().is_empty(),
            "hidden at once"
        );
        store.flush().unwrap(); // a buffered dead page must not be written by this
        store.maintain();
        assert!(
            !dir_has(&dir, "zzqxsegmentterm91"),
            "{}: still on disk",
            name
        );
        assert_eq!(
            store.postings("alpha").unwrap().len(),
            5,
            "{}: others kept",
            name
        );
    }

    fn five_others(store: &Store) {
        for i in 0..5 {
            let item = format!("other{}", i);
            store
                .upsert(&item, &page(&item, "o", &[("alpha", 1)]), 1)
                .unwrap();
        }
    }

    #[test]
    fn a_takedown_erases_a_buffered_page() {
        assert_takedown_erases("erase-buffered", |store| {
            store
                .upsert(
                    "victim",
                    &page("victim", "v", &[("zzqxsegmentterm91", 1)]),
                    1,
                )
                .unwrap();
            five_others(store);
        });
    }

    #[test]
    fn a_takedown_erases_a_flushed_page() {
        assert_takedown_erases("erase-flushed", |store| {
            store
                .upsert(
                    "victim",
                    &page("victim", "v", &[("zzqxsegmentterm91", 1)]),
                    1,
                )
                .unwrap();
            five_others(store);
            store.flush().unwrap();
        });
    }

    #[test]
    fn a_takedown_erases_a_merged_page() {
        assert_takedown_erases("erase-merged", |store| {
            store
                .upsert(
                    "victim",
                    &page("victim", "v", &[("zzqxsegmentterm91", 1)]),
                    1,
                )
                .unwrap();
            five_others(store);
            store.flush().unwrap();
            for i in 0..9 {
                let item = format!("filler{}", i);
                store
                    .upsert(&item, &page(&item, "f", &[("filler", 1)]), 1)
                    .unwrap();
                store.flush().unwrap();
            }
            assert_eq!(store.segments.segment_count(), 10);
            store.maintain(); // ten segments merge into one
            assert_eq!(store.segments.segment_count(), 1, "merged");
        });
    }

    #[test]
    fn a_takedown_erases_an_earlier_version() {
        assert_takedown_erases("erase-version", |store| {
            store
                .upsert(
                    "victim",
                    &page("victim", "v", &[("zzqxsegmentterm91", 1)]),
                    1,
                )
                .unwrap();
            five_others(store);
            store.flush().unwrap();
            // the term is only in the old version now, in a segment that is 1/6 dead
            store
                .upsert("victim", &page("victim", "v", &[("beta", 1)]), 2)
                .unwrap();
            store.flush().unwrap();
            store.maintain();
            assert!(dir_has(
                store.path().unwrap().parent().unwrap(),
                "zzqxsegmentterm91"
            ));
        });
    }

    #[test]
    fn an_erase_interrupted_by_a_crash_finishes_on_open() {
        let dir = temp_dir("erase-crash");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert(
                "victim",
                &page("victim", "v", &[("zzqxsegmentterm91", 1)]),
                1,
            )
            .unwrap();
        five_others(&store);
        store.flush().unwrap();
        store.delete(&["victim".to_string()], true).unwrap();
        std::mem::forget(store); // before the upkeep thread got to it
        assert!(dir_has(&dir, "zzqxsegmentterm91"));
        Store::open(&dir, 0).unwrap();
        assert!(!dir_has(&dir, "zzqxsegmentterm91"));
    }

    #[test]
    fn migrates_postings_into_a_segment_and_out_of_sqlite() {
        // the schema between #251 and #247
        let dir = temp_dir("migrate-segments");
        let conn = Connection::open(dir.join("index.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE pages (id INTEGER PRIMARY KEY, item TEXT NOT NULL UNIQUE,
               cid TEXT NOT NULL, title TEXT NOT NULL, excerpt TEXT NOT NULL,
               indexed_at INTEGER NOT NULL, length INTEGER NOT NULL DEFAULT 0, text_hash BLOB);
             CREATE TABLE postings (term TEXT NOT NULL, page INTEGER NOT NULL,
               count INTEGER NOT NULL, PRIMARY KEY (term, page)) WITHOUT ROWID;
             CREATE TABLE terms (term TEXT PRIMARY KEY, pages INTEGER NOT NULL) WITHOUT ROWID;
             INSERT INTO pages VALUES (4, 'a', 'a', 't', 'e', 1, 3, NULL),
                                      (9, 'b', 'b', 't', 'e', 1, 1, NULL);
             INSERT INTO postings VALUES ('zzqxmigratedterm', 4, 2), ('shared', 4, 1),
                                         ('shared', 9, 1);
             INSERT INTO terms VALUES ('zzqxmigratedterm', 1), ('shared', 2);",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(
            postings_by_item(&store, "shared"),
            vec![("a".into(), 1, 3), ("b".into(), 1, 1)]
        );
        assert_eq!(
            postings_by_item(&store, "zzqxmigratedterm"),
            vec![("a".into(), 2, 3)]
        );
        assert!(!dir_has_table(&dir, "postings") && !dir_has_table(&dir, "terms"));
        assert!(!contains(
            &fs::read(dir.join("index.sqlite")).unwrap(),
            "zzqxmigratedterm"
        ));
        // ids kept, never reused: not even the highest one's, once its page is gone
        store.delete(&["b".to_string()], false).unwrap();
        store.upsert("c", &page("c", "c", &[]), 1).unwrap();
        let c = store.items(&[10]).unwrap();
        assert_eq!(c.get(&10).map(String::as_str), Some("c"));
    }

    #[test]
    fn ids_never_repeat_when_sqlite_falls_behind_the_segments() {
        let dir = temp_dir("behind");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("old", &page("old", "o", &[("oldterm", 1)]), 1)
            .unwrap();
        let old = store.postings("oldterm").unwrap()[0].0;
        store.flush().unwrap();
        drop(store);
        // index.sqlite lost, segments/ kept
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("index.sqlite")
            {
                fs::remove_file(path).unwrap();
            }
        }
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("new", &page("new", "n", &[("newterm", 1)]), 1)
            .unwrap();
        let new = store.postings("newterm").unwrap()[0].0;
        assert!(new > old, "{} should be above {}", new, old);
        assert_eq!(
            postings_by_item(&store, "newterm"),
            vec![("new".into(), 1, 1)]
        );
        assert!(
            store.items(&[old]).unwrap().is_empty(),
            "no item has the old id"
        );
    }

    #[test]
    fn ids_never_repeat_when_a_rolled_back_sqlite_has_a_sequence_row() {
        let dir = temp_dir("behind-seq");
        let store = Store::open(&dir, 0).unwrap();
        for i in 0..5 {
            let item = format!("p{}", i);
            store
                .upsert(&item, &page(&item, "p", &[("oldterm", 1)]), 1)
                .unwrap();
        }
        let old = store
            .postings("oldterm")
            .unwrap()
            .iter()
            .map(|p| p.0)
            .max()
            .unwrap();
        store.flush().unwrap();
        drop(store);
        // a rollback: the pages are gone and the sequence is behind the segments
        let conn = Connection::open(dir.join("index.sqlite")).unwrap();
        conn.execute_batch(
            "DELETE FROM pages; UPDATE sqlite_sequence SET seq = 1 WHERE name = 'pages';",
        )
        .unwrap();
        drop(conn);
        drop(Store::open(&dir, 0).unwrap());
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("new", &page("new", "n", &[("newterm", 1)]), 1)
            .unwrap();
        let new = store.postings("newterm").unwrap()[0].0;
        assert!(new > old, "{} should be above {}", new, old);
        assert_eq!(
            postings_by_item(&store, "newterm"),
            vec![("new".into(), 1, 1)]
        );
        let rows: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sqlite_sequence WHERE name = 'pages'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn recovery_requeues_the_mirrors_of_unflushed_pages() {
        let dir = temp_dir("recover-mirrors");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("lost", &hashed("lost", &[("beta", 1)], b"h"), 1)
            .unwrap();
        store
            .upsert("copy", &hashed("copy", &[("beta", 1)], b"h"), 1)
            .unwrap();
        std::mem::forget(store);
        let store = Store::open(&dir, 0).unwrap();
        assert!(store.mirrors_load().unwrap().is_empty());
        let mut queued = store.queue_load().unwrap();
        queued.sort();
        assert_eq!(queued, vec!["copy".to_string(), "lost".to_string()]);
    }

    #[test]
    fn a_rolled_back_sqlite_leaves_no_orphan_text_in_the_segments() {
        let dir = temp_dir("behind-orphans");
        let store = Store::open(&dir, 0).unwrap();
        for i in 0..5 {
            let item = format!("p{}", i);
            store
                .upsert(&item, &page(&item, "p", &[("zzqxorphanterm", 1)]), 1)
                .unwrap();
        }
        store.flush().unwrap();
        drop(store);
        // a rollback: the pages and their ids are gone from SQLite, but not from the segments
        let conn = Connection::open(dir.join("index.sqlite")).unwrap();
        conn.execute_batch(
            "DELETE FROM pages; UPDATE sqlite_sequence SET seq = 1 WHERE name = 'pages';",
        )
        .unwrap();
        drop(conn);
        let store = Store::open(&dir, 0).unwrap();
        store.maintain();
        assert!(
            !dir_has(&dir, "zzqxorphanterm"),
            "no page refers to them, so nothing else would ever erase them"
        );
        assert!(store.postings("zzqxorphanterm").unwrap().is_empty());
        let rows: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM dead_pages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "erased, so their records went too");
    }
}
