/*!
 * Discovery (#213): entries from the seeds file, ENS and submissions live in the store's
 * `discovered` table. The resolver turns each into a root (a name through the gateway's /ipns/),
 * queues it through enqueue() and checks it again daily. New roots are capped per hour.
 */
use crate::ens::{self, Rpc, RpcError, Scan};
use crate::entry::{self, Contenthash, Entry};
use crate::index_queue::{Enqueued, IndexQueue};
use crate::store::{now_secs, Discovered};
use log::{debug, info, warn};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;
// the ENS scan's next block, in discovery_state
const ENS_NEXT_BLOCK: &str = "ens_next_block";

/// At most `per_hour` new roots an hour, across every source (0: no limit)
pub struct HourlyCap {
    per_hour: u32,
    // the hour and how many it has used
    used: Mutex<(i64, u32)>,
}

impl HourlyCap {
    pub fn new(per_hour: u32) -> HourlyCap {
        HourlyCap {
            per_hour,
            used: Mutex::new((-1, 0)),
        }
    }

    /// Takes one from this hour's allowance; false if it's used up
    pub fn take(&self, now: i64) -> bool {
        if self.per_hour == 0 {
            return true;
        }
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if used.0 != now / HOUR {
            *used = (now / HOUR, 0);
        }
        if used.1 >= self.per_hour {
            return false;
        }
        used.1 += 1;
        true
    }
}

// the start of the next hour, when a capped entry is tried again
fn next_hour(now: i64) -> i64 {
    (now / HOUR + 1) * HOUR
}

// how long after a failure an entry is tried again: an hour, doubling, at most a day
fn retry_after(failures: u32) -> i64 {
    (HOUR << failures.min(5)).min(DAY)
}

/// The root CID the gateway resolves a name to: GET /ipns/<name>/ and the first CID in its
/// X-Ipfs-Roots header. The body isn't read.
pub fn resolve(gateway: &str, name: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    // a dot segment would be taken as a step up, whatever its encoding
    if name.is_empty() || name.chars().all(|c| c == '.') {
        return Err(format!("not a name: {:.100}", name));
    }
    // the name is one percent-encoded path segment, never a "..", a "\\" or a query
    let mut url =
        reqwest::Url::parse(&format!("http://{}/", gateway)).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|_| "bad gateway address".to_string())?
        .clear()
        .push("ipns")
        .push(name)
        .push("");
    let response = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let roots = response
        .headers()
        .get("x-ipfs-roots")
        .and_then(|v| v.to_str().ok())
        .ok_or("no X-Ipfs-Roots header")?;
    let root = roots.split(',').next().unwrap_or("").trim();
    match entry::parse(root) {
        Some(Entry::Cid(cid)) if !cid.contains('/') => Ok(cid),
        _ => Err(format!("not a CID in X-Ipfs-Roots: {:.100}", roots)),
    }
}

/// What checking a due entry came to
#[derive(Debug, PartialEq)]
pub enum Checked {
    /// a new root, queued
    Queued(String),
    /// the root it had, queued again (a no-op unless its pages expired)
    Unchanged(String),
    /// a new root over the hourly cap: tried again next hour
    Capped,
    Failed(String),
}

/**
 * Checks one due entry: a CID is its own root, a name is resolved through the gateway. A new
 * root is queued if the hourly cap allows; otherwise the entry is put off to the next hour,
 * uncounted. An unchanged root is queued again, uncapped, which re-crawls a site once its pages
 * expire. Then the entry is due again in a day, or after a failure in an hour, doubling up to a
 * day.
 */
