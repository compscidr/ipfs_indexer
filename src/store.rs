use crate::index_result::IndexResult;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/**
 * The persisted index (#106): one SQLite file, `index.sqlite`, under DATA_DIR. secure_delete
 * zeroes deleted rows in the file. The rollback journal is kept in memory: an on-disk journal
 * (DELETE or WAL mode) would write plaintext copies of pages and later only unlink them. The
 * trade-off is that a crash mid-write can corrupt the file; the index is rebuildable, so
 * recovery is the documented wipe.
 */
pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(dir: &Path) -> rusqlite::Result<Store> {
        let path = dir.join("index.sqlite");
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "secure_delete", true)?;
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pages (
                item       TEXT PRIMARY KEY,
                cid        TEXT NOT NULL,
                title      TEXT NOT NULL,
                excerpt    TEXT NOT NULL,
                keywords   TEXT NOT NULL,
                indexed_at INTEGER NOT NULL
            )",
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn upsert(
        &self,
        item: &str,
        result: &IndexResult,
        indexed_at: i64,
    ) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO pages (item, cid, title, excerpt, keywords, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                item,
                result.cid,
                result.title,
                result.excerpt,
                encode_keywords(&result.keywords),
                indexed_at
            ],
        )?;
        Ok(())
    }

    /// Deletes in one transaction; returns how many rows went
    pub fn delete(&self, items: &[String]) -> rusqlite::Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut deleted = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM pages WHERE item = ?1")?;
            for item in items {
                deleted += stmt.execute([item])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    }

    pub fn load_all(&self) -> rusqlite::Result<Vec<(String, IndexResult, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT item, cid, title, excerpt, keywords, indexed_at FROM pages")?;
        let rows = stmt.query_map([], |r| {
            let keywords: String = r.get(4)?;
            Ok((
                r.get(0)?,
                IndexResult::new(r.get(1)?, r.get(2)?, r.get(3)?, decode_keywords(&keywords)),
                r.get(5)?,
            ))
        })?;
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

// "word count" per line; words come from split_whitespace so they never contain the separator
fn encode_keywords(keywords: &HashMap<String, u32>) -> String {
    keywords
        .iter()
        .map(|(w, c)| format!("{} {}\n", w, c))
        .collect()
}

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
        let store = Store::open(&dir).unwrap();
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

        let rows = Store::open(&dir).unwrap().load_all().unwrap();
        assert_eq!(rows.len(), 1, "upsert replaces");
        let (item, result, at) = &rows[0];
        assert_eq!(
            (item.as_str(), result.title.as_str(), *at),
            ("bafyA/x", "Title A2", 200)
        );
        assert_eq!(result.keywords.get("alpha"), Some(&3));
        assert_eq!(result.excerpt, "an excerpt");
    }

    #[test]
    fn delete_erases_the_text_from_the_file() {
        let dir = temp_dir("erase");
        let store = Store::open(&dir).unwrap();
        store
            .upsert("bafyB", &page("bafyB", "ZZQX-takedown-title-77", &[]), 1)
            .unwrap();
        assert!(contains(
            &std::fs::read(store.path()).unwrap(),
            "ZZQX-takedown-title-77"
        ));

        assert_eq!(store.delete(&["bafyB".to_string()]).unwrap(), 1);
        assert!(store.load_all().unwrap().is_empty());
        assert!(
            !contains(
                &std::fs::read(store.path()).unwrap(),
                "ZZQX-takedown-title-77"
            ),
            "secure_delete must zero the deleted row in the file"
        );
    }

    #[test]
    fn without_secure_delete_the_text_would_remain() {
        // control: proves the assertion above can fail
        let dir = temp_dir("control");
        let store = Store::open(&dir).unwrap();
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
            &std::fs::read(store.path()).unwrap(),
            "ZZQX-control-title-88"
        ));
    }

    #[test]
    fn rollback_journal_never_touches_disk() {
        // review #2: an on-disk journal holds plaintext copies of pages, then is merely unlinked
        let dir = temp_dir("journal");
        let store = Store::open(&dir).unwrap();
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
    fn finds_items_older_than_a_cutoff() {
        let store = Store::open(&temp_dir("age")).unwrap();
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
        assert!(Store::open(&file).is_err(), "DATA_DIR that is a file");

        std::fs::write(
            dir.join("index.sqlite"),
            b"this is not a sqlite database at all, 32+ bytes",
        )
        .unwrap();
        assert!(Store::open(&dir).is_err(), "corrupt index.sqlite");
    }

    #[test]
    fn decode_skips_malformed_keyword_lines() {
        let decoded = decode_keywords("good 3\nno-count\nbad x\n\nalso 1");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.get("good"), Some(&3));
        assert_eq!(decode_keywords(&encode_keywords(&decoded)), decoded);
    }
}
