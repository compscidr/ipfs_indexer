use crate::denylist::{normalize_item, Denylist};
use crate::index_result::IndexResult;
use crate::store::{now_secs, Store};
use actix_web::body::MessageBody;
use cid::Cid;
use dashmap::{DashMap, Map};
use log::{info, trace, warn};
use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::ops::Not;
use std::sync::{Mutex, OnceLock, RwLock};
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

// The in-memory copy of a page: its keyword counts live in the store's postings (#220, #231).
fn without_keywords(result: &IndexResult) -> IndexResult {
    IndexResult::new(
        result.cid.clone(),
        result.title.clone(),
        result.excerpt.clone(),
        HashMap::new(),
    )
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
    // index results (cid -> result)
    pub map: DashMap<String, IndexResult>,

    // badbits and the hand-maintained local list, checked before every enqueue and fetch
    denylist: RwLock<Denylist>,
    local_denylist: RwLock<Denylist>,

    // when the gateway last answered a fetch, and the error if it didn't
    last_fetch: Mutex<Option<(Instant, Result<(), String>)>>,

    // the persisted index (#106), the inverted index and the crawl queue (#231). Set by
    // attach_store, or an in-memory one on first use when DATA_DIR isn't configured.
    store: OnceLock<Store>,

    // persist() holds this shared across its check + insert + upsert; denylist swaps and
    // expire() hold it exclusively, so a page checked against an old list can't be written
    // after the purge that should have removed it
    writes: RwLock<()>,

    // spaces out gateway requests; unset means no limit
    throttle: OnceLock<Throttle>,
}