pub fn check(
    queue: &IndexQueue,
    gateway: &str,
    cap: &HourlyCap,
    row: &Discovered,
    now: i64,
) -> Checked {
    let store = queue.store();
    let root = match entry::parse(&row.entry) {
        Some(Entry::Cid(cid)) => Ok(cid),
        Some(Entry::Name(name)) => {
            queue.wait_for_gateway();
            resolve(gateway, &name)
        }
        None => Err("not a CID or a name".to_string()),
    };
    let outcome = match root {
        Err(err) => Checked::Failed(err),
        Ok(root) => {
            let new = row.root.as_deref() != Some(root.as_str());
            if new && !cap.take(now) {
                if let Err(err) = store.discovered_postpone(&row.entry, next_hour(now)) {
                    warn!("Couldn't put off {}: {}", row.entry, err);
                    store.note_error(&err);
                }
                return Checked::Capped;
            }
            match queue.enqueue(root.clone()) {
                Enqueued::Queued if new => Checked::Queued(root),
                Enqueued::Queued => Checked::Unchanged(root),
                Enqueued::Blocked => Checked::Failed(format!("{} is blocked", root)),
                Enqueued::InvalidCid => Checked::Failed(format!("{} is not a valid CID", root)),
            }
        }
    };
    let written = match &outcome {
        Checked::Queued(root) | Checked::Unchanged(root) => {
            store.discovered_ok(&row.entry, root, now, now + DAY)
        }
        Checked::Failed(err) => {
            warn!("Discovery: couldn't check {}: {}", row.entry, err);
            store.discovered_failed(&row.entry, err, now, now + retry_after(row.failures))
        }
        Checked::Capped => Ok(()),
    };
    if let Err(err) = written {
        warn!("Couldn't save the check of {}: {}", row.entry, err);
        store.note_error(&err);
    }
    outcome
}

/// Checks the next due entry, unless the crawl is paused (#284) or none is due. Also says whether
/// the entry was a name, which went to the gateway.
pub fn resolve_next(queue: &IndexQueue, gateway: &str, cap: &HourlyCap) -> Option<(Checked, bool)> {
    if queue.crawl_paused().is_some() {
        return None;
    }
    let now = now_secs();
    let row = match queue.store().discovered_due(now, 1) {
        Ok(mut due) => due.pop()?,
        Err(err) => {
            warn!("Couldn't read due entries: {}", err);
            queue.store().note_error(&err);
            return None;
        }
    };
    let checked = check(queue, gateway, cap, &row, now);
    debug!("Discovery: {} {:?}", row.entry, checked);
    Some((
        checked,
        matches!(entry::parse(&row.entry), Some(Entry::Name(_))),
    ))
}

// how long the resolver waits after a check: a name, which went to the gateway, is followed by
// 1 s; a CID entry by nothing; with none due (None), or the crawl paused, it looks again in 10 s
fn pause_after(used_gateway: Option<bool>) -> Duration {
    match used_gateway {
        Some(true) => Duration::from_secs(1),
        Some(false) => Duration::ZERO,
        None => Duration::from_secs(10),
    }
}

/// The resolver thread: at most one name a second
pub fn run_resolver(queue: &IndexQueue, gateway: &str, cap: &HourlyCap) {
    loop {
        let used_gateway = resolve_next(queue, gateway, cap).map(|(_, name)| name);
        std::thread::sleep(pause_after(used_gateway));
    }
}

/// A seeds file's entries: one a line, "#" comments and blank lines skipped. A line that isn't
/// an entry is skipped with a warning.
pub fn parse_seeds(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                return None;
            }
            let parsed = entry::parse(line);
            if parsed.is_none() {
                warn!("Seeds file: skipping {:?}: not a CID or a name", line);
            }
            parsed
        })
        .collect()
}

/// Records the seeds file's entries, new ones due now; known ones stay as they are. Returns how
/// many were new.
pub fn add_seeds(queue: &IndexQueue, entries: &[Entry], now: i64) -> usize {
    entries
        .iter()
        .filter(|e| match queue.store().discover(e.as_str(), "seeds", now) {
            Ok(new) => new,
            Err(err) => {
                warn!("Couldn't record seed {}: {}", e.as_str(), err);
                queue.store().note_error(&err);
                false
            }
        })
        .count()
}

/// The seeds thread: reads the file at start and every hour. An unreadable file is logged; the
/// entries read before are in the store and stay.
pub fn run_seeds(queue: &IndexQueue, path: &Path) {
    loop {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let entries = parse_seeds(&text);
                let new = add_seeds(queue, &entries, now_secs());
                if new > 0 {
                    info!("Seeds file: {} new of {} entries", new, entries.len());
                }
            }
            Err(err) => warn!(
                "Couldn't read seeds file {}: {}; keeping the entries read before",
                path.display(),
                err
            ),
        }
        std::thread::sleep(Duration::from_secs(HOUR as u64));
    }
}

