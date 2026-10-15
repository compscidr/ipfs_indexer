use crate::index_result::IndexResult;
use crate::segments::{erase_file, Maintained, Segments};
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
    // "already indexed?" checks for every link the crawl finds (#231), on their own connection so
    // they don't wait behind a search on `reader`
    lookup: Option<Mutex<Connection>>,
    // long reads (the denylist purge's scan, /admin/recrawl), so they don't hold up either
    scan: Option<Mutex<Connection>>,
    // None for an in-memory store (no DATA_DIR)
    path: Option<PathBuf>,
    // the inverted index (#247)
    segments: Segments,
    // rows in `queue`, kept here since counting them for every /status is a full scan
    queued: AtomicUsize,
    // rows in `pages` and the sum of their lengths, for BM25's average page length (#251)
    pages: AtomicUsize,
    lengths: AtomicU64,
    // rows in `mirrors`, for /status (#253)
    mirrors: AtomicUsize,
}

/**
 * The most items one transaction deletes or queues. The rollback journal is in memory, so a
 * transaction's size is RAM: one over a whole expiry, purge or recrawl could reach gigabytes,
 * and being killed mid-commit with the journal gone corrupts the file.
 */
pub(crate) const WRITE_BATCH: usize = 1000;

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

// indexed pages and queued items per root CID, kept in step with them for /roots (#231). Apart
// from SCHEMA: an index from before it creates it in the same transaction that fills it.
const ROOTS_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS roots (
        root   TEXT PRIMARY KEY,
        pages  INTEGER NOT NULL,
        queued INTEGER NOT NULL
    );";

impl Store {
    /// Opens DATA_DIR's index; SQLite keeps at most cache_mb of it in memory
    pub fn open(dir: &Path, cache_mb: u32) -> Result<Store, OpenError> {
        let path = dir.join("index.sqlite");
        let conn = Connection::open(&path)?;
        let segments = Segments::open(&dir.join("segments"))?;
        let mut store = Store::init(conn, Some(path.clone()), cache_mb, segments)?;
        // default cache_size, not cache_mb: SQLite drops a connection's cache whenever another
        // one commits, which the crawl does constantly, so a big one here would rarely be warm
        let read_only = || -> rusqlite::Result<Mutex<Connection>> {
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            conn.busy_timeout(BUSY_TIMEOUT)?;
            Ok(Mutex::new(conn))
        };
        store.reader = Some(read_only()?);
        store.lookup = Some(read_only()?);
        store.scan = Some(read_only()?);
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
        // sorts and temp tables stay in RAM: a temp file would hold plaintext
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // a commit waits for the reader's queries to finish rather than failing
        conn.busy_timeout(BUSY_TIMEOUT)?;
        if cache_mb > 0 {
            // negative: in KiB rather than pages
            conn.pragma_update(None, "cache_size", -(cache_mb as i64) * 1024)?;
        }
        refuse_old_format(&conn)?;
        let has_roots: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'roots'")?
            .exists([])?;
        let has_pages: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'pages'")?
            .exists([])?;
        conn.execute_batch(SCHEMA)?;
        if has_pages && !has_roots {
            fill_roots(&mut conn)?;
        } else {
            conn.execute_batch(ROOTS_SCHEMA)?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS pages_text_hash ON pages (text_hash);
             CREATE INDEX IF NOT EXISTS pages_indexed_at ON pages (indexed_at);
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
        let mirrors: i64 = conn.query_row("SELECT count(*) FROM mirrors", [], |r| r.get(0))?;
        let (pages, lengths): (i64, i64) = conn.query_row(
            "SELECT count(*), coalesce(sum(length), 0) FROM pages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
            reader: None,
            lookup: None,
            scan: None,
            path,
            segments,
            queued: AtomicUsize::new(queued as usize),
            pages: AtomicUsize::new(pages as usize),
            lengths: AtomicU64::new(lengths as u64),
            mirrors: AtomicUsize::new(mirrors as usize),
        })
    }

