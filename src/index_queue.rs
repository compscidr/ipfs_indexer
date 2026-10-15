use crate::denylist::{normalize_item, Denylist};
use crate::index_result::IndexResult;
use crate::rate::Rate;
use crate::search::{self, Query, Scorer, Total, MAX_RANKED};
use crate::store::{self, now_secs, OpenError, Store, Upserted};
use actix_web::body::MessageBody;
use cid::Cid;
use dashmap::Map;
use log::{error, info, trace, warn};
use scraper::{Html, Selector};
use sha2::Digest;
use std::collections::HashMap;
use std::io::Read;
use std::ops::Not;
use std::sync::{Condvar, Mutex, OnceLock, RwLock};
use std::time::Duration;
use std::time::Instant;

// where crawling starts: the IPFS wikipedia mirror. Queued at startup and again after the daily
// expiry, so the crawl walks back to pages that expired.
pub const SEED_CIDS: &[&str] = &["bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze"];

/**
 * Spaces out gateway requests across all workers: at most `per_min` a minute, 0 meaning no limit.
 * Public gateways rate-limit by client (Filebase: 200/min), and ten workers hitting cached pages
 * burst far past that.
 */
pub struct Throttle {
    interval: Duration,
    next: Mutex<Instant>,
}

impl Throttle {
    pub fn per_minute(per_min: u32) -> Throttle {
        let interval = if per_min == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(60) / per_min
        };
        Throttle {
            interval,
            next: Mutex::new(Instant::now()),
        }
    }

    /// Blocks until this caller's slot; each call reserves the next slot under the lock
    pub fn wait(&self) {
        if self.interval.is_zero() {
            return;
        }
        let slot = {
            let mut next = self.next.lock().unwrap();
            let slot = (*next).max(Instant::now());
            *next = slot + self.interval;
            slot
        };
        let now = Instant::now();
        if slot > now {
            std::thread::sleep(slot - now);
        }
    }
}

// too common to be worth a posting per page; replaces the old "longer than 3 bytes" cutoff (#209)
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "in", "is", "it", "its",
    "of", "on", "or", "that", "the", "this", "to", "was", "were", "with",
];

/**
 * Split text into search terms: lowercase, split on anything not alphanumeric (Unicode-aware),
 * stopwords dropped. Used for both indexing and queries so "Files." finds "files".
 */
pub fn normalize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
        .collect()
}

/**
 * A query's terms as AND-ed groups of OR-ed terms (#252): a page matches if it has at least one
 * term of every group. Words are AND-ed; an uppercase OR between two words puts them in one
 * group, binding tighter than AND, so "rust ipfs OR filecoin" is rust AND (ipfs OR filecoin).
 * Lowercase "or" is a stopword. A word that normalizes to several terms ("ada-lovelace") joins
 * by its first and last term.
 */
fn parse_query(query: &str) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut or_next = false;
    for word in query.split_whitespace() {
        if word == "OR" {
            or_next = !groups.is_empty();
            continue;
        }
        let terms = normalize(word);
        for (i, term) in terms.iter().enumerate() {
            match groups.last_mut() {
                Some(group) if i == 0 && or_next => {
                    if !group.contains(term) {
                        group.push(term.clone())
                    }
                }
                _ => groups.push(vec![term.clone()]),
            }
        }
        // a skipped word ("ada OR the OR nft") leaves the OR waiting for the next one
        or_next &= terms.is_empty();
    }
    groups
}

// a purge scan batch that fails to read is tried this many times, this far apart (#231)
const PURGE_READ_TRIES: u32 = 3;
const PURGE_READ_PAUSE: Duration = Duration::from_millis(100);
// and a purge round that didn't finish runs again after this
const PURGE_RETRY_AFTER: Duration = Duration::from_secs(60);

// a purge scan's read, tried PURGE_READ_TRIES times; None if every try failed
fn read_retrying<T>(what: &str, read: impl Fn() -> rusqlite::Result<T>) -> Option<T> {
    for attempt in 1..=PURGE_READ_TRIES {
        match read() {
            Ok(rows) => return Some(rows),
            Err(err) => {
                warn!(
                    "Couldn't scan {} for denylisted ones (try {} of {}): {}",
                    what, attempt, PURGE_READ_TRIES, err
                );
                if attempt < PURGE_READ_TRIES {
                    std::thread::sleep(PURGE_READ_PAUSE);
                }
            }
        }
    }
    None
}

// skip anything bigger than this rather than pulling it all into memory
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

/**
 * Fetch a url and return its body, but only if the gateway says it's html and it fits in
 * MAX_BODY_BYTES. Ok(None) means it was skipped: images, video, etc are never downloaded.
 */
fn fetch_html(url: &str) -> Result<Option<(String, reqwest::Url)>, reqwest::Error> {
    let client = reqwest::blocking::Client::builder()
        // public gateways can take ~50s on a cache miss
        .timeout(Duration::from_secs(90))
        .build()?;
    let response = client.get(url).send()?.error_for_status()?;
    // where the page actually is after redirects (e.g. "wiki" -> "wiki/"); links resolve against it
    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("text/html") {
        warn!("Skipping {}: content type {:?}", url, content_type);
        return Ok(None);
    }
    let mut body = Vec::new();
    if let Err(err) = response.take(MAX_BODY_BYTES + 1).read_to_end(&mut body) {
        warn!("Skipping {}: error reading body: {}", url, err);
        return Ok(None);
    }
    if body.len() as u64 > MAX_BODY_BYTES {
        warn!("Skipping {}: larger than {} bytes", url, MAX_BODY_BYTES);
        return Ok(None);
    }
    Ok(Some((
        String::from_utf8_lossy(&body).into_owned(),
        final_url,
    )))
}

/**
 * The "cid/path" an <a href> on a page at `base` points to, or None if it's not an IPFS page on
 * the same gateway. Resolves like a browser (so "geo:", "mailto:", other hosts and fragments
 * drop out). The Wikipedia mirror renamed its A/ folder to wiki/ but its index still links
 * "../A/X", a 404 as written, so <cid>/A/... is mapped to <cid>/wiki/....
 */
fn resolve_link(base: &reqwest::Url, href: &str) -> Option<String> {
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let target = base.join(href).ok()?;
    if !matches!(target.scheme(), "http" | "https") || target.host_str() != base.host_str() {
        return None;
    }
    let item = normalize_item(target.path().strip_prefix("/ipfs/")?)?;
    // ponytail: heuristic. The mirror writes non-web links as "./geo:..." / "./ftp:/...", which
    // resolve to 404 paths; a segment starting with a lowercase scheme and ":" is one of those.
    // Titles containing a colon ("Star_Trek:_...") don't start with a lowercase scheme.
    if item.split('/').skip(1).any(looks_like_scheme) {
        return None;
    }
    let mut parts = item.splitn(3, '/');
    let root = parts.next()?;
    match (parts.next(), parts.next()) {
        (Some("A"), Some(rest)) => Some(format!("{}/wiki/{}", root, rest)),
        _ => Some(item),
    }
}

// "geo:...", "ftp:...", "mailto:...": a lowercase URI scheme (RFC 3986 characters) then ":"
fn looks_like_scheme(segment: &str) -> bool {
    match segment.split_once(':') {
        Some((scheme, _)) => {
            scheme.starts_with(|c: char| c.is_ascii_lowercase())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+.-".contains(c))
        }
        None => false,
    }
}

// whether a failed fetch says something about the gateway itself, rather than the page:
// network errors, timeouts, 5xx and 429 do; a 404 or other 4xx just means that page is missing
fn is_gateway_error(err: &reqwest::Error) -> bool {
    match err.status() {
        Some(status) => status.is_server_error() || status.as_u16() == 429,
        None => true,
    }
}

#[derive(Debug, PartialEq)]
pub enum Enqueued {
    Queued,
    InvalidCid,
    Blocked,
}

pub struct IndexQueue {
    // badbits and the hand-maintained local list, checked before every enqueue and fetch
    denylist: RwLock<Denylist>,
    local_denylist: RwLock<Denylist>,

    // when the gateway last answered a fetch, and the error if it didn't
    last_fetch: Mutex<Option<(Instant, Result<(), String>)>>,

    // the persisted index (#106), the inverted index and the crawl queue (#231). Set by
    // attach_store, or an in-memory one on first use when DATA_DIR isn't configured.
    store: OnceLock<Store>,

    // persist() holds this shared across its check + upsert; denylist swaps and each expiry
    // batch hold it exclusively, so a page checked against an old list can't be written after
    // the swap (the purge that follows could miss it)
    writes: RwLock<()>,

    // set when a list gains entries (or a store is attached); the purge thread waits on the
    // condvar, takes the flag and scans (#231)
    purge_due: Mutex<bool>,
    purge_wake: Condvar,

    // spaces out gateway requests; unset means no limit
    throttle: OnceLock<Throttle>,

    // pages written per minute, for /status (#264)
    indexed: Rate,
}

impl IndexQueue {
    pub fn new() -> Self {
        IndexQueue {
            denylist: RwLock::new(Denylist::empty()),
            local_denylist: RwLock::new(Denylist::empty()),
            last_fetch: Mutex::new(None),
            store: OnceLock::new(),
            writes: RwLock::new(()),
            purge_due: Mutex::new(false),
            purge_wake: Condvar::new(),
            throttle: OnceLock::new(),
            indexed: Rate::new(),
        }
    }

    // the store for indexing and search; in memory if none was attached before first use
    fn store(&self) -> &Store {
        self.store
            .get_or_init(|| Store::open_in_memory().expect("in-memory sqlite"))
    }

    // indexes a page the way persist does, minus the denylist check (test items aren't real CIDs)
    #[cfg(test)]
    pub(crate) fn index_for_test(&self, item: &str, result: &IndexResult) {
        self.store().upsert(item, result, now_secs()).unwrap();
    }

    #[cfg(test)]
    pub(crate) fn flush_for_test(&self) {
        self.store().flush().unwrap();
    }

    #[cfg(test)]
    pub(crate) fn maintain_for_test(&self) {
        self.store().maintain();
    }

    /// Waits for, then runs, one round of index upkeep (#247): flushes, erasures (#249) and
    /// retired files. For a dedicated thread.
    pub fn maintain_store(&self) {
        let store = self.store();
        store.wait_for_maintenance();
        store.upkeep();
    }

    /// Waits for, then runs, the segment merges due (#247). For a thread of its own: a merge can
    /// take minutes, and flushes and erasures don't wait for it.
    pub fn merge_store(&self) {
        let store = self.store();
        store.wait_for_merge();
        store.merge();
    }

    /// Writes buffered postings to disk, for a clean shutdown
    pub fn flush_store(&self) {
        if let Some(store) = self.store.get() {
            if let Err(err) = store.flush() {
                warn!("Couldn't flush the index: {}", err);
            }
        }
    }