/// Stores ENS events: a node's IPFS or IPNS record becomes its entry (due now), a cleared one
/// drops it, anything else (Swarm...) is skipped. Returns how many were skipped.
pub fn apply_ens(queue: &IndexQueue, events: &[ens::Event], now: i64) -> usize {
    let mut skipped = 0;
    for event in events {
        let entry = match &event.record {
            Contenthash::Set(entry) => Some(entry.as_str()),
            Contenthash::Cleared => None,
            Contenthash::Other => {
                skipped += 1;
                continue;
            }
        };
        if let Err(err) = queue.store().discover_ens(&event.node, entry, now) {
            warn!("Couldn't record ENS node {}: {}", event.node, err);
            queue.store().note_error(&err);
        }
    }
    skipped
}

/// Where the ENS scan resumes: the saved block, or START_BLOCK
pub fn ens_start(queue: &IndexQueue) -> Scan {
    let saved = queue.store().state(ENS_NEXT_BLOCK).ok().flatten();
    Scan::starting(
        saved
            .and_then(|v| v.parse().ok())
            .unwrap_or(ens::START_BLOCK),
    )
}

/**
 * One pass of the ENS scan, from where it is to the latest block, with `pause` between requests.
 * The position is saved after each one. A JSON-RPC error halves the range and the pass carries
 * on; at one block, or on a transport error, it stops with the error.
 */
pub fn ens_pass(
    queue: &IndexQueue,
    rpc: &dyn Rpc,
    scan: &mut Scan,
    pause: Duration,
) -> Result<(), RpcError> {
    let latest = ens::latest_block(rpc)?;
    while scan.next <= latest {
        let range = scan.range;
        match ens::step(rpc, scan, latest) {
            Ok(events) => {
                let skipped = apply_ens(queue, &events, now_secs());
                debug!(
                    "ENS scan: {} records, {} skipped, next block {}",
                    events.len(),
                    skipped,
                    scan.next
                );
                if let Err(err) = queue
                    .store()
                    .set_state(ENS_NEXT_BLOCK, &scan.next.to_string())
                {
                    warn!("Couldn't save the ENS scan's position: {}", err);
                    queue.store().note_error(&err);
                }
            }
            Err(RpcError::Rpc(msg)) if range > 1 => {
                debug!("ENS scan: {}; asking for {} blocks", msg, scan.range);
            }
            Err(err) => return Err(err),
        }
        std::thread::sleep(pause);
    }
    Ok(())
}