    fn read_conn(&self) -> MutexGuard<'_, Connection> {
        self.reader.as_ref().unwrap_or(&self.conn).lock().unwrap()
    }

    fn lookup_conn(&self) -> MutexGuard<'_, Connection> {
        self.lookup.as_ref().unwrap_or(&self.conn).lock().unwrap()
    }

    fn scan_conn(&self) -> MutexGuard<'_, Connection> {
        self.scan.as_ref().unwrap_or(&self.conn).lock().unwrap()
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
                let replaced = tx
                    .prepare_cached("DELETE FROM mirrors WHERE item = ?1")?
                    .execute([item])?;
                tx.prepare_cached("INSERT INTO mirrors (item, page) VALUES (?1, ?2)")?
                    .execute(params![item, page])?;
                tx.commit()?;
                drop(conn);
                self.mirrors.fetch_add(1 - replaced, Ordering::Relaxed);
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
        let unmirrored = tx
            .prepare_cached("DELETE FROM mirrors WHERE item = ?1")?
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
        bump_root(&tx, item, 1, 0)?;
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
        self.mirrors.fetch_sub(unmirrored, Ordering::Relaxed);
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
        bump_root(tx, item, -1, 0)?;
        Ok(Some((id, length as u64)))
    }

    // removes pages from the running page count and length total
    fn adjust_lengths(&self, pages: usize, length: u64) {
        self.pages.fetch_sub(pages, Ordering::Relaxed);
        self.lengths.fetch_sub(length, Ordering::Relaxed);
    }

    /**
     * Deletes pages and their mirrors (items that are mirrors themselves are dropped too);
     * returns how many pages went. They leave search at once. With `erase` (takedowns, expiry),
     * the segment files holding them or any earlier version of them are rewritten without them
     * promptly (#249). Commits every WRITE_BATCH items, so an error part-way leaves the earlier
     * batches deleted: callers retry the whole list (the next expiry or purge, daily, on a
     * list change or at startup, picks up what's left).
     */
    pub fn delete(&self, items: &[String], erase: bool) -> rusqlite::Result<usize> {
        let mut deleted = 0;
        for batch in items.chunks(WRITE_BATCH) {
            deleted += self.delete_batch(batch, erase)?;
        }
        Ok(deleted)
    }

    fn delete_batch(&self, items: &[String], erase: bool) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (mut dead, mut erased, mut length) = (Vec::new(), Vec::new(), 0);
        let mut unmirrored = 0;
        {
            let mut mirror = tx.prepare("DELETE FROM mirrors WHERE item = ?1")?;
            let mut mirrors_of = tx.prepare("DELETE FROM mirrors WHERE page = ?1")?;
            let mut versions =
                tx.prepare("UPDATE dead_pages SET erase = 1 WHERE item = ?1 RETURNING id")?;
            for item in items {
                unmirrored += mirror.execute([item])?;
                if let Some((id, page_length)) = self.remove_page(&tx, item)? {
                    dead.push(id as u64);
                    length += page_length;
                    unmirrored += mirrors_of.execute([id])?;
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
        self.mirrors.fetch_sub(unmirrored, Ordering::Relaxed);
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

    /// The live pages a term is on, as (page id, how often it appears there, the page's length)
    pub fn postings(&self, term: &str) -> std::io::Result<Vec<(u64, u32, u32)>> {
        self.segments.postings(term)
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

    /// Adds items to the frontier, committing every WRITE_BATCH items (an error part-way keeps
    /// the batches before it); ones already there are ignored. Returns how many were new.
    pub fn queue_add(&self, items: &[String]) -> rusqlite::Result<usize> {
        let start = Instant::now();
        let (mut added, mut waited) = (0, Duration::ZERO);
        for batch in items.chunks(WRITE_BATCH) {
            let locking = Instant::now();
            let mut conn = self.conn.lock().unwrap();
            waited += locking.elapsed();
            let tx = conn.transaction()?;
            let mut batch_added = 0;
            {
                let mut stmt =
                    tx.prepare_cached("INSERT OR IGNORE INTO queue (item) VALUES (?1)")?;
                for item in batch {
                    let n = stmt.execute([item])?;
                    bump_root(&tx, item, 0, n as i64)?;
                    batch_added += n;
                }
            }
            tx.commit()?;
            self.queued.fetch_add(batch_added, Ordering::Relaxed);
            added += batch_added;
        }
        info!(
            "Store queue_add: {} of {} new, waited {:?}, took {:?}",
            added,
            items.len(),
            waited,
            start.elapsed() - waited
        );
        Ok(added)
    }

    // startup's queue check was its only caller (#231); kept, roots and all, for tests
    #[cfg(test)]
    pub fn queue_remove(&self, items: &[String]) -> rusqlite::Result<()> {
        for batch in items.chunks(WRITE_BATCH) {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn.transaction()?;
            let mut removed = 0;
            {
                let mut stmt = tx.prepare_cached("DELETE FROM queue WHERE item = ?1")?;
                for item in batch {
                    let n = stmt.execute([item])?;
                    bump_root(&tx, item, 0, -(n as i64))?;
                    removed += n;
                }
            }
            tx.commit()?;
            self.queued.fetch_sub(removed, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Takes the oldest item off the frontier, or None if it's empty
    pub fn queue_pop(&self) -> rusqlite::Result<Option<String>> {
        let start = Instant::now();
        let mut conn = self.conn.lock().unwrap();
        let waited = start.elapsed();
        let tx = conn.transaction()?;
        let item: Option<String> = tx
            .prepare_cached(
                "DELETE FROM queue WHERE rowid = (SELECT min(rowid) FROM queue) RETURNING item",
            )?
            .query_row([], |r| r.get(0))
            .optional()?;
        if let Some(item) = &item {
            bump_root(&tx, item, 0, -1)?;
        }
        tx.commit()?;
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

    /// Indexed and queued counts per root CID with the root page's title and cid ("" if it
    /// isn't indexed), most indexed first (#232); from `roots`, no scan (#231)
    pub fn root_counts(&self) -> rusqlite::Result<Vec<(String, usize, usize, String, String)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare_cached(
            "SELECT roots.root, roots.pages, roots.queued, coalesce(pages.title, ''),
                    coalesce(pages.cid, '')
             FROM roots LEFT JOIN pages ON pages.item = roots.root
             ORDER BY roots.pages DESC, roots.queued DESC, roots.root",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? as usize,
                r.get::<_, i64>(2)? as usize,
                r.get(3)?,
                r.get(4)?,
            ))
        })?;
        rows.collect()
    }

    pub fn page_count(&self) -> usize {
        self.pages.load(Ordering::Relaxed)
    }

    pub fn mirror_count(&self) -> usize {
        self.mirrors.load(Ordering::Relaxed)
    }

    /// Whether an item is indexed or recorded as a mirror (#231: replaces the in-memory maps)
    pub fn known(&self, item: &str) -> rusqlite::Result<bool> {
        let conn = self.lookup_conn();
        let found = conn
            .prepare_cached(
                "SELECT 1 FROM pages WHERE item = ?1 UNION ALL SELECT 1 FROM mirrors WHERE item = ?1",
            )?
            .exists([item])?;
        Ok(found)
    }

    /// The item and page (without keyword counts) of page ids; ids no longer in `pages` are
    /// left out
    pub fn pages_by_id(
        &self,
        ids: &[u64],
    ) -> rusqlite::Result<HashMap<u64, (String, IndexResult)>> {
        let conn = self.read_conn();
        let mut stmt =
            conn.prepare_cached("SELECT item, cid, title, excerpt FROM pages WHERE id = ?1")?;
        let mut found = HashMap::new();
        for &id in ids {
            let row = stmt
                .query_row([id as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        IndexResult::new(r.get(1)?, r.get(2)?, r.get(3)?, HashMap::new()),
                    ))
                })
                .optional()?;
            if let Some(row) = row {
                found.insert(id, row);
            }
        }
        Ok(found)
    }

    /// Up to n pages after id `after`, as (id, item, cid), in id order: for walking every page
    /// in batches without holding a connection for the whole walk
    pub fn items_after(
        &self,
        after: u64,
        n: usize,
    ) -> rusqlite::Result<Vec<(u64, String, String)>> {
        let conn = self.scan_conn();
        let mut stmt = conn
            .prepare_cached("SELECT id, item, cid FROM pages WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = stmt.query_map(
            params![after as i64, n.min(i64::MAX as usize) as i64],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?)),
        )?;
        rows.collect()
    }

    /// Up to n mirrors after rowid `after`, as (rowid, item), in rowid order
    pub fn mirror_items_after(&self, after: i64, n: usize) -> rusqlite::Result<Vec<(i64, String)>> {
        let conn = self.scan_conn();
        let mut stmt = conn.prepare_cached(
            "SELECT rowid, item FROM mirrors WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after, n.min(i64::MAX as usize) as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect()
    }

    /// Up to n items indexed before `cutoff` (by the indexed_at index). On the write
    /// connection: expiry lists and deletes a batch under the writes lock, so a page re-crawled
    /// meanwhile isn't listed with its old date.
    pub fn older_than(&self, cutoff: i64, n: usize) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT item FROM pages WHERE indexed_at < ?1 LIMIT ?2")?;
        let rows = stmt.query_map(params![cutoff, n.min(i64::MAX as usize) as i64], |r| {
            r.get(0)
        })?;
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
    /// Every page, without keyword counts (those stay on disk)
    pub(crate) fn load_all(&self) -> rusqlite::Result<Vec<(String, IndexResult, i64)>> {
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
    pub(crate) fn mirrors_load(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT mirrors.item, pages.item FROM mirrors JOIN pages ON pages.id = mirrors.page",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// The frontier in the order it was added
    pub(crate) fn queue_load(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item FROM queue ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.segments.segment_count()
    }

    // makes every following read/write fail, to exercise callers' error handling
    pub(crate) fn break_for_test(&self) {
        self.drop_table_for_test("pages");
    }

    // makes only what touches `table` fail
    pub(crate) fn drop_table_for_test(&self, table: &str) {
        self.conn
            .lock()
            .unwrap()
            .execute_batch(&format!("DROP TABLE {}", table))
            .unwrap();
    }
}

/// Wiping and rebuilding is how to upgrade an index file older than this one (see `OldFormat`)
pub const WIPE: &str = "rm -r $DATA_DIR/index.sqlite* $DATA_DIR/segments";

/**
 * An index file from before v0.2.6 (postings still in SQLite, or pages without the columns
 * added since). Converting one in place took hours and, on a 30 GB file, ran out of memory
 * halfway and corrupted it, so it's refused: wipe it and it's rebuilt by crawling.
 */
#[derive(Debug)]
pub struct OldFormat;

impl std::fmt::Display for OldFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "index.sqlite is from before v0.2.6 and can't be converted; wipe it ({}) and it \
             is rebuilt by crawling",
            WIPE
        )
    }
}