    /// Limit gateway requests to `per_min` a minute across all workers (0 = no limit)
    pub fn set_gateway_rate(&self, per_min: u32) {
        let _ = self.throttle.set(Throttle::per_minute(per_min));
    }

    fn wait_for_gateway(&self) {
        if let Some(throttle) = self.throttle.get() {
            throttle.wait();
        }
    }

    /// e.g. "ok 12s ago" or "error 3s ago: <reason>", for /status
    pub fn gateway_health(&self) -> String {
        match &*self.last_fetch.lock().unwrap() {
            None => "no fetches yet".to_string(),
            Some((at, Ok(()))) => format!("ok {}s ago", at.elapsed().as_secs()),
            Some((at, Err(err))) => format!("error {}s ago: {}", at.elapsed().as_secs(), err),
        }
    }

    /// Swaps in a new badbits list. Pages it blocks are hidden from search at once and erased
    /// by the purge thread, which only runs if the list gained entries (#231).
    pub fn set_denylist(&self, denylist: Denylist) {
        let added = {
            let _writes = self.writes.write().unwrap();
            let mut current = self.denylist.write().unwrap();
            let added = denylist.adds_to(&current);
            *current = denylist;
            added
        };
        if added && self.store.get().is_some() {
            self.request_purge();
        }
    }

    /// Same as set_denylist, for the local takedown list
    pub fn set_local_denylist(&self, denylist: Denylist) {
        let added = {
            let _writes = self.writes.write().unwrap();
            let mut current = self.local_denylist.write().unwrap();
            let added = denylist.adds_to(&current);
            *current = denylist;
            added
        };
        if added && self.store.get().is_some() {
            self.request_purge();
        }
    }

    /**
     * Opens DATA_DIR's index and attaches it (see attach_store). A damaged index is erased and
     * started again empty, once, rather than refusing to start until someone wipes it by hand:
     * it's rebuilt by crawling either way.
     */
    pub fn open_store(
        &self,
        dir: &std::path::Path,
        cache_mb: u32,
        max_age_secs: i64,
    ) -> Result<(usize, usize), OpenError> {
        let open = || -> Result<_, OpenError> {
            let store = Store::open(dir, cache_mb)?;
            Ok(self.attach_store(store, max_age_secs)?)
        };
        match open() {
            Err(err) if store::is_corrupt(&*err) => {
                error!(
                    "The index in {} is damaged ({}); erasing it and starting again empty",
                    dir.display(),
                    err
                );
                store::wipe(dir)?;
                open()
            }
            other => other,
        }
    }

    /**
     * Use `store` for this queue (call before crawling or serving). Nothing is loaded into memory
     * (#231): pages, mirrors and the queue stay on disk. Pages a denylist blocked while the
     * index was down are hidden by search's check and erased by the purge queued here; expiry
     * runs from main. Returns the pages and queued items there are.
     */
    pub fn attach_store(
        &self,
        store: Store,
        _max_age_secs: i64,
    ) -> rusqlite::Result<(usize, usize)> {
        // the startup upkeep (erasures a crash cut short, #249) ran in Store::open
        let counts = (store.page_count(), store.queue_len());
        if self.store.set(store).is_err() {
            panic!("attach_store must run before the store is first used");
        }
        self.request_purge();
        Ok(counts)
    }

    // marks a purge as due and wakes the purge thread; one round covers every change before it
    fn request_purge(&self) {
        *self.purge_due.lock().unwrap() = true;
        self.purge_wake.notify_one();
    }

    // takes the due flag: a change after this sets it again, so it gets a round of its own
    fn take_purge_due(&self) -> bool {
        std::mem::replace(&mut *self.purge_due.lock().unwrap(), false)
    }

    /// Waits until a purge is due, then runs one (#231). For a thread of its own. A round the
    /// store failed part-way is run again after PURGE_RETRY_AFTER.
    pub fn run_purges(&self) {
        let due = self.purge_due.lock().unwrap();
        drop(self.purge_wake.wait_while(due, |due| !*due).unwrap());
        // only this thread clears the flag, so it's still set
        if self.take_purge_due() && !self.purge_blocked() {
            // not at once: a store that keeps failing would have this spin
            std::thread::sleep(PURGE_RETRY_AFTER);
            self.request_purge();
        }
    }

    #[cfg(test)]
    pub(crate) fn purge_for_test(&self) {
        if self.take_purge_due() && !self.purge_blocked() {
            self.request_purge();
        }
    }

    /// the index file, if a store is attached
    pub fn store_path(&self) -> Option<std::path::PathBuf> {
        self.store.get()?.path().map(|path| path.to_path_buf())
    }

    /// Queue each seed that isn't indexed or queued already; returns how many were queued
    pub fn reseed(&self) -> usize {
        let seeds = SEED_CIDS
            .iter()
            .filter_map(|seed| self.admit(seed.to_string()).1)
            .collect();
        self.push_queued(seeds)
    }

    /**
     * Queue every indexed page to be fetched again, so links to pages not indexed yet are followed
     * again (the crawl frontier was lost before it was saved, #225). Pages stay searchable while
     * they wait, and what's left of it survives a restart. Streams `pages` in batches (#231).
     */
    pub fn recrawl(&self) -> usize {
        let store = self.store();
        let (mut queued, mut after) = (0, 0);
        loop {
            let batch = match store.items_after(after, crate::store::WRITE_BATCH) {
                Ok(batch) => batch,
                Err(err) => {
                    warn!("Couldn't list pages to recrawl: {}", err);
                    break;
                }
            };
            let Some(last) = batch.last() else { break };
            after = last.0;
            let items = batch
                .into_iter()
                .map(|(_, item, _)| item)
                .filter(|item| !self.blocked_quietly(item))
                .collect();
            queued += self.push_queued(items);
        }
        queued
    }

    /// Drop pages indexed more than max_age_secs ago, a batch at a time under the writes lock,
    /// so a page re-crawled meanwhile isn't listed with its old date
    pub fn expire(&self, max_age_secs: i64) -> usize {
        let Some(store) = self.store.get() else {
            return 0;
        };
        let cutoff = now_secs() - max_age_secs;
        let mut expired = 0;
        loop {
            let _writes = self.writes.write().unwrap();
            let old = match store.older_than(cutoff, crate::store::WRITE_BATCH) {
                Ok(old) => old,
                Err(err) => {
                    warn!("Couldn't check for expired pages: {}", err);
                    break;
                }
            };
            if old.is_empty() {
                break;
            }
            match store.delete(&old, true) {
                Ok(_) => expired += old.len(),
                Err(err) => {
                    // what's left is still old enough for the next expiry to retry
                    warn!(
                        "Couldn't delete every expired page; the next expiry retries: {}",
                        err
                    );
                    break;
                }
            }
        }
        // old versions of re-crawled pages don't outlive the retention window either
        match store.dead_versions() {
            Ok(versions) => {
                let old_versions: Vec<u64> = versions
                    .into_iter()
                    .filter(|(.., died_at)| *died_at < cutoff)
                    .map(|(id, ..)| id)
                    .collect();
                if let Err(err) = store.erase_ids(&old_versions) {
                    warn!("Couldn't mark expired versions to erase: {}", err);
                }
            }
            Err(err) => warn!("Couldn't check old page versions: {}", err),
        }
        expired
    }

    /// Pages written per minute, averaged over the last ten full minutes (#264)
    pub fn indexed_per_minute(&self) -> u64 {
        self.indexed.per_minute()
    }

    /**
     * Add a crawled page to the index and the store. Re-checks the denylists first: a takedown
     * that landed while this page was being fetched must not be written back to disk. A page with
     * the same text as one indexed already is only recorded as its mirror (#253). Returns whether
     * the page was indexed.
     */
    pub fn persist(&self, item: &str, result: &IndexResult) -> bool {
        let _writes = self.writes.read().unwrap();
        if self.blocked_quietly(item) || self.blocked_quietly(&result.cid) {
            return false;
        }
        match self.store().upsert(item, result, now_secs()) {
            Ok(Upserted::Mirror(_)) => false,
            Ok(Upserted::Indexed) => {
                self.indexed.add();
                true
            }
            Err(err) => {
                warn!("Couldn't persist {}: {}", item, err);
                // its links are still followed
                true
            }
        }
    }

    /**
     * Deletes and erases (#249) every stored page, mirror and superseded version a denylist
     * blocks now, scanning `pages` in batches on the store's scan connection (#231). No lock is
     * held across batches: pages written meanwhile were checked against the lists already. A
     * read (a batch, or the old versions) that fails is tried again a few times; if it still
     * fails, or a delete or erase mark fails, the rest of the round goes on and false is
     * returned, for another round later.
     */
    pub fn purge_blocked(&self) -> bool {
        let Some(store) = self.store.get() else {
            return true;
        };
        let blocked = |item: &str| self.blocked_quietly(item);
        let (mut removed, mut complete) = (0, true);
        let mut after = 0;
        loop {
            let Some(batch) = read_retrying("pages", || store.items_after(after, 10_000)) else {
                complete = false;
                break;
            };
            let Some(last) = batch.last() else { break };
            after = last.0;
            let hits: Vec<String> = batch
                .into_iter()
                .filter(|(_, item, cid)| blocked(item) || blocked(cid))
                .map(|(_, item, _)| item)
                .collect();
            match store.delete(&hits, true) {
                Ok(n) => removed += n,
                Err(err) => {
                    warn!("Couldn't delete {} denylisted pages: {}", hits.len(), err);
                    complete = false;
                }
            }
        }
        let mut after = 0;
        loop {
            let Some(batch) = read_retrying("mirrors", || store.mirror_items_after(after, 10_000))
            else {
                complete = false;
                break;
            };
            let Some(last) = batch.last() else { break };
            after = last.0;
            let hits: Vec<String> = batch
                .into_iter()
                .map(|(_, item)| item)
                .filter(|item| blocked(item))
                .collect();
            if let Err(err) = store.delete(&hits, true) {
                warn!("Couldn't delete {} denylisted mirrors: {}", hits.len(), err);
                complete = false;
            }
        }
        // superseded versions whose own item or cid is now blocked, e.g. one that redirected
        // to a cid since taken down (#249)
        match read_retrying("old page versions", || store.dead_versions()) {
            Some(versions) => {
                let ids: Vec<u64> = versions
                    .into_iter()
                    .filter(|(_, item, cid, _)| blocked(item) || blocked(cid))
                    .map(|(id, ..)| id)
                    .collect();
                if let Err(err) = store.erase_ids(&ids) {
                    warn!(
                        "Couldn't mark {} blocked versions to erase: {}",
                        ids.len(),
                        err
                    );
                    complete = false;
                }
            }
            None => complete = false,
        }
        if removed > 0 {
            info!("Purged {} denylisted pages", removed);
        }
        if !complete {
            warn!("The denylist purge didn't finish; another round runs later");
        }
        complete
    }

    // is_blocked without the per-item warn!, for bulk checks
    fn blocked_quietly(&self, item: &str) -> bool {
        self.denylist.read().unwrap().is_blocked(item)
            || self.local_denylist.read().unwrap().is_blocked(item)
    }

