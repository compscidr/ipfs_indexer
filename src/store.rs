use crate::index_queue::normalize;
use crate::index_result::IndexResult;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

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
        indexed_at INTEGER NOT NULL
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
        // a commit waits for the reader's queries to finish rather than failing
        conn.busy_timeout(BUSY_TIMEOUT)?;
        if cache_mb > 0 {
            // negative: in KiB rather than pages
            conn.pragma_update(None, "cache_size", -(cache_mb as i64) * 1024)?;
        }
        migrate_keywords_column(&mut conn)?;
        conn.execute_batch(SCHEMA)?;
        // after the bulk load in the migration: deleting a page's postings needs it
        conn.execute_batch("CREATE INDEX IF NOT EXISTS postings_page ON postings (page)")?;
        let terms: i64 = conn.query_row("SELECT count(*) FROM terms", [], |r| r.get(0))?;
        let queued: i64 = conn.query_row("SELECT count(*) FROM queue", [], |r| r.get(0))?;
        Ok(Store {
            conn: Mutex::new(conn),
            reader: None,
            path,
            terms: AtomicUsize::new(terms as usize),
            queued: AtomicUsize::new(queued as usize),
        })
    }

    fn read_conn(&self) -> MutexGuard<'_, Connection> {
        self.reader.as_ref().unwrap_or(&self.conn).lock().unwrap()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Adds or replaces a page and its postings (from result.keywords) in one transaction
    pub fn upsert(
        &self,
        item: &str,
        result: &IndexResult,
        indexed_at: i64,
    ) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // ON CONFLICT keeps the row's id, so a re-crawl replaces its postings in place
        let id: i64 = tx.query_row(
            "INSERT INTO pages (item, cid, title, excerpt, indexed_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (item) DO UPDATE SET cid = excluded.cid, title = excluded.title,
               excerpt = excluded.excerpt, indexed_at = excluded.indexed_at
             RETURNING id",
            params![item, result.cid, result.title, result.excerpt, indexed_at],
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
        Ok(())
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

    /// Deletes pages and their postings in one transaction; returns how many pages went
    pub fn delete(&self, items: &[String]) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (mut deleted, mut terms) = (0, 0);
        {
            let mut stmt = tx.prepare("DELETE FROM pages WHERE item = ?1 RETURNING id")?;
            for item in items {
                let id: Option<i64> = stmt.query_row([item], |r| r.get(0)).optional()?;
                if let Some(id) = id {
                    deleted += 1;
                    terms += self.remove_postings(&tx, id)?;
                }
            }
        }
        tx.commit()?;
        self.adjust_terms(terms);
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

    /// The pages a term is on, as (item, how often it appears there)
    pub fn postings(&self, term: &str) -> rusqlite::Result<Vec<(String, u32)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare_cached(
            "SELECT pages.item, postings.count FROM postings JOIN pages ON pages.id = postings.page
             WHERE postings.term = ?1",
        )?;
        let rows = stmt.query_map([term], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
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
        let mut conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        let item = conn
            .prepare_cached(
                "DELETE FROM queue WHERE rowid = (SELECT min(rowid) FROM queue) RETURNING item",
            )?
            .query_row([], |r| r.get(0))
            .optional()?;
        if item.is_some() {
            self.queued.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(item)
    }

    pub fn queue_len(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    /// Queued items per root CID (the first segment of "cid/path")
    pub fn queue_roots(&self) -> rusqlite::Result<Vec<(String, usize)>> {
        let conn = self.conn.lock().unwrap();
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
         DROP TABLE staged;
         DROP TABLE pages_old;",
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
            vec![("bafyA/x".to_string(), 3)]
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
            vec![("bafyR".to_string(), 2)],
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
            vec![("bafyA".to_string(), 3)]
        );
        let mut alpha = store.postings("alpha").unwrap();
        alpha.sort();
        assert_eq!(
            alpha,
            vec![("bafyA".to_string(), 1), ("bafyB".to_string(), 4)]
        );
        assert_eq!(store.term_count(), 2);
        assert_eq!(store.top_terms(1).unwrap(), vec![("alpha".to_string(), 2)]);
        assert_eq!(store.load_all().unwrap().len(), 2);
        assert_eq!(store.queue_load().unwrap(), vec!["bafyQ"]);
        // and re-opening doesn't migrate again
        drop(store);
        assert_eq!(Store::open(&dir, 0).unwrap().term_count(), 2);
    }
}