impl std::error::Error for OldFormat {}

/// Whether opening failed because the index is damaged (SQLite's "malformed", or a segment or
/// MANIFEST failing its checks), as opposed to a missing directory, permissions or a full disk
pub fn is_corrupt(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(failure, _)) = err.downcast_ref::<rusqlite::Error>()
    {
        return matches!(
            failure.code,
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
        );
    }
    matches!(err.downcast_ref::<std::io::Error>(), Some(io) if io.kind() == std::io::ErrorKind::InvalidData)
}

/**
 * Erases DATA_DIR's index so the next open starts empty: index.sqlite (and anything SQLite left
 * beside it) and every segment file, each zeroed before it's removed. A damaged file may have
 * lost the record of which pages were taken down, so its text gets the same erasure (#249).
 */
pub fn wipe(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let is_index = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("index.sqlite"));
        if is_index && path.is_file() {
            erase_file(&path)?;
        }
    }
    let segments = dir.join("segments");
    if segments.is_dir() {
        for entry in std::fs::read_dir(&segments)? {
            erase_file(&entry?.path())?;
        }
        std::fs::remove_dir(&segments)?;
    }
    Ok(())
}

fn refuse_old_format(conn: &Connection) -> Result<(), OpenError> {
    let has_table = |name: &str| -> rusqlite::Result<bool> {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
            .exists([name])
    };
    if !has_table("pages")? {
        return Ok(()); // a new file
    }
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('pages')")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let has_column = |name: &str| columns.iter().any(|c| c == name);
    if has_table("postings")? || !has_column("length") || !has_column("text_hash") {
        return Err(Box::new(OldFormat));
    }
    Ok(())
}