    fn is_blocked(&self, item: &str) -> bool {
        let blocked = self.denylist.read().unwrap().is_blocked(item)
            || self.local_denylist.read().unwrap().is_blocked(item);
        if blocked {
            warn!("Denylisted, skipping {}", item);
        }
        blocked
    }

    /**
     * Queue a "cid" or "cid/path" for indexing. Nothing is queued unless it returns Queued.
     */
    pub fn enqueue(&self, item: String) -> Enqueued {
        let (result, new) = self.admit(item);
        self.push_queued(new.into_iter().collect());
        result
    }

    /// enqueue for many items (a page's links), saved to disk in one transaction
    pub fn enqueue_many(&self, items: Vec<String>) {
        let new = items
            .into_iter()
            .filter_map(|item| self.admit(item).1)
            .collect();
        self.push_queued(new);
    }

    // adds admitted items to the queue (ones already queued are skipped); returns how many were new
    fn push_queued(&self, items: Vec<String>) -> usize {
        if items.is_empty() {
            return 0;
        }
        match self.store().queue_add(&items) {
            Ok(added) => {
                info!("Enqueued {} of {} items", added, items.len());
                added
            }
            Err(err) => {
                warn!("Couldn't queue {} items: {}", items.len(), err);
                0
            }
        }
    }

    /// Validates an item; it's returned (spelled canonically) if it can be queued
    fn admit(&self, item: String) -> (Enqueued, Option<String>) {
        // one canonical spelling per page (#218); a path that climbs out of its CID is invalid
        let Some(item) = normalize_item(&item) else {
            warn!("Not a valid cid/path, ignoring {}", item);
            return (Enqueued::InvalidCid, None);
        };
        let root = item.split('/').next().unwrap_or("");
        if Cid::try_from(root).is_err() {
            warn!("Not a CID, ignoring {}", item);
            return (Enqueued::InvalidCid, None);
        }
        if self.is_blocked(&item) {
            return (Enqueued::Blocked, None);
        }
        match self.store().known(&item) {
            Ok(true) => {
                trace!("Already indexed {}", item);
                return (Enqueued::Queued, None);
            }
            Ok(false) => {}
            // admitted: a wasted fetch at worst, where dropping it would lose the link
            Err(err) => warn!("Couldn't check whether {} is indexed: {}", item, err),
        }
        (Enqueued::Queued, Some(item))
    }

    pub fn queue_length(&self) -> usize {
        self.store().queue_len()
    }

    pub fn index_length(&self) -> usize {
        self.store().page_count()
    }

    /// Indexed and queued counts per root CID (the first segment of "cid/path"), with the root
    /// page's title if it's indexed, most indexed first (#232). Roots a denylist blocks are left
    /// out, and a blocked root page's title blanked, even before the purge erases them (#231).
    pub fn root_counts(&self) -> Vec<(String, usize, usize, String)> {
        let rows = self.store().root_counts().unwrap_or_else(|err| {
            warn!("Couldn't count pages by root: {}", err);
            Vec::new()
        });
        rows.into_iter()
            .filter(|(root, ..)| !self.blocked_quietly(root))
            .map(|(root, pages, queued, title, cid)| {
                // the root page may have redirected to a cid since taken down
                let title = if cid.is_empty() || !self.blocked_quietly(&cid) {
                    title
                } else {
                    String::new()
                };
                (root, pages, queued, title)
            })
            .collect()
    }

    /// how many crawled items weren't indexed, being the same text as an indexed page (#253)
    pub fn mirror_length(&self) -> usize {
        self.store().mirror_count()
    }

    pub fn keyword_length(&self) -> usize {
        self.store().term_count()
    }

    /*
     * Returns the top n keywords by the number of CIDs they map to
     */
    pub fn top_keywords(&self, n: usize) -> Vec<(String, u32)> {
        self.store().top_terms(n).unwrap_or_else(|err| {
            warn!("Couldn't read the top keywords: {}", err);
            Vec::new()
        })
    }

    /**
     * The best pages for a query, ranked by BM25 (#248): at most `offset + limit` of them, clamped
     * to MAX_RANKED, are ranked; `offset..offset + limit` are returned. The total is exact for one
     * word or a small smallest group, else an estimate.
     */
    pub fn search(&self, query: &str, offset: usize, limit: usize) -> (Total, Vec<IndexResult>) {
        let none = (Total { n: 0, about: false }, Vec::new());
        let query = Query::new(&parse_query(query));
        let terms: Vec<&str> = query.terms.iter().map(String::as_str).collect();
        let view = self.store().view(&terms);
        let dfs = match search::dfs(&view, &query) {
            Ok(dfs) => dfs,
            Err(err) => {
                warn!("Couldn't search for {:?}: {}", query.terms, err);
                return none;
            }
        };
        // a group none of whose terms are indexed matches nothing
        if query.groups.iter().any(|g| g.iter().all(|&t| dfs[t] == 0)) {
            return none;
        }
        let scorer = Scorer::new(
            self.store().page_count() as u64,
            self.store().average_length(),
            &dfs,
        );
        let k = offset.saturating_add(limit).min(MAX_RANKED);
        let (total, ranked) = match search::search(&view, &query, &scorer, k) {
            Ok(found) => found,
            Err(err) => {
                warn!("Couldn't search for {:?}: {}", query.terms, err);
                return none;
            }
        };
        drop(view);
        // resolve only the requested page of results to items, not every match
        let shown: Vec<u64> = ranked.into_iter().skip(offset).take(limit).collect();
        let found = match self.store().pages_by_id(&shown) {
            Ok(found) => found,
            Err(err) => {
                warn!("Couldn't look up search results: {}", err);
                return none;
            }
        };
        // a page deleted since its postings were read isn't found; one a denylist blocks isn't
        // served even before the purge erases it (#231)
        let results = shown
            .iter()
            .filter_map(|page| found.get(page))
            .filter(|(item, result)| {
                !self.blocked_quietly(item) && !self.blocked_quietly(&result.cid)
            })
            .map(|(_, result)| result.clone());
        (total, results.collect())
    }