impl IndexQueue {
    pub fn new() -> Self {
        IndexQueue {
            map: DashMap::new(),
            denylist: RwLock::new(Denylist::empty()),
            local_denylist: RwLock::new(Denylist::empty()),
            last_fetch: Mutex::new(None),
            store: OnceLock::new(),
            writes: RwLock::new(()),
            throttle: OnceLock::new(),
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
        self.map.insert(item.to_string(), without_keywords(result));
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

    /**
     * Replace the denylist and drop anything already indexed that is now on it. Search only
     * returns cids still in the map, so purged pages disappear from results.
     */
    pub fn set_denylist(&self, denylist: Denylist) {
        let _writes = self.writes.write().unwrap();
        self.purge(&denylist);
        *self.denylist.write().unwrap() = denylist;
    }

    /// Same as set_denylist, for the local takedown list
    /// returns how many pages it purged
    pub fn set_local_denylist(&self, denylist: Denylist) -> usize {
        let _writes = self.writes.write().unwrap();
        let removed = self.purge(&denylist);
        *self.local_denylist.write().unwrap() = denylist;
        removed
    }

    /**
     * Use `store` for this queue: drop rows that are denylisted or older than max_age_secs
     * (deleting them from disk), then load the rest into the map and keyword index. Call after
     * both denylists are loaded and before crawling or serving.
     */
    pub fn attach_store(
        &self,
        store: Store,
        max_age_secs: i64,
    ) -> rusqlite::Result<(usize, usize, usize)> {
        let cutoff = now_secs() - max_age_secs;
        let (mut to_delete, mut blocked, mut expired) = (Vec::new(), 0, 0);
        // canonical item -> result; rows stored before #218 can have paths like "wiki//../X",
        // re-keyed here (item and cid, keeping their postings) or dropped as duplicates
        let mut keep: HashMap<String, IndexResult> = HashMap::new();
        let mut rekey = Vec::new();
        for (item, result, indexed_at) in store.load_all()? {
            if self.blocked_quietly(&item) || self.blocked_quietly(&result.cid) {
                blocked += 1;
                to_delete.push(item);
            } else if indexed_at < cutoff {
                expired += 1;
                to_delete.push(item);
            } else {
                match normalize_item(&item) {
                    // climbs out of its CID: can't be served correctly
                    None => to_delete.push(item),
                    Some(canonical) if canonical == item => {
                        keep.insert(item, result);
                    }
                    Some(canonical) => rekey.push((item, canonical, result)),
                }
            }
        }
        // a row already under the canonical key wins over a re-keyed duplicate
        for (item, canonical, mut result) in rekey {
            result.cid = normalize_item(&result.cid).unwrap_or_else(|| canonical.clone());
            if !keep.contains_key(&canonical) && store.rekey(&item, &canonical, &result.cid)? {
                keep.insert(canonical, result);
            } else {
                to_delete.push(item);
            }
        }
        store.delete(&to_delete)?;
        for (item, result) in keep.iter() {
            self.map.insert(item.clone(), without_keywords(result));
        }
        // resume the crawl (#224): pages blocked or indexed since they were queued are dropped
        let (mut respelled, mut dropped) = (Vec::new(), Vec::new());
        for item in store.queue_load()? {
            match self.admit(item.clone()) {
                (_, Some(admitted)) if admitted == item => {}
                (_, admitted) => {
                    dropped.push(item);
                    respelled.extend(admitted); // re-spelled canonically
                }
            }
        }
        store.queue_remove(&dropped)?;
        store.queue_add(&respelled)?;
        info!(
            "Resumed {} queued items ({} dropped)",
            store.queue_len(),
            dropped.len()
        );
        if self.store.set(store).is_err() {
            panic!("attach_store must run before the store is first used");
        }
        Ok((keep.len(), blocked, expired))
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
     * they wait. A restart drops what's left: the saved queue skips indexed pages on load.
     */
    pub fn recrawl(&self) -> usize {
        let indexed: Vec<String> = self.map.iter().map(|e| e.key().clone()).collect();
        let items: Vec<String> = indexed
            .into_iter()
            .filter(|item| !self.blocked_quietly(item))
            .collect();
        self.push_queued(items)
    }

    /// Drop pages indexed more than max_age_secs ago, from the store and the map
    pub fn expire(&self, max_age_secs: i64) -> usize {
        let _writes = self.writes.write().unwrap();
        let Some(store) = self.store.get() else {
            return 0;
        };
        let old = match store.items_older_than(now_secs() - max_age_secs) {
            Ok(old) => old,
            Err(err) => {
                warn!("Couldn't check for expired pages: {}", err);
                return 0;
            }
        };
        if let Err(err) = store.delete(&old) {
            warn!("Couldn't delete expired pages: {}", err);
            return 0;
        }
        for item in &old {
            self.map.remove(item);
        }
        old.len()
    }

    /**
     * Add a crawled page to the index and the store. Re-checks the denylists first: a takedown
     * that landed while this page was being fetched must not be written back to disk.
     */
    pub fn persist(&self, item: &str, result: &IndexResult) {
        let _writes = self.writes.read().unwrap();
        if self.blocked_quietly(item) || self.blocked_quietly(&result.cid) {
            return;
        }
        self.map.insert(item.to_string(), without_keywords(result));
        if let Err(err) = self.store().upsert(item, result, now_secs()) {
            warn!("Couldn't persist {}: {}", item, err);
        }
    }

    fn purge(&self, denylist: &Denylist) -> usize {
        let mut removed = Vec::new();
        self.map.retain(|item, result| {
            let keep = !denylist.is_blocked(item) && !denylist.is_blocked(&result.cid);
            if !keep {
                removed.push(item.clone());
            }
            keep
        });
        if let Some(store) = self.store.get() {
            if let Err(err) = store.delete(&removed) {
                warn!(
                    "Couldn't delete {} purged pages from the store: {}",
                    removed.len(),
                    err
                );
            }
        }
        removed.len()
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
        if self.map.contains_key(&item) {
            trace!("Already indexed {}", item);
            return (Enqueued::Queued, None);
        }
        (Enqueued::Queued, Some(item))
    }

    pub fn queue_length(&self) -> usize {
        self.store().queue_len()
    }

    pub fn index_length(&self) -> usize {
        self.map.len()
    }

    /// Indexed and queued counts per root CID (the first segment of "cid/path"), with the root
    /// page's title if it's indexed, most indexed first (#232)
    pub fn root_counts(&self) -> Vec<(String, usize, usize, String)> {
        let root = |item: &str| item.split('/').next().unwrap_or(item).to_string();
        let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
        for entry in self.map.iter() {
            counts.entry(root(entry.key())).or_default().0 += 1;
        }
        match self.store().queue_roots() {
            Ok(queued) => {
                for (cid, n) in queued {
                    counts.entry(cid).or_default().1 += n;
                }
            }
            Err(err) => warn!("Couldn't count the queue by root: {}", err),
        }
        let mut roots: Vec<_> = counts
            .into_iter()
            .map(|(cid, (indexed, queued))| {
                let title = self
                    .map
                    .get(&cid)
                    .map(|r| r.title.clone())
                    .unwrap_or_default();
                (cid, indexed, queued, title)
            })
            .collect();
        roots.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        roots
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
     * Pages matching the query (see parse_query: every word, or any of the words joined by OR),
     * best first, as (total matches, the page of them from offset, at most limit long). Scored by
     * TF-IDF, summed over the query terms on the page: each counts
     * (1 + ln count on the page) * ln(1 + pages / pages with the term).
     */
    pub fn search(&self, query: &str, offset: usize, limit: usize) -> (usize, Vec<IndexResult>) {
        let groups = parse_query(query);
        let pages = self.map.len().max(1) as f64;
        // each distinct term's postings, read once
        let mut postings: HashMap<&str, HashMap<String, u32>> = HashMap::new();
        for group in &groups {
            for term in group {
                if postings.contains_key(term.as_str()) {
                    continue;
                }
                match self.store().postings(term) {
                    Ok(found) => postings.insert(term, found.into_iter().collect()),
                    Err(err) => {
                        warn!("Couldn't search for {:?}: {}", term, err);
                        return (0, Vec::new());
                    }
                };
            }
            // a group none of whose terms are indexed matches nothing; skip reading the rest
            if group.iter().all(|term| postings[term.as_str()].is_empty()) {
                return (0, Vec::new());
            }
        }
        let weight = |term: &str, count: u32| {
            let idf = (1.0 + pages / postings[term].len().max(1) as f64).ln();
            (1.0 + (count.max(1) as f64).ln()) * idf
        };
        let has = |group: &Vec<String>, page: &String| {
            group
                .iter()
                .any(|term| postings[term.as_str()].contains_key(page))
        };
        // candidates from the group with the fewest postings, kept if they match every group
        let size = |group: &&Vec<String>| -> usize {
            group.iter().map(|term| postings[term.as_str()].len()).sum()
        };
        let mut matches: HashSet<&String> = match groups.iter().min_by_key(size) {
            Some(smallest) => smallest
                .iter()
                .flat_map(|term| postings[term.as_str()].keys())
                .collect(),
            None => HashSet::new(),
        };
        matches.retain(|page| groups.iter().all(|group| has(group, page)));
        let scores = matches.into_iter().map(|page| {
            let score: f64 = postings
                .iter()
                .filter_map(|(term, found)| found.get(page).map(|count| weight(term, *count)))
                .sum();
            (page.clone(), score)
        });
        // only what's in the map is served (a page whose store write failed isn't in postings)
        let mut ranked: Vec<(f64, String)> = scores
            .filter(|(page, _)| self.map.contains_key(page))
            .map(|(page, score)| (score, page))
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = ranked.len();
        // clone only the requested page of results, not every match
        let page = ranked.into_iter().skip(offset).take(limit);
        let results = page.filter_map(|(_, item)| self.map.get(&item).map(|r| r.clone()));
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
                Some(result) => self.persist(&item, &result),
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
    fn retrieve_content(&self, gateway: String, cid: String) -> Option<IndexResult> {
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
     * Process the content of the page, extract keywords, enqueue more cids, return the IndexResult
     */
    fn process_content(
        &self,
        base: &reqwest::Url,
        cid: String,
        document: Html,
    ) -> Option<IndexResult> {
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
        self.enqueue_many(links);
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
            if end.is_some() {
                let endunwrap = end.unwrap();
                let excerpt = content[..endunwrap].to_string();
                return Some(IndexResult::new(fullcid, title, excerpt, index_keywords));
            } else {
                return Some(IndexResult::new(
                    fullcid,
                    title,
                    "".to_string(),
                    index_keywords,
                ));
            }
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
        let queue = IndexQueue::new();
        for item in ["wiki", "wiki/wiki/A", "wiki/wiki/B", "other/x"] {
            queue.map.insert(item.to_string(), page(item, item, &[]));
        }
        queue.push_queued(vec!["wiki/wiki/C".to_string(), "lone/y".to_string()]);
        assert_eq!(
            queue.root_counts(),
            vec![
                ("wiki".to_string(), 3, 1, "wiki".to_string()),
                ("other".to_string(), 1, 0, String::new()),
                ("lone".to_string(), 0, 1, String::new()),
            ]
        );
    }

    #[test]
    fn set_denylist_purges_indexed_and_blocks_fetches() {
        let bad = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let good = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        let queue = IndexQueue::new();
        for cid in [bad, good] {
            let item = format!("{}/page", cid);
            let result = IndexResult::new(item.clone(), "".into(), "".into(), HashMap::new());
            queue.map.insert(item, result);
        }

        let hash: String = sha2::Sha256::digest(format!("{}/", bad).as_bytes())
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        queue.set_denylist(Denylist::parse(&format!("//{}\n", hash)));

        assert_eq!(queue.index_length(), 1);
        assert!(queue.map.contains_key(&format!("{}/page", good)));
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
            queue.map.insert(item, result);
        }

        queue.set_denylist(Denylist::parse(&format!("{}/badbits-page\n", good)));
        queue.set_local_denylist(Denylist::parse(&format!("{}/page\n", bad)));

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

    #[test]
    fn attach_reloads_pages_and_keywords() {
        let dir = temp_dir("attach");
        seed(&dir, &[(GOOD, "goodtitle", now_secs())]);
        let queue = IndexQueue::new();
        let (loaded, blocked, expired) = queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert_eq!((loaded, blocked, expired), (1, 0, 0));
        assert_eq!(queue.search("goodtitle", 0, usize::MAX).1.len(), 1);
        assert_eq!(queue.search("shared", 0, usize::MAX).1.len(), 1);
    }

    #[test]
    fn attach_drops_denylisted_and_expired_rows_from_disk() {
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
        let (loaded, blocked, expired) = queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert_eq!((loaded, blocked, expired), (1, 1, 1));
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
        assert_eq!(queue.index_length(), 1);
        assert!(!file_has(&dir, "purgetitle"), "takedown reaches the disk");
        assert!(file_has(&dir, "stays"));
    }

    #[test]
    fn expire_removes_old_pages_from_memory_and_disk() {
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
    fn attach_rekeys_non_canonical_rows_and_drops_duplicates() {
        let dir = temp_dir("rekey");
        let store = Store::open(&dir, 0).unwrap();
        for (item, title) in [
            (format!("{}/wiki//../Tech", GOOD), "techtitle"),
            (format!("{}/wiki/Tech", GOOD), "techdup"),
            (format!("{}/wiki//Llull", GOOD), "llulltitle"),
            (format!("{}/..", GOOD), "escapetitle"),
        ] {
            store
                .upsert(&item, &page(&item, title, &[(title, 1)]), now_secs())
                .unwrap();
        }
        drop(store);

        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        let mut keys: Vec<String> = queue.map.iter().map(|e| e.key().clone()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                format!("{}/wiki/Llull", GOOD),
                format!("{}/wiki/Tech", GOOD)
            ]
        );
        // the result's cid is re-keyed too, so search (which looks pages up by it) still finds them
        assert_eq!(queue.search("llulltitle", 0, usize::MAX).1.len(), 1);
        assert_eq!(
            queue.search("techtitle", 0, usize::MAX).1.len()
                + queue.search("techdup", 0, usize::MAX).1.len(),
            1
        );

        let rows: Vec<String> = Store::open(&dir, 0)
            .unwrap()
            .load_all()
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(rows.len(), 2, "disk matches memory: {:?}", rows);
        assert!(!file_has(&dir, "escapetitle"));
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
        for (item, words) in pages {
            let counts: HashMap<String, u32> = words.iter().map(|w| (w.to_string(), 5)).collect();
            queue.index_for_test(
                item,
                &IndexResult::new(item.to_string(), "t".into(), "e".into(), counts),
            );
        }
        assert_eq!(queue.keyword_length(), 3);
        assert_eq!(
            queue.top_keywords(2),
            vec![("common".to_string(), 3), ("middle".to_string(), 2)]
        );
        let mut found: Vec<String> = queue
            .search("common", 0, usize::MAX)
            .1
            .into_iter()
            .map(|r| r.cid)
            .collect();
        found.sort();
        assert_eq!(found, vec!["bafyP1", "bafyP2", "bafyP3"]);
        // re-indexing a page replaces its postings: no double count, and terms it lost go
        queue.index_for_test("bafyP1", &page("bafyP1", "t", &[("common", 1)]));
        assert_eq!(queue.top_keywords(1), vec![("common".to_string(), 3)]);
        assert!(queue.search("rare", 0, usize::MAX).1.is_empty());
        assert_eq!(queue.keyword_length(), 2);
        assert!(queue.search("absent", 0, usize::MAX).1.is_empty());
    }

    #[test]
    fn keyword_counts_live_on_disk_not_in_the_in_memory_records() {
        // #220: per-page counts duplicated the inverted index in RAM and bloated /search output
        let dir = temp_dir("counts");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.persist(GOOD, &page(GOOD, "t", &[("alpha", 3)]));
        assert!(
            queue.map.get(GOOD).unwrap().keywords.is_empty(),
            "in-memory record carries no counts"
        );
        assert_eq!(
            queue.search("alpha", 0, usize::MAX).1.len(),
            1,
            "still searchable"
        );
        assert_eq!(
            Store::open(&dir, 0).unwrap().postings("alpha").unwrap(),
            vec![(GOOD.to_string(), 3)],
            "counts kept on disk"
        );

        // and after a reload: searchable, counts dropped from memory again
        let reloaded = IndexQueue::new();
        reloaded
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        assert_eq!(reloaded.search("alpha", 0, usize::MAX).1.len(), 1);
        assert!(reloaded.map.get(GOOD).unwrap().keywords.is_empty());
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
        assert_eq!(
            second.queue_length(),
            1,
            "only the still-pending, allowed item resumes"
        );
        assert_eq!(
            Store::open(&dir, 0).unwrap().queue_load().unwrap(),
            vec![format!("{}/wiki/B", GOOD)]
        );
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
        assert!(Store::open(&dir, 0)
            .unwrap()
            .queue_load()
            .unwrap()
            .is_empty());
        assert!(!queue.process_next(&gateway), "nothing left");
    }

    #[test]
    fn store_write_failure_does_not_panic() {
        let dir = temp_dir("fail");
        let queue = IndexQueue::new();
        queue
            .attach_store(Store::open(&dir, 0).unwrap(), 30 * DAY)
            .unwrap();
        queue.store.get().unwrap().break_for_test(); // e.g. disk full / read-only remount
        queue.persist(GOOD, &page(GOOD, "t", &[])); // upsert fails: logs, doesn't panic
        assert_eq!(queue.index_length(), 1, "still indexed in memory");
        assert_eq!(queue.expire(0), 0, "expire tolerates the broken store too");
        queue.set_denylist(Denylist::parse(&format!("{}\n", GOOD))); // purge's delete fails: logs only
        assert_eq!(queue.index_length(), 0);
    }

    // index a page's text the way process_content does
    fn index_text(queue: &IndexQueue, item: &str, text: &str) {
        let mut counts = HashMap::new();
        for word in normalize(text) {
            *counts.entry(word).or_insert(0) += 1;
        }
        queue.index_for_test(
            item,
            &IndexResult::new(item.into(), "t".into(), "e".into(), counts),
        );
    }

    fn found(queue: &IndexQueue, query: &str) -> Vec<String> {
        queue
            .search(query, 0, usize::MAX)
            .1
            .into_iter()
            .map(|r| r.cid)
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
    fn search_ranks_by_tf_idf_and_pages_with_offset_and_limit() {
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
            page.into_iter().map(|r| r.cid).collect::<Vec<_>>(),
            vec!["bafyOnce"]
        );
        let (total, page) = queue.search("rust", 5, 10);
        assert_eq!((total, page.len()), (3, 0));
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
        let store = queue.store.get().unwrap();
        assert!(store.postings("takedownword").unwrap().is_empty());
        assert_eq!(
            store.postings("shared").unwrap(),
            vec![(BAD.to_string(), 1)]
        );
        assert_eq!(queue.keyword_length(), 1);
        assert!(
            !file_has(&dir, "takedownword"),
            "secure_delete covers postings too"
        );
        assert_eq!(queue.expire(-DAY), 1);
        assert_eq!(queue.keyword_length(), 0);
    }
}