// a "cid/path" item's root CID
fn root_of(item: &str) -> &str {
    item.split('/').next().unwrap_or(item)
}

// adds to a root's page and queued counts, dropping the row once both are 0
fn bump_root(tx: &Connection, item: &str, pages: i64, queued: i64) -> rusqlite::Result<()> {
    if pages == 0 && queued == 0 {
        return Ok(());
    }
    let root = root_of(item);
    tx.prepare_cached(
        "INSERT INTO roots (root, pages, queued) VALUES (?1, ?2, ?3)
         ON CONFLICT (root) DO UPDATE SET pages = pages + ?2, queued = queued + ?3",
    )?
    .execute(params![root, pages, queued])?;
    tx.prepare_cached("DELETE FROM roots WHERE root = ?1 AND pages <= 0 AND queued <= 0")?
        .execute([root])?;
    Ok(())
}

/**
 * Creates and fills `roots` from `pages` and `queue`, once, for an index from before it
 * (v0.2.9). One transaction, so a fill that's cut short leaves no table and the next open fills
 * it again. It inserts one row per root (a few hundred), so the in-memory journal stays small;
 * the two GROUP BYs sort every row's root in memory (temp_store), about 350 MB at 2M pages and
 * 3M queued, which is fine at that size for a one-time step.
 */