/// The ENS thread: a pass every 10 minutes; after an error, 1 minute, doubling up to an hour
pub fn run_ens(queue: &IndexQueue, rpc: &dyn Rpc) {
    let mut scan = ens_start(queue);
    let mut backoff = Duration::from_secs(60);
    loop {
        match ens_pass(queue, rpc, &mut scan, Duration::from_secs(1)) {
            Ok(()) => {
                backoff = Duration::from_secs(60);
                std::thread::sleep(Duration::from_secs(600));
            }
            Err(err) => {
                warn!(
                    "ENS scan: {} at block {}; trying again in {:?}",
                    err, scan.next, backoff
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(3600));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::Breaker;
    use crate::denylist::Denylist;
    use crate::ens::fake::{log, FakeRpc};
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    const A: &str = "bafybeier6ud42ptljtfaxrdlknktw54b7ohfpg5h33mosxpxjndxs7xzvm";
    const B: &str = "bafybeiesmy6rsaglneqxbzkbdj5degzp2izydfadhl2vu3oyrp7k52fypu";
    const NOW: i64 = 1_800_000_000;

    // a gateway answering /ipns/<name>/ with X-Ipfs-Roots: <root>, or 504 if root is None, and
    // /ipfs/<path> with a page naming the path; returns its host:port
    fn gateway(root: Option<&'static str>) -> String {
        gateway_recording(root, Arc::new(Mutex::new(vec![])))
    }

    // the same, noting the path of every request
    fn gateway_recording(root: Option<&'static str>, paths: Arc<Mutex<Vec<String>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                paths.lock().unwrap().push(path.clone());
                let reply = match (path.starts_with("/ipns/"), root) {
                    (true, Some(root)) => format!(
                        "HTTP/1.1 200 OK\r\nX-Ipfs-Roots: {},bafkreiextra\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        root
                    ),
                    (true, None) => "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    (false, _) => {
                        let body = format!("<title>{0}</title><body>words for {0}</body>", path);
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        host
    }

    fn row(queue: &IndexQueue, entry: &str) -> Discovered {
        queue
            .discovered()
            .into_iter()
            .find(|d| d.entry == entry)
            .unwrap()
    }

    #[test]
    fn the_hourly_cap_counts_per_hour_and_zero_means_none() {
        let cap = HourlyCap::new(2);
        assert!(cap.take(NOW));
        assert!(cap.take(NOW + 1));
        assert!(!cap.take(NOW + 2));
        assert!(cap.take(next_hour(NOW)), "a new hour");
        let none = HourlyCap::new(0);
        assert!((0..1000).all(|i| none.take(NOW + i)));
    }

    #[test]
    fn failures_wait_an_hour_doubling_up_to_a_day() {
        let waits: Vec<i64> = [0, 1, 4, 5, 30].iter().map(|f| retry_after(*f)).collect();
        assert_eq!(waits, vec![HOUR, 2 * HOUR, 16 * HOUR, DAY, DAY]);
    }

    #[test]
    fn a_name_resolves_and_its_root_is_queued() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let checked = check(
            &queue,
            &gateway(Some(A)),
            &HourlyCap::new(10),
            &row(&queue, "docs.ipfs.tech"),
            NOW,
        );
        assert_eq!(checked, Checked::Queued(A.into()));
        assert_eq!(queue.queue_length(), 1);
        let row = row(&queue, "docs.ipfs.tech");
        assert_eq!(
            (row.root.as_deref(), row.checked_at, row.next_at),
            (Some(A), Some(NOW), NOW + DAY)
        );
    }

    #[test]
    fn a_changed_root_is_queued_and_an_unchanged_one_again() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(10);
        check(
            &queue,
            &gateway(Some(A)),
            &cap,
            &row(&queue, "docs.ipfs.tech"),
            NOW,
        );
        assert_eq!(
            check(
                &queue,
                &gateway(Some(A)),
                &cap,
                &row(&queue, "docs.ipfs.tech"),
                NOW + DAY
            ),
            Checked::Unchanged(A.into())
        );
        assert_eq!(
            check(
                &queue,
                &gateway(Some(B)),
                &cap,
                &row(&queue, "docs.ipfs.tech"),
                NOW + 2 * DAY
            ),
            Checked::Queued(B.into())
        );
        assert_eq!(row(&queue, "docs.ipfs.tech").root.as_deref(), Some(B));
    }

    #[test]
    fn an_unchanged_root_is_queued_again_even_when_capped() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(1);
        check(
            &queue,
            &gateway(Some(A)),
            &cap,
            &row(&queue, "docs.ipfs.tech"),
            NOW,
        );
        assert_eq!(
            check(
                &queue,
                &gateway(Some(A)),
                &cap,
                &row(&queue, "docs.ipfs.tech"),
                NOW + 10
            ),
            Checked::Unchanged(A.into())
        );
    }

    #[test]
    fn a_new_root_over_the_cap_waits_for_the_next_hour() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(1);
        assert!(cap.take(NOW));
        assert_eq!(
            check(
                &queue,
                &gateway(Some(A)),
                &cap,
                &row(&queue, "docs.ipfs.tech"),
                NOW
            ),
            Checked::Capped
        );
        let row = row(&queue, "docs.ipfs.tech");
        assert_eq!(
            (row.next_at, row.checked_at, row.failures),
            (next_hour(NOW), None, 0)
        );
        assert_eq!(queue.queue_length(), 0);
    }

    #[test]
    fn a_failure_is_stored_and_backs_off() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(10);
        let gw = gateway(None);
        assert!(matches!(
            check(&queue, &gw, &cap, &row(&queue, "docs.ipfs.tech"), NOW),
            Checked::Failed(_)
        ));
        let first = row(&queue, "docs.ipfs.tech");
        assert_eq!((first.failures, first.next_at), (1, NOW + HOUR));
        assert!(first.error.unwrap().contains("504"));
        check(
            &queue,
            &gw,
            &cap,
            &row(&queue, "docs.ipfs.tech"),
            NOW + HOUR,
        );
        assert_eq!(row(&queue, "docs.ipfs.tech").next_at, NOW + HOUR + 2 * HOUR);
    }

    #[test]
    fn a_cid_entry_is_its_own_root_and_a_blocked_root_is_refused() {
        let queue = IndexQueue::new();
        queue.discover(A, "seeds").unwrap();
        let cap = HourlyCap::new(10);
        // no gateway needed
        assert_eq!(
            check(&queue, "127.0.0.1:1", &cap, &row(&queue, A), NOW),
            Checked::Queued(A.into())
        );
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", B)));
        queue.discover("blocked.example", "submitted").unwrap();
        match check(
            &queue,
            &gateway(Some(B)),
            &cap,
            &row(&queue, "blocked.example"),
            NOW,
        ) {
            Checked::Failed(err) => assert!(err.contains("blocked"), "{}", err),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn the_resolver_waits_while_the_crawl_is_paused() {
        let queue = IndexQueue::new();
        queue.set_breaker_for_test(Breaker::with(
            Duration::from_secs(600),
            Duration::from_secs(900),
        ));
        for _ in 0..16 {
            queue.breaker().record(Some("504"));
        }
        queue.discover(A, "seeds").unwrap();
        assert_eq!(
            resolve_next(&queue, "127.0.0.1:1", &HourlyCap::new(10)),
            None
        );
        assert_eq!(row(&queue, A).checked_at, None, "still due");
    }

    #[test]
    fn a_name_is_one_path_segment_under_ipns() {
        for name in [
            "..",
            "%2e%2e",
            ".",
            "..\\ipfs\\bafyanycid",
            "x/../../ipfs/bafyanycid",
            "a?b=/ipfs/c",
            "a#b",
        ] {
            let paths = Arc::new(Mutex::new(vec![]));
            let gw = gateway_recording(Some(A), paths.clone());
            let result = resolve(&gw, name);
            let seen = paths.lock().unwrap().clone();
            if name.chars().all(|c| c == '.') {
                assert!(result.is_err() && seen.is_empty(), "{:?} {:?}", name, seen);
                continue;
            }
            assert_eq!(seen.len(), 1, "{:?}", name);
            assert!(seen[0].starts_with("/ipns/"), "{:?} -> {}", name, seen[0]);
            assert_eq!(seen[0].matches('/').count(), 3, "{:?} -> {}", name, seen[0]);
        }
    }

    #[test]
    fn only_a_name_costs_the_resolver_its_pause() {
        let queue = IndexQueue::new();
        queue.discover(A, "seeds").unwrap();
        let cap = HourlyCap::new(10);
        let (checked, used_gateway) = resolve_next(&queue, "127.0.0.1:1", &cap).unwrap();
        assert_eq!(checked, Checked::Queued(A.into()));
        assert!(!used_gateway);
        assert_eq!(pause_after(Some(used_gateway)), Duration::ZERO);
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let (_, used_gateway) = resolve_next(&queue, &gateway(Some(B)), &cap).unwrap();
        assert!(used_gateway);
        assert_eq!(pause_after(Some(used_gateway)), Duration::from_secs(1));
        assert_eq!(pause_after(None), Duration::from_secs(10));
    }

    #[test]
    fn seeds_lines_skip_comments_blanks_and_junk() {
        let text = format!(
            "# IPFS project sites\n\ndocs.ipfs.tech  # the docs\n/ipfs/{}\nnot a seed\nVitalik.eth\n",
            A
        );
        assert_eq!(
            parse_seeds(&text),
            vec![
                Entry::Name("docs.ipfs.tech".into()),
                Entry::Cid(A.into()),
                Entry::Name("vitalik.eth".into()),
            ]
        );
    }

    #[test]
    fn seeds_are_added_once_and_stay_when_the_file_goes() {
        let queue = IndexQueue::new();
        let entries = parse_seeds("docs.ipfs.tech\nvitalik.eth\n");
        assert_eq!(add_seeds(&queue, &entries, NOW), 2);
        assert_eq!(add_seeds(&queue, &entries, NOW + HOUR), 0);
        assert_eq!(row(&queue, "vitalik.eth").source, "seeds");
        // an unreadable file adds nothing, and the entries stay
        assert_eq!(add_seeds(&queue, &[], NOW + 2 * HOUR), 0);
        assert_eq!(queue.discovered_count(), 2);
    }

    #[test]
    fn ens_events_keep_the_latest_record_per_node() {
        use crate::ens::Event;
        let queue = IndexQueue::new();
        let set = |node: &str, entry: Entry| Event {
            node: node.into(),
            record: Contenthash::Set(entry),
        };
        let skipped = apply_ens(
            &queue,
            &[
                set("0x01", Entry::Cid(A.into())),
                set("0x02", Entry::Name("app.uniswap.org".into())),
                Event {
                    node: "0x03".into(),
                    record: Contenthash::Other,
                },
                Event {
                    node: "0x01".into(),
                    record: Contenthash::Cleared,
                },
            ],
            NOW,
        );
        assert_eq!(skipped, 1);
        let entries: Vec<(String, String)> = queue
            .discovered()
            .into_iter()
            .map(|d| (d.entry, d.source))
            .collect();
        assert_eq!(
            entries,
            vec![("app.uniswap.org".to_string(), "ens".to_string())]
        );
    }

    #[test]
    fn an_ens_pass_halves_and_carries_on_and_saves_its_position() {
        let queue = IndexQueue::new();
        let start = crate::ens::START_BLOCK;
        let ipfs = [vec![0xe3, 0x01], cid::Cid::try_from(A).unwrap().to_bytes()].concat();
        let rpc = FakeRpc::new(vec![
            Ok(json!(format!("{:#x}", start + 15_000))),
            Err(RpcError::Rpc("too many blocks".into())),
            Ok(json!([log("0x01", &ipfs)])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let mut scan = ens_start(&queue);
        assert_eq!(scan.next, start);
        ens_pass(&queue, &rpc, &mut scan, Duration::ZERO).unwrap();
        assert_eq!(scan.next, start + 15_001);
        assert_eq!(rpc.calls().len(), 5);
        assert_eq!(row(&queue, A).source, "ens");
        assert_eq!(ens_start(&queue).next, start + 15_001, "saved");
    }

    #[test]
    fn a_failing_ens_pass_keeps_its_position() {
        let queue = IndexQueue::new();
        let start = crate::ens::START_BLOCK;
        let rpc = FakeRpc::new(vec![
            Ok(json!(format!("{:#x}", start + 50_000))),
            Ok(json!([])),
            Err(RpcError::Transport("timed out".into())),
        ]);
        let mut scan = ens_start(&queue);
        assert_eq!(
            ens_pass(&queue, &rpc, &mut scan, Duration::ZERO),
            Err(RpcError::Transport("timed out".into()))
        );
        assert_eq!(ens_start(&queue).next, start + 10_000);
        // a provider refusing even one block: the pass gives up instead of spinning
        let rpc = FakeRpc::new(vec![
            Ok(json!(format!("{:#x}", start + 50_000))),
            Err(RpcError::Rpc("no".into())),
        ]);
        let mut scan = Scan {
            next: start + 10_000,
            range: 1,
        };
        assert!(ens_pass(&queue, &rpc, &mut scan, Duration::ZERO).is_err());
        assert_eq!(rpc.calls().len(), 2);
    }

    #[test]
    fn a_seeded_site_is_resolved_queued_and_indexed() {
        let queue = IndexQueue::new();
        add_seeds(&queue, &parse_seeds("site.example\n"), now_secs());
        let gw = gateway(Some(A));
        assert_eq!(
            resolve_next(&queue, &gw, &HourlyCap::new(10)),
            Some((Checked::Queued(A.into()), true))
        );
        assert!(queue.process_next(&gw));
        assert_eq!(queue.index_length(), 1);
    }
}
