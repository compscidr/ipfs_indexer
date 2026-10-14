use crate::index_queue::normalize;
use crate::index_result::IndexResult;
use log::info;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/**
 * The persisted index (#106): one SQLite file, `index.sqlite`, under DATA_DIR. secure_delete
 * zeroes deleted rows in the file. The rollback journal is kept in memory: an on-disk journal
 * (DELETE or WAL mode) would write plaintext copies of pages and later only unlink them. The
 * trade-off is that a crash mid-write can corrupt the file; the index is rebuildable, so
 * recovery is the documented wipe.
 *
 * The inverted index lives here too, not in RAM (#231): `postings` (term, page, count) and
 * `terms` (term, how many pages have it). Memory is bounded by SQLite's page cache (cache_mb).
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
    // rows in `terms` and `queue`, kept here since counting them for every /status is a full scan
    terms: AtomicUsize,
    queued: AtomicUsize,
    // rows in `pages` and the sum of their lengths, for BM25's average page length (#251)
    pages: AtomicUsize,
    lengths: AtomicU64,
}

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
        id         INTEGER PRIMARY KEY,
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
    -- the inverted index (#231): how often each term appears on each page
    CREATE TABLE IF NOT EXISTS postings (
        term  TEXT NOT NULL,
        page  INTEGER NOT NULL,
        count INTEGER NOT NULL,
        PRIMARY KEY (term, page)
    ) WITHOUT ROWID;
    -- how many pages each term is on (document frequency), for ranking and /keywords
    CREATE TABLE IF NOT EXISTS terms (
        term  TEXT PRIMARY KEY,
        pages INTEGER NOT NULL
    ) WITHOUT ROWID;
    -- the crawl frontier (#224): items waiting to be fetched, in insertion order
    CREATE TABLE IF NOT EXISTS queue (
        item TEXT PRIMARY KEY
    );";

impl Store {
    /// Opens DATA_DIR's index; SQLite keeps at most cache_mb of it in memory
    pub fn open(dir: &Path, cache_mb: u32) -> rusqlite::Result<Store> {
        let path = dir.join("index.sqlite");
        let conn = Connection::open(&path)?;
        let mut store = Store::init(conn, Some(path.clone()), cache_mb)?;
        // default cache_size, not cache_mb: SQLite drops a connection's cache whenever another
        // one commits, which the crawl does constantly, so a big one here would rarely be warm
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        reader.busy_timeout(BUSY_TIMEOUT)?;
        store.reader = Some(Mutex::new(reader));
        Ok(store)
    }

    /// For running without DATA_DIR: nothing persists, and the cache cap doesn't apply
    pub fn open_in_memory() -> rusqlite::Result<Store> {
        Store::init(Connection::open_in_memory()?, None, 0)
    }

    fn init(mut conn: Connection, path: Option<PathBuf>, cache_mb: u32) -> rusqlite::Result<Store> {
        conn.pragma_update(None, "secure_delete", true)?;
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
        // commits go to the OS page cache instead of waiting on fsync: on spinning disks each
        // page's scattered postings writes held the lock for seconds, timing out searches. The
        // memory journal already means a crash can corrupt the file (rebuild); this extends that
        // to power loss.
        conn.pragma_update(None, "synchronous", "OFF")?;
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
        // after the bulk load in the migration: deleting a page's postings needs it
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS postings_page ON postings (page);
             CREATE INDEX IF NOT EXISTS pages_text_hash ON pages (text_hash);
             CREATE INDEX IF NOT EXISTS mirrors_page ON mirrors (page);",
        )?;
        migrate_page_lengths(&mut conn)?;
        let terms: i64 = conn.query_row("SELECT count(*) FROM terms", [], |r| r.get(0))?;
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
            terms: AtomicUsize::new(terms as usize),
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
     * Adds or replaces a page and its postings (from result.keywords) in one transaction. If
     * another item already has the same text (result.text_hash), the page isn't indexed: it's
     * recorded as a mirror of that one instead.
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
                let (terms, length) = match self.remove_page(&tx, item)? {
                    Some((old, terms, length)) => {
                        tx.prepare_cached("UPDATE mirrors SET page = ?2 WHERE page = ?1")?
                            .execute(params![old, page])?;
                        (terms, Some(length))
                    }
                    None => (0, None),
                };
                tx.prepare_cached(
                    "INSERT INTO mirrors (item, page) VALUES (?1, ?2)
                     ON CONFLICT (item) DO UPDATE SET page = excluded.page",
                )?
                .execute(params![item, page])?;
                tx.commit()?;
                self.adjust_terms(terms);
                if let Some(length) = length {
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
        let old_length: Option<i64> = tx
            .prepare_cached("SELECT length FROM pages WHERE item = ?1")?
            .query_row([item], |r| r.get(0))
            .optional()?;
        // ON CONFLICT keeps the row's id, so a re-crawl replaces its postings in place
        let id: i64 = tx.query_row(
            "INSERT INTO pages (item, cid, title, excerpt, indexed_at, length, text_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (item) DO UPDATE SET cid = excluded.cid, title = excluded.title,
               excerpt = excluded.excerpt, indexed_at = excluded.indexed_at,
               length = excluded.length, text_hash = excluded.text_hash
             RETURNING id",
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
        let mut terms = self.remove_postings(&tx, id)?;
        {
            let mut posting =
                tx.prepare_cached("INSERT INTO postings (term, page, count) VALUES (?1, ?2, ?3)")?;
            let mut term = tx.prepare_cached(
                "INSERT INTO terms (term, pages) VALUES (?1, 1)
                 ON CONFLICT (term) DO UPDATE SET pages = pages + 1 RETURNING pages",
            )?;
            for (word, count) in &result.keywords {
                posting.execute(params![word, id, count])?;
                if term.query_row([word], |r| r.get::<_, i64>(0))? == 1 {
                    terms += 1;
                }
            }
        }
        tx.commit()?;
        self.adjust_terms(terms);
        match old_length {
            Some(old) => self.adjust_lengths(0, old as u64),
            None => {
                self.pages.fetch_add(1, Ordering::Relaxed);
            }
        }
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

    // deletes an item's page and postings; returns its id, the change in the number of terms and
    // its length
    fn remove_page(
        &self,
        tx: &Transaction,
        item: &str,
    ) -> rusqlite::Result<Option<(i64, isize, u64)>> {
        let row: Option<(i64, i64)> = tx
            .prepare_cached("DELETE FROM pages WHERE item = ?1 RETURNING id, length")?
            .query_row([item], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match row {
            Some((id, length)) => Ok(Some((id, self.remove_postings(tx, id)?, length as u64))),
            None => Ok(None),
        }
    }

    // deletes a page's postings and decrements their terms (dropping terms no page has any
    // more); returns the change in the number of terms
    fn remove_postings(&self, tx: &Transaction, page: i64) -> rusqlite::Result<isize> {
        let removed: Vec<String> = tx
            .prepare_cached("DELETE FROM postings WHERE page = ?1 RETURNING term")?
            .query_map([page], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut decrement = tx
            .prepare_cached("UPDATE terms SET pages = pages - 1 WHERE term = ?1 RETURNING pages")?;
        let mut drop = tx.prepare_cached("DELETE FROM terms WHERE term = ?1")?;
        let mut change = 0;
        for term in removed {
            if decrement.query_row([&term], |r| r.get::<_, i64>(0))? <= 0 {
                drop.execute([&term])?;
                change -= 1;
            }
        }
        Ok(change)
    }

    fn adjust_terms(&self, change: isize) {
        if change >= 0 {
            self.terms.fetch_add(change as usize, Ordering::Relaxed);
        } else {
            self.terms
                .fetch_sub(change.unsigned_abs(), Ordering::Relaxed);
        }
    }

    // removes pages from the running page count and length total
    fn adjust_lengths(&self, pages: usize, length: u64) {
        self.pages.fetch_sub(pages, Ordering::Relaxed);
        self.lengths.fetch_sub(length, Ordering::Relaxed);
    }

    /**
     * Deletes pages, their postings and their mirrors in one transaction (items that are mirrors
     * themselves are dropped too); returns how many pages went
     */
    pub fn delete(&self, items: &[String]) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (mut deleted, mut terms, mut length) = (0, 0, 0);
        {
            let mut mirror = tx.prepare("DELETE FROM mirrors WHERE item = ?1")?;
            let mut mirrors_of = tx.prepare("DELETE FROM mirrors WHERE page = ?1")?;
            for item in items {
                mirror.execute([item])?;
                if let Some((id, change, page_length)) = self.remove_page(&tx, item)? {
                    deleted += 1;
                    terms += change;
                    length += page_length;
                    mirrors_of.execute([id])?;
                }
            }
        }
        tx.commit()?;
        self.adjust_terms(terms);
        self.adjust_lengths(deleted, length);
        Ok(deleted)
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

    /// The pages a term is on, as (item, how often it appears there, the page's length)
    pub fn postings(&self, term: &str) -> rusqlite::Result<Vec<(String, u32, u32)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare_cached(
            "SELECT pages.item, postings.count, pages.length
             FROM postings JOIN pages ON pages.id = postings.page WHERE postings.term = ?1",
        )?;
        let rows = stmt.query_map([term], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
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
        self.terms.load(Ordering::Relaxed)
    }

    /// The n terms on the most pages
    pub fn top_terms(&self, n: usize) -> rusqlite::Result<Vec<(String, u32)>> {
        let conn = self.read_conn();
        let mut stmt =
            conn.prepare("SELECT term, pages FROM terms ORDER BY pages DESC, term LIMIT ?1")?;
        let rows = stmt.query_map([n as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
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
    log::info!("Computing page lengths from their postings (one-time, #251)");
    let tx = conn.transaction()?;
    tx.execute_batch(
        "ALTER TABLE pages ADD COLUMN length INTEGER NOT NULL DEFAULT 0;
         UPDATE pages SET length = (SELECT coalesce(sum(count), 0) FROM postings
                                    WHERE postings.page = pages.id);",
    )?;
    tx.commit()
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
    use std::time::{SystemTime, UNIX_EPOCH};

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
            store.postings("alpha").unwrap(),
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

        assert_eq!(store.delete(&["bafyB".to_string()]).unwrap(), 1);
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
        store.delete(&["bafyC".to_string()]).unwrap();
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
        store.delete(&["bafyJ".to_string()]).unwrap();
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
        tx.execute("DELETE FROM postings", []).unwrap();
        assert_eq!(
            store.postings("word").unwrap(),
            vec![("bafyR".to_string(), 2, 2)],
            "the last committed state, without waiting"
        );
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
            store.postings("alpha").unwrap(),
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
        assert_eq!(store.delete(&["bafyA/x".to_string()]).unwrap(), 1);
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
            store.delete(&["bafyA".to_string()]).unwrap(),
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
            store.postings("x").unwrap().iter().find(|p| p.0 == "a"),
            Some(&("a".to_string(), 2, 6))
        );
        // a re-crawl replaces the length rather than adding a page
        store.upsert("a", &page("a", "a", &[("x", 1)]), 2).unwrap();
        assert_eq!(store.average_length(), 1.5, "(1 + 2) / 2");
        store.delete(&["b".to_string()]).unwrap();
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
        let mut alpha = store.postings("alpha").unwrap();
        alpha.sort();
        assert_eq!(
            alpha,
            vec![("bafyA".to_string(), 2, 5), ("bafyB".to_string(), 1, 1)]
        );
        assert_eq!(store.average_length(), 3.0);
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
            store.postings("files").unwrap(),
            vec![("bafyA".to_string(), 3, 4)]
        );
        let mut alpha = store.postings("alpha").unwrap();
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
        // and re-opening doesn't migrate again
        drop(store);
        assert_eq!(Store::open(&dir, 0).unwrap().term_count(), 2);
    }
}