fn fill_roots(conn: &mut Connection) -> rusqlite::Result<()> {
    log::info!("Counting pages and queued items per root (one-time, #231)");
    let tx = conn.transaction()?;
    tx.execute_batch(ROOTS_SCHEMA)?;
    tx.execute_batch(
        "INSERT INTO roots (root, pages, queued)
           SELECT substr(item, 1, instr(item || '/', '/') - 1) AS r, count(*), 0
           FROM pages GROUP BY r;
         INSERT INTO roots (root, pages, queued)
           SELECT substr(item, 1, instr(item || '/', '/') - 1) AS r, 0, count(*)
           FROM queue GROUP BY r
           ON CONFLICT (root) DO UPDATE SET queued = excluded.queued;",
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
    let mirrored: Vec<String> = tx
        .prepare("SELECT item FROM mirrors WHERE page > ?1")?
        .query_map([flushed_through as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    tx.execute(
        "DELETE FROM mirrors WHERE page > ?1",
        [flushed_through as i64],
    )?;
    tx.execute("DELETE FROM pages WHERE id > ?1", [flushed_through as i64])?;
    {
        let mut queue = tx.prepare("INSERT OR IGNORE INTO queue (item) VALUES (?1)")?;
        for item in &items {
            bump_root(&tx, item, -1, 0)?;
        }
        for item in items.iter().chain(&mirrored) {
            let n = queue.execute([item])?;
            bump_root(&tx, item, 0, n as i64)?;
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
        let items = store
            .pages_by_id(&ids)
            .unwrap()
            .into_iter()
            .map(|(id, (item, _))| (id, item))
            .collect::<HashMap<_, _>>();
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
    fn known_finds_pages_and_mirrors_only() {
        let dir = temp_dir("known");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("a/1", &hashed("a/1", &[("x", 1)], b"h"), 1)
            .unwrap();
        store
            .upsert("b/1", &hashed("b/1", &[("x", 1)], b"h"), 1)
            .unwrap(); // mirror
        store.queue_add(&["c/1".into()]).unwrap();
        assert!(store.known("a/1").unwrap());
        assert!(store.known("b/1").unwrap());
        assert!(!store.known("c/1").unwrap(), "queued isn't known");
        assert!(!store.known("zzz").unwrap());
    }

    #[test]
    fn pages_by_id_returns_rows_still_there() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert("a/1", &page("a/1", "Title A", &[("x", 1)]), 1)
            .unwrap();
        store
            .upsert("a/2", &page("a/2", "Title B", &[("x", 1)]), 1)
            .unwrap();
        let ids: Vec<u64> = store
            .items_after(0, 10)
            .unwrap()
            .iter()
            .map(|r| r.0)
            .collect();
        store.delete(&["a/2".to_string()], false).unwrap();
        let found = store.pages_by_id(&ids).unwrap();
        assert_eq!(found.len(), 1);
        let (item, result) = &found[&ids[0]];
        assert_eq!((item.as_str(), result.title.as_str()), ("a/1", "Title A"));
        assert!(result.keywords.is_empty());
    }

    #[test]
    fn items_after_pages_through_in_id_order() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            let item = format!("a/{}", i);
            store
                .upsert(&item, &page(&item, "t", &[("x", 1)]), 1)
                .unwrap();
        }
        let first = store.items_after(0, 2).unwrap();
        let rest = store.items_after(first[1].0, 10).unwrap();
        let items: Vec<String> = first.iter().chain(&rest).map(|r| r.1.clone()).collect();
        assert_eq!(items, vec!["a/0", "a/1", "a/2", "a/3", "a/4"]);
    }

    #[test]
    fn older_than_lists_at_most_n() {
        let store = Store::open_in_memory().unwrap();
        for (item, at) in [("a/1", 1), ("a/2", 2), ("a/3", 9)] {
            store
                .upsert(item, &page(item, "t", &[("x", 1)]), at)
                .unwrap();
        }
        assert_eq!(store.older_than(5, 1).unwrap().len(), 1);
        let mut all = store.older_than(5, 10).unwrap();
        all.sort();
        assert_eq!(all, vec!["a/1", "a/2"]);
    }

    #[test]
    fn finds_items_older_than_a_cutoff() {
        let store = Store::open(&temp_dir("age"), 0).unwrap();
        store.upsert("old", &page("old", "o", &[]), 10).unwrap();
        store.upsert("new", &page("new", "n", &[]), 1000).unwrap();
        assert_eq!(
            store.older_than(500, usize::MAX).unwrap(),
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
    fn deletes_and_queue_changes_bigger_than_a_batch_all_land() {
        let store = Store::open_in_memory().unwrap();
        let n = WRITE_BATCH * 2 + 500;
        let items: Vec<String> = (0..n).map(|i| format!("bafyQ{}", i)).collect();
        assert_eq!(store.queue_add(&items).unwrap(), n);
        assert_eq!(store.queue_len(), n);
        store.queue_remove(&items).unwrap();
        assert_eq!(store.queue_len(), 0);

        for item in &items {
            store
                .upsert(item, &page(item, "t", &[("word", 1)]), 1)
                .unwrap();
        }
        assert_eq!(store.delete(&items, true).unwrap(), n);
        assert!(store.load_all().unwrap().is_empty());
        assert_eq!(store.average_length(), 0.0);
    }

    #[test]
    fn refuses_an_index_from_before_segments_and_leaves_it_alone() {
        let schemas = [
            // before #231: keyword counts as text
            "CREATE TABLE pages (item TEXT PRIMARY KEY, cid TEXT NOT NULL, title TEXT NOT NULL,
               excerpt TEXT NOT NULL, keywords TEXT NOT NULL, indexed_at INTEGER NOT NULL);",
            // #231 to #251: postings in SQLite, no length
            "CREATE TABLE pages (id INTEGER PRIMARY KEY, item TEXT NOT NULL UNIQUE,
               cid TEXT NOT NULL, title TEXT NOT NULL, excerpt TEXT NOT NULL,
               indexed_at INTEGER NOT NULL);
             CREATE TABLE postings (term TEXT NOT NULL, page INTEGER NOT NULL,
               count INTEGER NOT NULL, PRIMARY KEY (term, page)) WITHOUT ROWID;",
            // #251 to #247: every column, postings still in SQLite
            "CREATE TABLE pages (id INTEGER PRIMARY KEY, item TEXT NOT NULL UNIQUE,
               cid TEXT NOT NULL, title TEXT NOT NULL, excerpt TEXT NOT NULL,
               indexed_at INTEGER NOT NULL, length INTEGER NOT NULL DEFAULT 0, text_hash BLOB);
             CREATE TABLE postings (term TEXT NOT NULL, page INTEGER NOT NULL,
               count INTEGER NOT NULL, PRIMARY KEY (term, page)) WITHOUT ROWID;",
        ];
        for (i, schema) in schemas.iter().enumerate() {
            let dir = temp_dir(&format!("old-format-{}", i));
            let conn = Connection::open(dir.join("index.sqlite")).unwrap();
            conn.execute_batch(schema).unwrap();
            drop(conn);
            let err = Store::open(&dir, 0).err().expect("an old index is refused");
            assert!(err.is::<OldFormat>(), "schema {}: {}", i, err);
            assert!(!is_corrupt(&*err), "an old index isn't wiped as corrupt");
            assert!(dir_has_table(&dir, "pages"), "schema {} left as it was", i);
        }
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
            store.pages_by_id(&[old]).unwrap().is_empty(),
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

    // roots as a GROUP BY over pages and queue would count them
    fn roots_by_scan(store: &Store) -> Vec<(String, usize, usize)> {
        let conn = store.conn.lock().unwrap();
        let mut counts: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        for (table, col) in [("pages", 0), ("queue", 1)] {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT substr(item, 1, instr(item || '/', '/') - 1), count(*) FROM {} GROUP BY 1",
                    table
                ))
                .unwrap();
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
                })
                .unwrap();
            for row in rows {
                let (root, n) = row.unwrap();
                let entry = counts.entry(root).or_default();
                if col == 0 {
                    entry.0 = n
                } else {
                    entry.1 = n
                }
            }
        }
        counts.into_iter().map(|(r, (p, q))| (r, p, q)).collect()
    }

    fn roots_table(store: &Store) -> Vec<(String, usize, usize)> {
        let mut rows: Vec<_> = store
            .root_counts()
            .unwrap()
            .into_iter()
            .map(|(root, pages, queued, ..)| (root, pages, queued))
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn roots_track_pages_mirrors_deletes_and_the_queue() {
        let dir = temp_dir("roots");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("a", &page("a", "A root", &[("x", 1)]), 1)
            .unwrap();
        store
            .upsert("a/1", &page("a/1", "t", &[("x", 1)]), 1)
            .unwrap();
        store
            .upsert("a/1", &page("a/1", "t", &[("y", 1)]), 2)
            .unwrap(); // re-crawl: new id
        store
            .upsert("b/1", &hashed("b/1", &[("z", 1)], b"same"), 1)
            .unwrap();
        store
            .upsert("c/1", &hashed("c/1", &[("z", 1)], b"same"), 1)
            .unwrap(); // mirror of b/1
        store
            .queue_add(&["a/2".into(), "a/3".into(), "d/1".into(), "a/2".into()])
            .unwrap();
        store.queue_pop().unwrap(); // a/2
        store.queue_remove(&["d/1".into(), "nope".into()]).unwrap();
        store.delete(&["b/1".to_string()], true).unwrap(); // takes its mirror c/1 too
        assert_eq!(roots_table(&store), roots_by_scan(&store));
        assert_eq!(
            store.root_counts().unwrap()[0],
            ("a".to_string(), 2, 1, "A root".to_string(), "a".to_string())
        );
        assert_eq!(store.mirror_count(), 0);
        drop(store);
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(roots_table(&store), roots_by_scan(&store));
    }

    #[test]
    fn roots_match_a_group_by_after_crash_recovery() {
        let dir = temp_dir("roots-recover");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("a/1", &page("a/1", "t", &[("x", 1)]), 1)
            .unwrap();
        store.flush().unwrap();
        store
            .upsert("a/2", &page("a/2", "t", &[("x", 1)]), 1)
            .unwrap(); // never flushed
        store
            .upsert("b/1", &hashed("b/1", &[("z", 1)], b"h"), 1)
            .unwrap();
        store
            .upsert("c/1", &hashed("c/1", &[("z", 1)], b"h"), 1)
            .unwrap(); // mirror of b/1
        std::mem::forget(store); // a crash: no flush on drop
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(roots_table(&store), roots_by_scan(&store));
        assert_eq!(
            store.queue_len(),
            3,
            "a/2, b/1 and its mirror c/1 queued again"
        );
    }

    #[test]
    fn roots_are_filled_once_from_an_index_without_them() {
        let dir = temp_dir("roots-fill");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("a/1", &page("a/1", "t", &[("x", 1)]), 1)
            .unwrap();
        store
            .upsert("a", &page("a", "Root", &[("x", 1)]), 1)
            .unwrap();
        store.queue_add(&["a/2".into(), "b/1".into()]).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE roots")
            .unwrap(); // a v0.2.9 index
        drop(store);
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(
            roots_table(&store),
            vec![("a".to_string(), 2, 1), ("b".to_string(), 0, 1)]
        );
    }

    #[test]
    fn a_roots_fill_cut_short_leaves_no_table_and_the_next_open_fills_it() {
        let dir = temp_dir("roots-fill-fails");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert("a/1", &page("a/1", "t", &[("x", 1)]), 1)
            .unwrap();
        store.queue_add(&["a/2".into()]).unwrap();
        drop(store);
        {
            // a v0.2.9 index whose queue fails to read part-way through the fill
            let conn = Connection::open(dir.join("index.sqlite")).unwrap();
            conn.execute_batch(
                "DROP TABLE roots;
                 ALTER TABLE queue RENAME TO queue_kept;
                 CREATE VIEW queue AS SELECT abs(-9223372036854775808) AS item;",
            )
            .unwrap();
        }
        let err = Store::open(&dir, 0).err().unwrap();
        assert!(err.to_string().contains("integer overflow"), "{}", err);
        assert!(!dir_has_table(&dir, "roots"), "rolled back, table and all");
        {
            let conn = Connection::open(dir.join("index.sqlite")).unwrap();
            conn.execute_batch("DROP VIEW queue; ALTER TABLE queue_kept RENAME TO queue;")
                .unwrap();
        }
        let store = Store::open(&dir, 0).unwrap();
        assert_eq!(roots_table(&store), vec![("a".to_string(), 1, 1)]);
    }

    #[test]
    fn expiry_query_uses_the_indexed_at_index() {
        let store = Store::open_in_memory().unwrap();
        let plan: String = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "EXPLAIN QUERY PLAN SELECT item FROM pages WHERE indexed_at < 5",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("pages_indexed_at"), "{}", plan);
    }
}