    pub fn start(&self, gateway: String) {
        loop {
            if !self.process_next(&gateway) {
                // idle workers spun at 100% CPU each (#224)
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    /// Fetch and index the next queued item; false if the queue was empty
    pub fn process_next(&self, gateway: &str) -> bool {
        let start = Instant::now();
        // taken off disk now: a crash mid-fetch loses at most one item per worker
        let item = match self.store().queue_pop() {
            Ok(Some(item)) => item,
            Ok(None) => return false,
            Err(err) => {
                warn!("Couldn't take the next queued item: {}", err);
                return false;
            }
        };
        if !self.is_blocked(&item) {
            warn!("Indexing {}", item);
            match self.retrieve_content(gateway.to_string(), item.clone()) {
                // a mirror's links lead to more of the mirror: the original's were followed
                Some((result, links)) => {
                    if self.persist(&item, &result) {
                        self.enqueue_many(links);
                    }
                }
                // for now give up on error
                None => warn!("Error retrieving CID {}", item),
            }
            info!("Done {} in {:?}", item, start.elapsed());
        }
        true
    }

    /**
     * Use the http client to obtain the page from the ipfs gateway. If there is a failure to
     * obtain the CID, we give up for now.
     */
    fn retrieve_content(&self, gateway: String, cid: String) -> Option<(IndexResult, Vec<String>)> {
        let url = format!("http://{}/ipfs/{}", gateway, cid);
        warn!("Retreiving {}", url);
        self.wait_for_gateway();
        let fetch_start = Instant::now();
        let fetched = fetch_html(&url);
        info!("Fetched {} in {:?}", cid, fetch_start.elapsed());
        let health = match &fetched {
            Err(err) if is_gateway_error(err) => Err(err.to_string()),
            _ => Ok(()),
        };
        *self.last_fetch.lock().unwrap() = Some((Instant::now(), health));
        let (html, mut base) = match fetched {
            Ok(Some(page)) => page,
            Ok(None) => return None,
            Err(err) => {
                warn!("Error: {}, not re-enqueue-ing cid", err);
                // self.enqueue(cid.clone()); // for now give up on error
                return None;
            }
        };
        let mut document = Html::parse_document(html.as_str());
        let result = self.detect_redirect(url, cid.clone(), document.clone());
        let mut fullcid = cid.clone();
        if let Some((redirected_cid, redirected_doc, redirected_url)) = result {
            (fullcid, document, base) = (redirected_cid, redirected_doc, redirected_url);
        }
        trace!("received: {:?}", html.as_str());

        self.process_content(&base, fullcid.clone(), document.clone())
    }

    /**
     * Determine if the response we've received requires another request from a redirect.
     * If it requires another request, we will have an updated "full cid" as well.
     */
    fn detect_redirect(
        &self,
        url: String,
        cid: String,
        document: Html,
    ) -> Option<(String, Html, reqwest::Url)> {
        // ipfs.io does not use normal redirects (301, 307, etc) in the status code, so reqwest client
        // can't detect it. We will have to parse the meta http-equiv tag to get the redirect url.
        let selector = Selector::parse("noscript").unwrap();
        let noscript = document.select(&selector).next();

        if noscript.is_some() {
            info!("found noscript");
            let inner_html = noscript.unwrap().inner_html();
            if inner_html.find("meta http-equiv=\"refresh\"").is_none() {
                warn!("no meta http-equiv=\"refresh\" found");
            } else {
                info!("found meta http-equiv=\"refresh\"");
                let start_bytes = inner_html.find("url=").unwrap_or(0);
                let end_bytes = inner_html[start_bytes..]
                    .find("\"")
                    .unwrap_or(inner_html.len())
                    + start_bytes;
                let redirect_url = &inner_html[start_bytes + 4..end_bytes];
                warn!("Redirecting to {}", redirect_url);
                // assuming relative
                let newurl = format!("{}/{}", url, redirect_url);

                let fullcid = cid.clone() + "/" + redirect_url;
                if self.is_blocked(&fullcid) {
                    return None;
                }
                self.wait_for_gateway();
                return match fetch_html(&newurl) {
                    Ok(Some((html, final_url))) => {
                        Some((fullcid, Html::parse_document(html.as_str()), final_url))
                    }
                    Ok(None) => None,
                    Err(e) => {
                        warn!("error retrieving content from {}: {}", url, e);
                        if e.is_timeout() {
                            self.enqueue(fullcid.clone());
                        }
                        None
                    }
                };
            }
        }
        None
    }

    /**
     * Process the content of the page, extract keywords, return the IndexResult and the page's
     * links
     */
    fn process_content(
        &self,
        base: &reqwest::Url,
        cid: String,
        document: Html,
    ) -> Option<(IndexResult, Vec<String>)> {
        let fullcid = cid.clone();

        let selector = Selector::parse("title").unwrap();
        let titletag = document.select(&selector).next();
        let mut title: String = "".to_string();
        if titletag.is_some() {
            title = titletag.unwrap().text().collect();
        }

        // todo: get all relative links and add them to the index
        let selector = Selector::parse("a").unwrap();
        let links = document
            .select(&selector)
            .filter_map(|element| element.value().attr("href"))
            .filter_map(|href| resolve_link(base, href))
            .collect();
        let selector = Selector::parse("body").unwrap();
        let body = document.select(&selector).next();
        let mut index_keywords: HashMap<String, u32> = HashMap::new();
        if body.is_some() {
            // collect up the tags in the body, and get the contents within them without their tags
            let inner = body.unwrap().text().collect::<Vec<_>>();
            let mut content = inner.join(" ");
            // this leaves a ton of whitespace between things, so do this next step to remove that
            let iter = content.split_whitespace();
            content = iter.fold(String::new(), |a, b| a + b + " ");
            content = content.trim_start().trim_end().to_string();

            // get the frequency of words and turn it into a btree
            // https://stackoverflow.com/questions/41220872/how-if-possible-to-sort-a-btreemap-by-value-in-rust
            for word in normalize(&content) {
                *index_keywords.entry(word).or_insert(0) += 1;
            }

            if content.contains("no link named") {
                warn!("ipfs error on page {}, likely doesn't exist", fullcid);
            }

            let excerpt_len = if content.len() < 128 {
                content.len()
            } else {
                128
            };
            let end = content.char_indices().map(|(i, _)| i).nth(excerpt_len);
            let excerpt = match end {
                Some(end) => content[..end].to_string(),
                None => "".to_string(),
            };
            let mut result = IndexResult::new(fullcid, title, excerpt, index_keywords);
            // pages without text all hash the same, and may still have links worth following
            if !content.is_empty() {
                result = result.with_text_hash(sha2::Sha256::digest(content.as_bytes()).to_vec());
            }
            return Some((result, links));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::io::Write;
    use std::net::TcpListener;

    // serves a single http response with the given content type and body, returns its url
    fn serve_once(content_type: &'static str, body: Vec<u8>) -> String {
        serve_status_once("200 OK", content_type, body)
    }

    fn serve_status_once(
        status: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status,
                content_type,
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        });
        url
    }

    #[test]
    fn root_counts_group_items_by_their_root_cid() {
        // real CIDs: a root that isn't one counts as blocked
        let (wiki, other, lone) = (GOOD, BAD, QM);
        let queue = IndexQueue::new();
        for item in [
            wiki.to_string(),
            format!("{}/wiki/A", wiki),
            format!("{}/wiki/B", wiki),
            format!("{}/x", other),
        ] {
            queue.index_for_test(&item, &page(&item, "t", &[]));
        }
        queue.push_queued(vec![format!("{}/wiki/C", wiki), format!("{}/y", lone)]);
        assert_eq!(
            queue.root_counts(),
            vec![
                (wiki.to_string(), 3, 1, "t".to_string()),
                (other.to_string(), 1, 0, String::new()),
                (lone.to_string(), 0, 1, String::new()),
            ]
        );
    }

    #[test]
    fn root_counts_hide_a_takedown_before_the_purge() {
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open_in_memory().unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test(); // the one attach_store queued
        queue.persist(BAD, &page(BAD, "secret title", &[]));
        queue.persist(GOOD, &page(QM, "redirected title", &[]));
        queue.set_local_denylist(Denylist::parse(&format!("{}\n{}\n", BAD, QM)));
        assert_eq!(
            queue.root_counts(),
            vec![(GOOD.to_string(), 1, 0, String::new())],
            "a blocked root is gone and a blocked root page's title is blank, before the purge"
        );
        queue.purge_for_test();
        assert_eq!(queue.root_counts(), Vec::new());
    }

    #[test]
    fn set_denylist_purges_indexed_and_blocks_fetches() {
        let bad = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let good = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        let queue = IndexQueue::new();
        for cid in [bad, good] {
            let item = format!("{}/page", cid);
            let result = IndexResult::new(item.clone(), "".into(), "".into(), HashMap::new());
            queue.index_for_test(&item, &result);
        }

        let hash: String = sha2::Sha256::digest(format!("{}/", bad).as_bytes())
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        queue.set_denylist(Denylist::parse(&format!("//{}\n", hash)));
        queue.purge_for_test();

        assert_eq!(queue.index_length(), 1);
        assert!(queue.store().known(&format!("{}/page", good)).unwrap());
        assert!(queue.is_blocked(bad));
        assert!(!queue.is_blocked(good));
    }

    #[test]
    fn local_denylist_purges_and_blocks_alongside_badbits() {
        let bad = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let good = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        let queue = IndexQueue::new();
        for cid in [bad, good] {
            let item = format!("{}/page", cid);
            let result = IndexResult::new(item.clone(), "".into(), "".into(), HashMap::new());
            queue.index_for_test(&item, &result);
        }

        queue.set_denylist(Denylist::parse(&format!("{}/badbits-page\n", good)));
        queue.set_local_denylist(Denylist::parse(&format!("{}/page\n", bad)));
        queue.purge_for_test();

        assert_eq!(queue.index_length(), 1);
        assert_eq!(queue.enqueue(format!("{}/page", bad)), Enqueued::Blocked);
        // setting the local list left badbits in place
        assert_eq!(
            queue.enqueue(format!("{}/badbits-page", good)),
            Enqueued::Blocked
        );
        assert_eq!(queue.enqueue(good.to_string()), Enqueued::Queued);

        // and reloading badbits leaves the local list in place
        queue.set_denylist(Denylist::empty());
        assert_eq!(queue.enqueue(format!("{}/page", bad)), Enqueued::Blocked);
    }

    #[test]
    fn gateway_health_reflects_last_fetch() {
        let queue = IndexQueue::new();
        assert_eq!(queue.gateway_health(), "no fetches yet");

        let gateway = serve_once("text/html", b"<title>t</title>".to_vec())
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        queue.retrieve_content(gateway, "bafy".to_string());
        assert_eq!(queue.gateway_health(), "ok 0s ago");

        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        queue.retrieve_content(format!("127.0.0.1:{}", port), "bafy".to_string());
        assert!(queue.gateway_health().starts_with("error 0s ago: "));
    }

    #[test]
    fn throttle_spaces_requests_across_threads() {
        // 600/min = one request per 100ms; four requests from four threads span >= 300ms
        let throttle = std::sync::Arc::new(Throttle::per_minute(600));
        let start = Instant::now();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let throttle = std::sync::Arc::clone(&throttle);
                std::thread::spawn(move || throttle.wait())
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(290), "{:?}", elapsed);
        assert!(elapsed < Duration::from_millis(900), "{:?}", elapsed);
    }

    #[test]
    fn unlimited_throttle_never_waits() {
        let throttle = Throttle::per_minute(0);
        let start = Instant::now();
        (0..1000).for_each(|_| throttle.wait());
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn page_fetches_go_through_the_throttle() {
        let queue = IndexQueue::new();
        queue.set_gateway_rate(300); // one request per 200ms
        let start = Instant::now();
        for _ in 0..3 {
            let gateway = serve_once("text/html", b"<title>t</title>".to_vec())
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string();
            queue.retrieve_content(gateway, "bafy".to_string());
        }
        assert!(
            start.elapsed() >= Duration::from_millis(390),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn fetch_html_returns_html() {
        let url = serve_once("text/html; charset=utf-8", b"<title>hi</title>".to_vec());
        assert_eq!(
            fetch_html(&url).unwrap().map(|(body, _)| body).as_deref(),
            Some("<title>hi</title>")
        );
    }

    #[test]
    fn fetch_html_skips_non_html() {
        let url = serve_once("image/png", vec![0x89, b'P', b'N', b'G']);
        assert_eq!(fetch_html(&url).unwrap(), None);
    }

    #[test]
    fn fetch_html_skips_oversized_body() {
        let url = serve_once("text/html", vec![b'a'; MAX_BODY_BYTES as usize + 1]);
        assert_eq!(fetch_html(&url).unwrap(), None);
    }

    #[test]
    fn fetch_html_rejects_error_status() {
        let url = serve_status_once(
            "429 Too Many Requests",
            "text/html",
            b"<p>slow down</p>".to_vec(),
        );
        assert!(fetch_html(&url).is_err());
    }

    #[test]
    fn fetch_html_reports_connection_errors() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(fetch_html(&format!("http://127.0.0.1:{}/", port)).is_err());
    }

    use crate::store::tests::{page, temp_dir};
    use crate::store::{now_secs, Store};

    const DAY: i64 = 24 * 60 * 60;
    const GOOD: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
    const BAD: &str = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    const QM: &str = "QmbWqxBEKC3P8tqsKc98xmWNzrzDtRLMiMPL8wBuTGsMnR";

    use crate::store::tests::dir_has;

    #[test]
    fn a_takedown_hides_the_page_at_once_and_erases_it_from_every_file() {
        let dir = temp_dir("takedown-segments");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.persist(BAD, &page(BAD, "t", &[("zzqxsegmentterm91", 2)]));
        queue.persist(GOOD, &page(GOOD, "t", &[("alpha", 1)]));
        queue.flush_for_test();
        assert!(
            dir_has(&dir, "zzqxsegmentterm91"),
            "on disk before the takedown"
        );

        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        assert!(
            queue.search("zzqxsegmentterm91", 0, 10).1.is_empty(),
            "gone at once"
        );
        queue.purge_for_test(); // what the purge thread does
        queue.maintain_for_test(); // what the upkeep thread does next
        assert!(!dir_has(&dir, "zzqxsegmentterm91"));
        assert_eq!(queue.search("alpha", 0, 10).1.len(), 1);
    }

    #[test]
    fn a_takedown_of_an_old_versions_redirected_cid_erases_it() {
        let dir = temp_dir("takedown-version");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        // GOOD's item once redirected to BAD's cid; it now resolves elsewhere
        let old = page(BAD, "t", &[("zzqxredirectterm", 1)]); // its cid is BAD
        queue.persist(GOOD, &old);
        for i in 0..5 {
            let item = format!("{}/other{}", GOOD, i);
            queue.persist(&item, &page(&item, "o", &[("alpha", 1)]));
        }
        queue.flush_for_test();
        queue.persist(GOOD, &page(GOOD, "t", &[("beta", 1)]));
        queue.flush_for_test();
        queue.maintain_for_test();
        assert!(dir_has(&dir, "zzqxredirectterm"), "only in the old version");

        queue.set_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.purge_for_test();
        queue.maintain_for_test();
        assert!(!dir_has(&dir, "zzqxredirectterm"));
        assert_eq!(
            queue.search("beta", 0, 10).1.len(),
            1,
            "the current page isn't blocked"
        );
    }

    #[test]
    fn expiry_erases_old_versions_of_a_current_page() {
        let dir = temp_dir("expire-version");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.persist(GOOD, &page(GOOD, "t", &[("zzqxoldversionterm", 1)]));
        for i in 0..5 {
            let item = format!("{}/other{}", GOOD, i);
            queue.persist(&item, &page(&item, "o", &[("alpha", 1)]));
        }
        queue.flush_for_test();
        // re-crawled: the first version is superseded, and was 10 days ago
        queue.persist(GOOD, &page(GOOD, "t", &[("beta", 1)]));
        queue.flush_for_test();
        rusqlite::Connection::open(dir.join("index.sqlite"))
            .unwrap()
            .execute(
                "UPDATE dead_pages SET died_at = ?1",
                [now_secs() - 10 * DAY],
            )
            .unwrap();
        queue.maintain_for_test();
        assert!(
            dir_has(&dir, "zzqxoldversionterm"),
            "kept within the window"
        );

        assert_eq!(queue.expire(5 * DAY), 0, "the current pages are fresh");
        queue.maintain_for_test();
        assert!(!dir_has(&dir, "zzqxoldversionterm"));
        assert_eq!(queue.search("beta", 0, 10).1.len(), 1);
    }

    fn file_has(store_dir: &std::path::Path, needle: &str) -> bool {
        let bytes = std::fs::read(store_dir.join("index.sqlite")).unwrap();
        bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    fn seed(dir: &std::path::Path, rows: &[(&str, &str, i64)]) {
        let store = Store::open(dir, 0).unwrap();
        for (item, title, at) in rows {
            store
                .upsert(item, &page(item, title, &[("shared", 1), (title, 1)]), *at)
                .unwrap();
        }
    }

    fn hashed_page(item: &str, hash: &[u8]) -> IndexResult {
        let mut result = page(item, "t", &[("w", 1)]);
        result.text_hash = Some(hash.to_vec());
        result
    }

    #[test]
    fn search_hides_a_blocked_page_before_any_purge() {
        let queue = IndexQueue::new();
        queue.index_for_test(GOOD, &page(GOOD, "t", &[("alpha", 1)]));
        // swapped in without purging: what a takedown looks like before the scan reaches it
        *queue.local_denylist.write().unwrap() = Denylist::parse(&format!("{}\n", GOOD));
        assert!(queue.search("alpha", 0, 10).1.is_empty());
        assert_eq!(queue.index_length(), 1, "still on disk until the purge");
    }

    #[test]
    fn admit_skips_indexed_pages_and_mirrors_from_the_store() {
        let queue = IndexQueue::new();
        queue.persist(GOOD, &hashed_page(GOOD, b"h"));
        let mirror = format!("{}/copy", GOOD);
        queue.persist(&mirror, &hashed_page(&mirror, b"h"));
        assert_eq!(queue.mirror_length(), 1);
        assert_eq!(queue.admit(GOOD.to_string()).1, None);
        assert_eq!(queue.admit(mirror).1, None);
        assert!(queue.admit(format!("{}/new", GOOD)).1.is_some());
    }

    #[test]
    fn attach_store_doesnt_load_pages_and_keeps_the_queue() {
        let dir = temp_dir("attach-lazy");
        let store = Store::open(&dir, 0).unwrap();
        store
            .upsert(GOOD, &page(GOOD, "t", &[("alpha", 1)]), now_secs())
            .unwrap();
        store.queue_add(&[format!("{}/next", GOOD)]).unwrap();
        drop(store);
        let queue = IndexQueue::new();
        assert_eq!(
            queue
                .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
                .unwrap(),
            (1, 1)
        );
        assert_eq!(queue.search("alpha", 0, 10).1.len(), 1);
    }

    #[test]
    fn expiry_keeps_a_page_recrawled_since_it_was_listed() {
        let queue = IndexQueue::new();
        queue
            .store()
            .upsert(GOOD, &page(GOOD, "old", &[("alpha", 1)]), 1)
            .unwrap();
        // re-crawled: a fresh date under the same item
        queue
            .store()
            .upsert(GOOD, &page(GOOD, "new", &[("alpha", 1)]), now_secs())
            .unwrap();
        assert_eq!(queue.expire(DAY), 0);
        assert_eq!(queue.search("alpha", 0, 10).1[0].title, "new");
    }

    #[test]
    fn recrawl_streams_more_pages_than_a_batch() {
        let queue = IndexQueue::new();
        for i in 0..(crate::store::WRITE_BATCH + 5) {
            let item = format!("{}/p{}", GOOD, i);
            queue.index_for_test(&item, &page(&item, "t", &[]));
        }
        assert_eq!(queue.recrawl(), crate::store::WRITE_BATCH + 5);
        assert_eq!(queue.queue_length(), crate::store::WRITE_BATCH + 5);
    }

    // overwrites part of a file, the way an interrupted write leaves it
    fn damage(path: &std::path::Path, from: usize) {
        let mut bytes = std::fs::read(path).unwrap();
        let to = bytes.len().min(from + 64 * 1024);
        assert!(
            from < to,
            "{} is too small to damage at {}",
            path.display(),
            from
        );
        bytes[from..to].fill(0xAB);
        std::fs::write(path, bytes).unwrap();
    }

    fn assert_wiped_and_working(queue: &IndexQueue, dir: &std::path::Path, old_text: &str) {
        assert_eq!(queue.index_length(), 0);
        assert!(!dir_has(dir, old_text), "the damaged files' text is erased");
        queue.persist(GOOD, &page(GOOD, "t", &[("afterwipe", 1)]));
        assert_eq!(queue.search("afterwipe", 0, 10).1.len(), 1);
    }

    #[test]
    fn open_store_wipes_an_index_that_isnt_sqlite() {
        let dir = temp_dir("wipe-garbage");
        std::fs::write(
            dir.join("index.sqlite"),
            b"garbagetext: not a sqlite database at all",
        )
        .unwrap();
        let queue = IndexQueue::new();
        assert_eq!(queue.open_store(&dir, 0, 30 * DAY).unwrap(), (0, 0));
        assert_wiped_and_working(&queue, &dir, "garbagetext");
    }

    #[test]
    fn open_store_wipes_an_index_damaged_mid_file() {
        // what prod saw: valid header, "database disk image is malformed" further in
        let dir = temp_dir("wipe-malformed");
        let rows: Vec<(String, String)> = (0..400)
            .map(|i| {
                (
                    format!("{}/p{}", GOOD, i),
                    format!("damagedtitle{:0>200}", i),
                )
            })
            .collect();
        let rows: Vec<(&str, &str, i64)> = rows
            .iter()
            .map(|(item, title)| (item.as_str(), title.as_str(), now_secs()))
            .collect();
        seed(&dir, &rows);
        let file = dir.join("index.sqlite");
        assert!(std::fs::metadata(&file).unwrap().len() > 3 * 4096);
        damage(&file, 2 * 4096);
        let err = Store::open(&dir, 0)
            .and_then(|s| Ok(IndexQueue::new().attach_store(s, 30 * DAY)?))
            .err()
            .expect("the damage is noticed");
        assert!(store::is_corrupt(&*err), "{}", err);

        let queue = IndexQueue::new();
        assert_eq!(queue.open_store(&dir, 0, 30 * DAY).unwrap(), (0, 0));
        assert_wiped_and_working(&queue, &dir, "damagedtitle");
    }

    #[test]
    fn open_store_wipes_an_index_with_a_damaged_segment() {
        let dir = temp_dir("wipe-segment");
        let queue = IndexQueue::new();
        queue.open_store(&dir, 0, 30 * DAY).unwrap();
        queue.persist(BAD, &page(BAD, "t", &[("segmenttext77", 1)]));
        queue.flush_for_test();
        drop(queue);
        let segment = std::fs::read_dir(dir.join("segments"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "seg"))
            .expect("a segment was written");
        damage(&segment, 16);

        let queue = IndexQueue::new();
        assert_eq!(queue.open_store(&dir, 0, 30 * DAY).unwrap(), (0, 0));
        assert_wiped_and_working(&queue, &dir, "segmenttext77");
    }

    #[test]
    fn open_store_leaves_an_old_index_or_a_bad_data_dir_alone() {
        let dir = temp_dir("no-wipe");
        rusqlite::Connection::open(dir.join("index.sqlite"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE pages (id INTEGER PRIMARY KEY, item TEXT NOT NULL UNIQUE,
                   cid TEXT NOT NULL, title TEXT NOT NULL, excerpt TEXT NOT NULL,
                   indexed_at INTEGER NOT NULL);
                 INSERT INTO pages VALUES (1, 'bafyA', 'bafyA', 'oldformattitle', 'e', 5);",
            )
            .unwrap();
        let err = IndexQueue::new()
            .open_store(&dir, 0, 30 * DAY)
            .err()
            .unwrap();
        assert!(err.is::<store::OldFormat>(), "{}", err);
        assert!(dir_has(&dir, "oldformattitle"), "kept for the operator");

        let file = dir.join("a-file");
        std::fs::write(&file, b"keepme").unwrap();
        assert!(IndexQueue::new().open_store(&file, 0, 30 * DAY).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keepme");
    }

    #[test]
    fn attach_reloads_pages_and_keywords() {
        let dir = temp_dir("attach");
        seed(&dir, &[(GOOD, "goodtitle", now_secs())]);
        let queue = IndexQueue::new();
        let (pages, queued) = queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert_eq!((pages, queued), (1, 0));
        assert_eq!(queue.search("goodtitle", 0, usize::MAX).1.len(), 1);
        assert_eq!(queue.search("shared", 0, usize::MAX).1.len(), 1);
    }

    #[test]
    fn the_purge_and_expiry_after_attach_drop_denylisted_and_expired_rows_from_disk() {
        let dir = temp_dir("recheck");
        seed(
            &dir,
            &[
                (GOOD, "keeptitle", now_secs()),
                (&format!("{}/page", BAD), "blockedtitle", now_secs()),
                (
                    "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze/old",
                    "expiredtitle",
                    now_secs() - 40 * DAY,
                ),
            ],
        );
        let queue = IndexQueue::new();
        // local takedown list loaded before attach, as main.rs does
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test(); // the one attach_store queued
        assert_eq!(queue.expire(30 * DAY), 1);
        assert_eq!(queue.index_length(), 1);
        assert!(!file_has(&dir, "blockedtitle"));
        assert!(!file_has(&dir, "expiredtitle"));
        assert!(file_has(&dir, "keeptitle"));
    }

    #[test]
    fn purge_deletes_from_disk() {
        let dir = temp_dir("purge");
        seed(
            &dir,
            &[
                (&format!("{}/page", BAD), "purgetitle", now_secs()),
                (GOOD, "stays", now_secs()),
            ],
        );
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert!(file_has(&dir, "purgetitle"));

        queue.set_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.purge_for_test();
        assert_eq!(queue.index_length(), 1);
        assert!(!file_has(&dir, "purgetitle"), "takedown reaches the disk");
        assert!(file_has(&dir, "stays"));
    }

    #[test]
    fn expire_removes_old_pages_from_disk() {
        let dir = temp_dir("expire");
        seed(&dir, &[(GOOD, "agingtitle", now_secs() - 10 * DAY)]);
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert_eq!(queue.expire(30 * DAY), 0);
        assert_eq!(queue.expire(5 * DAY), 1);
        assert_eq!(queue.index_length(), 0);
        assert!(!file_has(&dir, "agingtitle"));
    }

    #[test]
    fn crawled_pages_are_persisted_but_not_if_blocked_meanwhile() {
        let dir = temp_dir("crawl");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.persist(GOOD, &page(GOOD, "crawledtitle", &[]));
        assert!(file_has(&dir, "crawledtitle"));

        // a takedown that lands between fetch and insert must not be re-written by the worker
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.persist(BAD, &page(BAD, "racetitle", &[]));
        assert!(!file_has(&dir, "racetitle"));
    }

    #[test]
    fn persist_cannot_interleave_with_a_denylist_swap_and_purge() {
        // review #1: a worker that checked against the OLD list must not write after the purge
        let dir = temp_dir("race");
        let queue = std::sync::Arc::new(IndexQueue::new());
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();

        let swap = queue.writes.write().unwrap(); // a setter is mid swap+purge
        let worker = {
            let queue = std::sync::Arc::clone(&queue);
            std::thread::spawn(move || queue.persist(BAD, &page(BAD, "racetitle2", &[])))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        *queue.local_denylist.write().unwrap() = Denylist::parse(&format!("{}\n", BAD));
        drop(swap);
        worker.join().unwrap();

        assert_eq!(queue.index_length(), 0);
        assert!(!file_has(&dir, "racetitle2"));
    }

    #[test]
    fn reseed_requeues_the_seed_after_its_page_expires() {
        // review #3: without a re-seed, a long-running instance empties itself 30 days on
        let dir = temp_dir("reseed");
        let seed = SEED_CIDS[0];
        seed_rows(&dir, seed, now_secs() - 40 * DAY);
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 50 * DAY)
            .unwrap();
        assert_eq!(queue.reseed(), 0, "seed already indexed: nothing queued");
        assert_eq!(queue.expire(30 * DAY), 1);
        assert_eq!(queue.reseed(), 1, "seed queued again so the crawl restarts");
        assert_eq!(queue.queue_length(), 1);
    }

    fn seed_rows(dir: &std::path::Path, item: &str, at: i64) {
        Store::open(dir, 0)
            .unwrap()
            .upsert(item, &page(item, "seedpage", &[]), at)
            .unwrap();
    }

    #[test]
    fn enqueue_normalizes_so_each_page_is_queued_once() {
        let queue = IndexQueue::new();
        for spelling in [
            "wiki/X",
            "wiki//X",
            "wiki/a/../X",
            "wiki/./X/",
            "wiki/X#top",
        ] {
            assert_eq!(
                queue.enqueue(format!("{}/{}", GOOD, spelling)),
                Enqueued::Queued
            );
        }
        assert_eq!(queue.queue_length(), 1);
        assert_eq!(
            queue.store().queue_pop().unwrap().unwrap(),
            format!("{}/wiki/X", GOOD)
        );
    }

    #[test]
    fn enqueue_rejects_paths_that_climb_out_of_their_cid() {
        let queue = IndexQueue::new();
        assert_eq!(
            queue.enqueue(format!("{}/../{}", GOOD, BAD)),
            Enqueued::InvalidCid
        );
        assert_eq!(queue.queue_length(), 0);
    }

    #[test]
    fn keywords_rank_by_number_of_pages_and_search_finds_each_page() {
        let queue = IndexQueue::new();
        let pages = [
            ("bafyP1", &["common", "rare"][..]),
            ("bafyP2", &["common"][..]),
            ("bafyP3", &["common", "middle"][..]),
            ("bafyP4", &["middle"][..]),
        ];
        for (name, words) in pages {
            let counts: HashMap<String, u32> = words.iter().map(|w| (w.to_string(), 5)).collect();
            let item = format!("{}/{}", GOOD, name);
            queue.index_for_test(
                &item,
                &IndexResult::new(item.clone(), "t".into(), "e".into(), counts),
            );
        }
        assert_eq!(queue.keyword_length(), 3);
        assert_eq!(
            queue.top_keywords(2),
            vec![("common".to_string(), 3), ("middle".to_string(), 2)]
        );
        let mut common = found(&queue, "common");
        common.sort();
        assert_eq!(common, vec!["bafyP1", "bafyP2", "bafyP3"]);
        // re-indexing a page replaces its postings: no double count, and terms it lost go
        index_text(&queue, "bafyP1", "common");
        assert_eq!(queue.top_keywords(1), vec![("common".to_string(), 3)]);
        assert!(queue.search("rare", 0, usize::MAX).1.is_empty());
        assert_eq!(queue.keyword_length(), 2);
        assert!(queue.search("absent", 0, usize::MAX).1.is_empty());
    }

    #[test]
    fn keyword_counts_live_on_disk_not_in_search_results() {
        // #220: per-page counts duplicated the inverted index in RAM and bloated /search output
        let dir = temp_dir("counts");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.persist(GOOD, &page(GOOD, "t", &[("alpha", 3)]));
        let (_, results) = queue.search("alpha", 0, usize::MAX);
        assert_eq!(results.len(), 1, "still searchable");
        assert!(results[0].keywords.is_empty(), "results carry no counts");
        queue.flush_for_test(); // a second Store would treat the buffered page as lost
        assert_eq!(
            crate::store::tests::postings_by_item(&Store::open(&dir, 0).unwrap(), "alpha"),
            vec![(GOOD.to_string(), 3, 3)],
            "counts kept on disk"
        );

        // and after a reload: searchable
        let reloaded = IndexQueue::new();
        reloaded
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert_eq!(reloaded.search("alpha", 0, usize::MAX).1.len(), 1);
    }

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn links_resolve_like_a_browser_against_the_final_url() {
        let c = GOOD;
        let index = url(&format!("https://ipfs.filebase.io/ipfs/{}/wiki/", c)); // directory page
        let article = url(&format!(
            "https://ipfs.filebase.io/ipfs/{}/wiki/Antarctica",
            c
        ));
        let r = |base: &reqwest::Url, href: &str| resolve_link(base, href);
        // the trailing slash matters: wiki/ + Antarctic is wiki/Antarctic, not <cid>/Antarctic
        assert_eq!(
            r(&index, "Antarctic"),
            Some(format!("{}/wiki/Antarctic", c))
        );
        assert_eq!(r(&article, "Earth"), Some(format!("{}/wiki/Earth", c)));
        assert_eq!(
            r(&article, "./Earth#History"),
            Some(format!("{}/wiki/Earth", c))
        );
        // the mirror renamed A/ to wiki/; its index still links ../A/X (a 404 as written)
        assert_eq!(
            r(&index, "../A/Wikipedia"),
            Some(format!("{}/wiki/Wikipedia", c))
        );
        // ../Antarctic is NOT mangled into "ntarctic" any more
        assert_eq!(
            r(&article, "../Antarctic"),
            Some(format!("{}/Antarctic", c))
        );
        // absolute links on the same gateway, to another CID
        assert_eq!(
            r(
                &article,
                &format!("https://ipfs.filebase.io/ipfs/{}/page", BAD)
            ),
            Some(format!("{}/page", BAD))
        );
        // the mirror hides schemes behind "./" (./geo:-69.3,-2.2): still not pages
        assert_eq!(r(&article, "./geo:-69.35,-2.24"), None);
        assert_eq!(r(&article, "./ftp:/ftp.example.org/x.txt"), None);
        // ...but titles that merely contain a colon are pages
        assert_eq!(
            r(&article, "Star_Trek:_The_Next_Generation"),
            Some(format!("{}/wiki/Star_Trek:_The_Next_Generation", c))
        );
        // not crawlable: other schemes, other hosts, fragments, climbing out of /ipfs/
        for href in [
            "geo:-69.35,-2.24",
            "mailto:x@y.z",
            "javascript:void(0)",
            "#top",
            "",
            "https://en.wikipedia.org/wiki/X",
            "https://ipfs.io/ipfs/bafyOther",
            "../../../../etc",
        ] {
            assert_eq!(r(&article, href), None, "{}", href);
        }
    }

    #[test]
    fn a_missing_page_is_not_a_gateway_error() {
        // a 404 means the gateway answered; only network errors, 5xx and 429 mean it's unhealthy
        let queue = IndexQueue::new();
        let host = |u: String| {
            u.trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string()
        };
        queue.retrieve_content(
            host(serve_status_once(
                "404 Not Found",
                "text/plain",
                b"no".to_vec(),
            )),
            "bafy".into(),
        );
        assert!(
            queue.gateway_health().starts_with("ok "),
            "{}",
            queue.gateway_health()
        );
        queue.retrieve_content(
            host(serve_status_once(
                "500 Internal Server Error",
                "text/plain",
                b"x".to_vec(),
            )),
            "bafy".into(),
        );
        assert!(
            queue.gateway_health().starts_with("error "),
            "{}",
            queue.gateway_health()
        );
        queue.retrieve_content(
            host(serve_status_once(
                "429 Too Many Requests",
                "text/plain",
                b"x".to_vec(),
            )),
            "bafy".into(),
        );
        assert!(
            queue.gateway_health().starts_with("error "),
            "{}",
            queue.gateway_health()
        );
    }

    #[test]
    fn a_long_queue_loses_no_links() {
        // #224: the 1000-slot queue dropped pushes but still marked the items as queued
        let queue = IndexQueue::new();
        let items: Vec<String> = (0..2500).map(|i| format!("{}/wiki/P{}", GOOD, i)).collect();
        queue.enqueue_many(items.clone());
        assert_eq!(queue.queue_length(), 2500);
        assert_eq!(queue.enqueue(items[0].clone()), Enqueued::Queued);
        assert_eq!(queue.queue_length(), 2500, "still no duplicates");
    }

    #[test]
    fn the_frontier_survives_a_restart() {
        let dir = temp_dir("frontier");
        let first = IndexQueue::new();
        first
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        first.enqueue_many(vec![
            format!("{}/wiki/A", GOOD),
            format!("{}/wiki/B", GOOD),
            format!("{}/x", BAD),
        ]);
        // wiki/A got indexed meanwhile
        Store::open(&dir, 0)
            .unwrap()
            .upsert(
                &format!("{}/wiki/A", GOOD),
                &page(&format!("{}/wiki/A", GOOD), "a", &[]),
                now_secs(),
            )
            .unwrap();
        drop(first);

        let second = IndexQueue::new();
        second.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        second
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        // all of it, in order (#231): an indexed item is fetched again, as a recrawl would
        assert_eq!(
            Store::open(&dir, 0).unwrap().queue_load().unwrap(),
            vec![
                format!("{}/wiki/A", GOOD),
                format!("{}/wiki/B", GOOD),
                format!("{}/x", BAD)
            ]
        );
        // a blocked one is skipped when popped, never fetched
        second.store().queue_pop().unwrap();
        second.store().queue_pop().unwrap();
        assert!(second.process_next("127.0.0.1:1"));
        assert_eq!(second.gateway_health(), "no fetches yet");
        assert_eq!(second.queue_length(), 0);
    }

    #[test]
    fn processed_items_leave_the_saved_frontier() {
        let dir = temp_dir("processed");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.enqueue(GOOD.to_string());
        let gateway = serve_once(
            "text/html",
            b"<title>t</title><body>words here</body>".to_vec(),
        )
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
        assert!(queue.process_next(&gateway));
        queue.flush_for_test();
        assert!(Store::open(&dir, 0)
            .unwrap()
            .queue_load()
            .unwrap()
            .is_empty());
        assert!(!queue.process_next(&gateway), "nothing left");
    }

    // crawls item from a one-shot gateway serving body; returns the links it queued
    fn crawl(queue: &IndexQueue, item: &str, body: &str) -> Vec<String> {
        assert_eq!(queue.queue_length(), 0);
        queue.enqueue(item.to_string());
        let gateway = serve_once("text/html", body.as_bytes().to_vec())
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        assert!(queue.process_next(&gateway));
        std::iter::from_fn(|| queue.store().queue_pop().unwrap()).collect()
    }

    #[test]
    fn a_mirror_of_an_indexed_page_is_neither_indexed_nor_followed() {
        let dir = temp_dir("mirrors");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        let (original, mirror) = (format!("{}/wiki/A", GOOD), format!("{}/wiki/A", BAD));
        // the same text; the links (attributes, not text) differ
        let links = crawl(
            &queue,
            &original,
            "<body>same words <a href='B'></a></body>",
        );
        assert_eq!(links, vec![format!("{}/wiki/B", GOOD)]);
        let links = crawl(
            &queue,
            &mirror,
            "<body> same\n words <a href='C'></a></body>",
        );
        assert!(links.is_empty(), "the mirror's links aren't followed");
        assert_eq!((queue.index_length(), queue.mirror_length()), (1, 1));
        assert_eq!(found(&queue, "words"), vec!["wiki/A"]);
        queue.enqueue(mirror.clone());
        assert_eq!(queue.queue_length(), 0, "nor is it queued again");
        // pages without text aren't mirrors of each other
        assert_eq!(
            crawl(
                &queue,
                &format!("{}/e1", GOOD),
                "<body><a href='D'></a></body>"
            )
            .len(),
            1
        );
        assert_eq!(
            crawl(
                &queue,
                &format!("{}/e2", GOOD),
                "<body><a href='E'></a></body>"
            )
            .len(),
            1
        );
        assert_eq!((queue.index_length(), queue.mirror_length()), (3, 1));

        // remembered across a restart, and expires with the original
        drop(queue);
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert_eq!((queue.index_length(), queue.mirror_length()), (3, 1));
        // indexed this second: a cutoff just past now
        assert_eq!(queue.expire(-1), 3);
        assert_eq!(queue.mirror_length(), 0);
        queue.enqueue(mirror);
        assert_eq!(queue.queue_length(), 1, "crawlable again");
    }

    #[test]
    fn the_indexing_rate_counts_pages_written_not_mirrors_or_blocked_ones() {
        let queue = IndexQueue::new();
        let hashed = |item: &str| page(item, "t", &[]).with_text_hash(b"h".to_vec());
        assert!(queue.persist(GOOD, &hashed(GOOD)));
        let mirror = format!("{}/mirror", GOOD);
        assert!(!queue.persist(&mirror, &hashed(&mirror)), "a mirror");
        let blocked = format!("{}/page", BAD);
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        assert!(
            !queue.persist(&blocked, &page(&blocked, "t", &[])),
            "blocked"
        );
        let other = format!("{}/other", GOOD);
        assert!(queue.persist(&other, &page(&other, "t", &[("alpha", 1)])));
        assert_eq!(queue.indexed.counted(), 2);
        assert_eq!(queue.indexed_per_minute(), 0, "no full minute yet");
    }

    #[test]
    fn takedowns_drop_mirrors() {
        let dir = temp_dir("mirrortakedown");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        let hashed = |item: &str| page(item, "t", &[]).with_text_hash(b"h".to_vec());
        let (good_b, bad_b) = (format!("{}/b", GOOD), format!("{}/b", BAD));
        assert!(queue.persist(GOOD, &hashed(GOOD)));
        assert!(!queue.persist(&bad_b, &hashed(&bad_b)));
        assert!(!queue.persist(&good_b, &hashed(&good_b)));
        assert_eq!(queue.mirror_length(), 2);

        // a blocked mirror goes, from disk too
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.purge_for_test();
        assert_eq!(queue.mirror_length(), 1);
        assert!(!file_has(&dir, &bad_b));
        // so do the mirrors of a blocked original, which can then be crawled themselves
        queue.set_denylist(Denylist::parse(&format!("{}\n", GOOD)));
        queue.purge_for_test();
        assert_eq!((queue.index_length(), queue.mirror_length()), (0, 0));
        assert!(Store::open(&dir, 0)
            .unwrap()
            .mirrors_load()
            .unwrap()
            .is_empty());

        // and a blocked mirror is dropped by the purge after a restart (blocked while the index
        // was down)
        let dir = temp_dir("mirrorrestart");
        {
            let store = Store::open(&dir, 0).unwrap();
            store.upsert(GOOD, &hashed(GOOD), now_secs()).unwrap();
            store.upsert(&bad_b, &hashed(&bad_b), now_secs()).unwrap();
        }
        let queue = IndexQueue::new();
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test(); // the one attach_store queued
        assert_eq!((queue.index_length(), queue.mirror_length()), (1, 0));
        assert!(!file_has(&dir, &bad_b));
    }

    #[test]
    fn store_write_failure_does_not_panic() {
        let dir = temp_dir("fail");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        // e.g. disk full / read-only remount
        queue.store.get().unwrap().break_for_test();
        // upsert fails: logs, doesn't panic, and its links are still followed
        assert!(queue.persist(GOOD, &page(GOOD, "t", &[])));
        assert_eq!(queue.expire(0), 0, "expire tolerates the broken store too");
        queue.set_denylist(Denylist::parse(&format!("{}\n", GOOD)));
        queue.purge_for_test(); // its scan fails: logs only
        assert!(queue.search("t", 0, 10).1.is_empty(), "search too");
    }

    #[test]
    fn expiry_that_fails_leaves_pages_for_the_next_run() {
        let dir = temp_dir("expire-fails");
        seed(&dir, &[(GOOD, "agingtitle", now_secs() - 10 * DAY)]);
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        // finding the old pages works, deleting them doesn't
        queue.store.get().unwrap().drop_table_for_test("mirrors");
        assert_eq!(queue.expire(5 * DAY), 0);
        assert_eq!(
            queue
                .store
                .get()
                .unwrap()
                .older_than(now_secs(), 10)
                .unwrap(),
            vec![GOOD.to_string()],
            "still on disk, for the next expiry"
        );
    }

    // index a page's text the way process_content does
    // under GOOD, and found() by `name`: search serves only real CIDs (it can't check a list
    // against anything else, so it counts as blocked)
    fn index_text(queue: &IndexQueue, name: &str, text: &str) {
        let mut counts = HashMap::new();
        for word in normalize(text) {
            *counts.entry(word).or_insert(0) += 1;
        }
        let item = format!("{}/{}", GOOD, name);
        queue.index_for_test(
            &item,
            &IndexResult::new(item.clone(), "t".into(), "e".into(), counts),
        );
    }

    fn name_of(result: IndexResult) -> String {
        result.cid[GOOD.len() + 1..].to_string()
    }

    fn found(queue: &IndexQueue, query: &str) -> Vec<String> {
        queue
            .search(query, 0, usize::MAX)
            .1
            .into_iter()
            .map(name_of)
            .collect()
    }

    #[test]
    fn normalize_lowercases_splits_on_punctuation_and_drops_stopwords() {
        assert_eq!(
            normalize("The Files. of Ada-Lovelace, NFT art: Ünïcode 3d"),
            vec!["files", "ada", "lovelace", "nft", "art", "ünïcode", "3d"]
        );
        assert!(normalize(" .,- the ").is_empty());
    }

    #[test]
    fn search_ands_terms_and_ignores_case_and_punctuation() {
        let queue = IndexQueue::new();
        index_text(&queue, "bafyBoth", "Ada Lovelace wrote the notes.");
        index_text(&queue, "bafyAda", "Ada, the language");
        index_text(&queue, "bafyNft", "an NFT art DAO");
        assert_eq!(found(&queue, "ada lovelace"), vec!["bafyBoth"]);
        assert_eq!(found(&queue, "LOVELACE"), vec!["bafyBoth"]);
        assert_eq!(found(&queue, "notes"), vec!["bafyBoth"]);
        assert_eq!(found(&queue, "nft"), vec!["bafyNft"]);
        assert!(found(&queue, "ada nft").is_empty());
        assert!(found(&queue, "the").is_empty());
        assert!(found(&queue, "").is_empty());
    }

    #[test]
    fn parse_query_groups_terms_joined_by_or() {
        let groups = |query: &str| parse_query(query);
        assert_eq!(groups("ada lovelace"), vec![vec!["ada"], vec!["lovelace"]]);
        assert_eq!(groups("ada OR nft"), vec![vec!["ada", "nft"]]);
        // OR binds tighter than the implicit AND
        assert_eq!(
            groups("rust ipfs OR filecoin"),
            vec![vec!["rust"], vec!["ipfs", "filecoin"]]
        );
        assert_eq!(groups("x OR y OR z"), vec![vec!["x", "y", "z"]]);
        // lowercase "or" is a stopword, and a stray or doubled OR is ignored
        assert_eq!(groups("ada or nft"), vec![vec!["ada"], vec!["nft"]]);
        assert_eq!(groups("OR ada OR OR nft OR"), vec![vec!["ada", "nft"]]);
        // a skipped word doesn't break the chain; OR joins the words beside it in a token
        assert_eq!(groups("ada OR the OR nft"), vec![vec!["ada", "nft"]]);
        assert_eq!(
            groups("ada-lovelace OR nft"),
            vec![vec!["ada"], vec!["lovelace", "nft"]]
        );
        assert_eq!(groups("Ada OR ada"), vec![vec!["ada"]]);
        assert!(groups("OR the").is_empty());
    }

    #[test]
    fn search_ors_terms_and_scores_by_the_terms_each_page_has() {
        let queue = IndexQueue::new();
        index_text(&queue, "bafyBoth", "Ada Lovelace wrote the notes.");
        index_text(&queue, "bafyAda", "Ada, the language");
        index_text(&queue, "bafyNft", "an NFT art DAO");
        index_text(&queue, "bafyOther", "unrelated");
        // pages with more of the terms rank higher
        assert_eq!(
            found(&queue, "ada OR lovelace"),
            vec!["bafyBoth", "bafyAda"]
        );
        let mut any = found(&queue, "ada OR nft");
        any.sort();
        assert_eq!(any, vec!["bafyAda", "bafyBoth", "bafyNft"]);
        // a term that matches nothing doesn't empty the results
        assert_eq!(found(&queue, "absent OR nft"), vec!["bafyNft"]);
        assert!(found(&queue, "absent OR missing").is_empty());
        // each AND group still has to match
        assert_eq!(found(&queue, "notes ada OR nft"), vec!["bafyBoth"]);
        assert!(found(&queue, "absent ada OR nft").is_empty());
        assert_eq!(queue.search("ada OR nft", 1, 1).0, 3);
    }

    #[test]
    fn search_ranks_by_bm25_and_pages_with_offset_and_limit() {
        let queue = IndexQueue::new();
        index_text(&queue, "bafyOnce", "rust ipfs");
        index_text(&queue, "bafyThrice", "rust rust rust ipfs");
        index_text(&queue, "bafyRare", "rust ipfs crawler");
        index_text(&queue, "bafyOther", "unrelated");
        // more occurrences rank higher
        assert_eq!(
            found(&queue, "rust"),
            vec!["bafyThrice", "bafyOnce", "bafyRare"]
        );
        // a rarer term outweighs extra occurrences of a common one
        assert_eq!(found(&queue, "rust crawler"), vec!["bafyRare"]);
        assert_eq!(found(&queue, "rust ipfs crawler")[0], "bafyRare");
        let (total, page) = queue.search("rust", 1, 1);
        assert_eq!(total, 3);
        assert_eq!(
            page.into_iter().map(name_of).collect::<Vec<_>>(),
            vec!["bafyOnce"]
        );
        let (total, page) = queue.search("rust", 5, 10);
        assert_eq!((total.n, page.len()), (3, 0));
    }

    #[test]
    fn search_ranks_at_most_a_thousand_and_reports_the_total_past_them() {
        let queue = IndexQueue::new();
        for i in 0..1005 {
            index_text(&queue, &format!("bafyPage{}", i), "rust");
        }
        let (total, page) = queue.search("rust", 0, usize::MAX);
        assert_eq!((total.n, page.len()), (1005, 1000));
        let (total, page) = queue.search("rust", 995, 10);
        assert_eq!((total.n, page.len()), (1005, 5));
        let (total, page) = queue.search("rust", 1000, 10);
        assert_eq!(
            (total.n, page.len()),
            (1005, 0),
            "past the ranked: the total, no results"
        );
    }

    #[test]
    fn search_ranks_focused_pages_above_long_ones_mentioning_a_term_in_passing() {
        let queue = IndexQueue::new();
        // "bafyALong" would win the tie on item name under TF-IDF
        index_text(
            &queue,
            "bafyALong",
            "rust mentioned once among many other unrelated words about gardening cooking travel",
        );
        index_text(&queue, "bafyBShort", "rust crawler");
        index_text(&queue, "bafyOther", "unrelated");
        assert_eq!(found(&queue, "rust"), vec!["bafyBShort", "bafyALong"]);
    }

    #[test]
    fn purged_and_expired_pages_leave_the_keyword_index() {
        // they used to linger in the in-memory index, only filtered out at search time
        let dir = temp_dir("purge-postings");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.persist(
            GOOD,
            &page(GOOD, "t", &[("takedownword", 1), ("shared", 1)]),
        );
        queue.persist(BAD, &page(BAD, "t", &[("shared", 1)]));
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", GOOD)));
        queue.purge_for_test();
        let store = queue.store.get().unwrap();
        assert!(store.postings("takedownword").unwrap().is_empty());
        assert_eq!(
            crate::store::tests::postings_by_item(&store, "shared"),
            vec![(BAD.to_string(), 1, 1)]
        );
        store.refresh_stats_for_test();
        assert_eq!(queue.keyword_length(), 1);
        assert!(
            !file_has(&dir, "takedownword"),
            "secure_delete covers postings too"
        );
        assert_eq!(queue.expire(-DAY), 1);
        store.refresh_stats_for_test();
        assert_eq!(queue.keyword_length(), 0);
    }

    #[test]
    fn search_is_the_same_buffered_flushed_merged_and_reopened() {
        // twelve pages with varied term counts and lengths, so BM25's order isn't trivial
        let pages: Vec<(String, Vec<(&str, u32)>)> = (0..12u32)
            .map(|i| {
                let mut words = vec![("rust", i % 4 + 1), ("filler", 1 + i * 7 % 11)];
                if i % 3 == 0 {
                    words.push(("ipfs", 2));
                }
                if i % 2 == 1 {
                    words.push(("crawler", 1 + i % 3));
                }
                if i == 7 {
                    words.push(("zzqxrare", 1));
                }
                (format!("{}/p{}", GOOD, i), words)
            })
            .collect();
        let queries = ["rust ipfs", "ipfs OR crawler", "zzqxrare", "rust"];
        let results = |queue: &IndexQueue| -> Vec<(usize, Vec<String>)> {
            queries
                .iter()
                .map(|query| {
                    let (total, found) = queue.search(query, 0, 100);
                    (total.n, found.into_iter().map(|r| r.cid).collect())
                })
                .collect()
        };
        let attached = |dir: &std::path::Path| {
            let queue = IndexQueue::new();
            queue
                .attach_store(Store::open(dir, 0).unwrap(), 30 * DAY)
                .unwrap();
            queue
        };
        let segments = |queue: &IndexQueue| queue.store.get().unwrap().segment_count();

        // all buffered, then flushed into one segment
        let one = attached(&temp_dir("parity-one"));
        for (item, words) in &pages {
            one.index_for_test(item, &page(item, "t", words));
        }
        let buffered = results(&one);
        one.flush_for_test();
        assert_eq!(segments(&one), 1);
        let flushed = results(&one);

        // a segment per page, then ten of them merged
        let dir = temp_dir("parity-merged");
        let many = attached(&dir);
        for (item, words) in &pages {
            many.index_for_test(item, &page(item, "t", words));
            many.flush_for_test();
        }
        assert_eq!(segments(&many), 12);
        many.maintain_for_test();
        assert_eq!(segments(&many), 3, "ten merged, two left");
        let merged = results(&many);
        drop(many);
        let reopened = results(&attached(&dir));

        let by_id: Vec<String> = pages.iter().map(|(item, _)| item.clone()).collect();
        assert_eq!(buffered[3].0, 12);
        assert_ne!(buffered[3].1, by_id, "ranked, not just in id order");
        assert_eq!(buffered[1].0, 8, "the OR query");
        assert_eq!(buffered[2], (1, vec![format!("{}/p7", GOOD)]));
        assert_eq!(flushed, buffered, "after a flush");
        assert_eq!(merged, buffered, "after a merge");
        assert_eq!(reopened, buffered, "after a reopen");
    }

    #[test]
    fn an_unchanged_local_list_queues_no_purge() {
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open_in_memory().unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test(); // the one attach_store queued
        let list = format!("{}\n", BAD);
        queue.set_local_denylist(Denylist::parse(&list));
        assert!(*queue.purge_due.lock().unwrap());
        queue.purge_for_test();
        queue.set_local_denylist(Denylist::parse(&list));
        assert!(
            !*queue.purge_due.lock().unwrap(),
            "same list: nothing to scan"
        );
    }

    #[test]
    fn a_takedown_is_hidden_at_once_and_erased_by_the_purge() {
        let dir = temp_dir("purge-erases");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test();
        queue.persist(BAD, &page(BAD, "t", &[("zzqxpurgeterm", 1)]));
        queue.flush_for_test();
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        assert!(
            queue.search("zzqxpurgeterm", 0, 10).1.is_empty(),
            "hidden at once"
        );
        assert!(
            dir_has(&dir, "zzqxpurgeterm"),
            "not erased until the purge runs"
        );
        queue.purge_for_test();
        queue.maintain_for_test();
        assert!(!dir_has(&dir, "zzqxpurgeterm"));
        assert_eq!(queue.index_length(), 0);
    }

    #[test]
    fn undeny_before_the_purge_keeps_the_page() {
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open_in_memory().unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test();
        queue.persist(GOOD, &page(GOOD, "t", &[("alpha", 1)]));
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", GOOD)));
        queue.set_local_denylist(Denylist::empty()); // undone before the scan
        queue.purge_for_test();
        assert_eq!(queue.index_length(), 1);
        assert_eq!(queue.search("alpha", 0, 10).1.len(), 1);
    }

    #[test]
    fn a_denylist_change_during_a_purge_runs_another() {
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open_in_memory().unwrap(), 30 * DAY)
            .unwrap();
        // the purge thread has taken the due flag and is scanning...
        assert!(queue.take_purge_due());
        // ...when a takedown lands
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        assert!(
            *queue.purge_due.lock().unwrap(),
            "the next round scans again"
        );
    }

    #[test]
    fn a_purge_the_store_fails_queues_another_round() {
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open_in_memory().unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test(); // the one attach_store queued
        queue.persist(BAD, &page(BAD, "t", &[("alpha", 1)]));
        queue.store.get().unwrap().drop_table_for_test("mirrors");
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.purge_for_test(); // the mirrors scan fails every try
        assert!(
            *queue.purge_due.lock().unwrap(),
            "another round is due rather than the takedown being dropped"
        );
    }

    #[test]
    fn a_purge_whose_old_versions_fail_to_read_queues_another_round() {
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open_in_memory().unwrap(), 30 * DAY)
            .unwrap();
        queue.purge_for_test(); // the one attach_store queued
        queue.persist(GOOD, &page(GOOD, "t", &[("alpha", 1)]));
        // pages and mirrors scan fine (nothing to delete); dead versions can't be read
        queue.store.get().unwrap().drop_table_for_test("dead_pages");
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.purge_for_test();
        assert!(
            *queue.purge_due.lock().unwrap(),
            "another round is due for the versions it couldn't check"
        );
    }

    #[test]
    fn read_retrying_tries_again_then_gives_up() {
        let tries = std::cell::Cell::new(0);
        let flaky = || {
            tries.set(tries.get() + 1);
            if tries.get() < 2 {
                Err(rusqlite::Error::InvalidQuery)
            } else {
                Ok(tries.get())
            }
        };
        assert_eq!(read_retrying("pages", flaky), Some(2), "a second try reads");
        tries.set(0);
        let broken = || -> rusqlite::Result<()> {
            tries.set(tries.get() + 1);
            Err(rusqlite::Error::InvalidQuery)
        };
        assert_eq!(read_retrying("pages", broken), None);
        assert_eq!(tries.get(), PURGE_READ_TRIES);
    }

    #[test]
    fn purge_works_without_a_data_dir() {
        let queue = IndexQueue::new(); // in-memory store on first use
        queue.persist(BAD, &page(BAD, "t", &[("alpha", 1)]));
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", BAD)));
        queue.purge_for_test();
        assert_eq!(queue.index_length(), 0);
    }
}
