use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use threadpool::ThreadPool;

use actix_web::{get, web, App, HttpResponse, HttpServer};
use log::{debug, error, info, warn};
use simple_logger::SimpleLogger;

use crate::denylist::Denylist;
use crate::entry::Entry;
use crate::index_queue::{Enqueued, IndexQueue};

mod admin;
mod breaker;
mod denylist;
mod discovery;
mod ens;
mod entry;
mod index_queue;
mod index_result;
mod pauses;
mod rate;
mod search;
mod segment;
mod segments;
mod store;

// crate version, plus the commit when built with GIT_SHA set (the docker build passes it)
fn version() -> String {
    match option_env!("GIT_SHA").filter(|sha| !sha.is_empty()) {
        Some(sha) => format!(
            "{} ({})",
            env!("CARGO_PKG_VERSION"),
            &sha[..sha.len().min(7)]
        ),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

// the gateway the workers fetch through, for /status
struct Gateway(String);

#[get("/status")]
async fn status(queue: web::Data<IndexQueue>, gateway: web::Data<Gateway>) -> HttpResponse {
    HttpResponse::Ok().body(format!(
        "Queue length: {} Index size: {} Mirrors skipped: {} Number of Keywords: {} Indexed per minute: {} Discovered: {} Gateway: {} ({}) Version: {}",
        queue.queue_length(),
        queue.index_length(),
        queue.mirror_length(),
        queue.keyword_length(),
        queue.indexed_per_minute(),
        queue.discovered_count(),
        gateway.0,
        queue.gateway_health(),
        version()
    ))
}

/// Process memory, index size (the SQLite file and segment files) and free space under DATA_DIR, in bytes ("-" if unknown) (#214)
#[get("/resources")]
async fn resources(queue: web::Data<IndexQueue>) -> HttpResponse {
    let path = queue.store_path();
    let show = |v: Option<u64>| v.map_or("-".to_string(), |v| v.to_string());
    HttpResponse::Ok().body(format!(
        "Memory: {} Index file: {} Disk free: {}",
        show(rss_bytes()),
        show(path.as_deref().and_then(index_bytes)),
        show(path.as_deref().and_then(disk_free_bytes)),
    ))
}

// index.sqlite plus the segment files beside it (#247)
fn index_bytes(path: &std::path::Path) -> Option<u64> {
    let file = std::fs::metadata(path).ok()?.len();
    let segments = path.parent().map(|dir| dir.join("segments"));
    let segments: u64 = match segments.and_then(|dir| std::fs::read_dir(dir).ok()) {
        Some(entries) => entries
            .filter_map(|e| e.ok()?.metadata().ok())
            .map(|m| m.len())
            .sum(),
        None => 0,
    };
    Some(file + segments)
}

// VmRSS from /proc/self/status (Linux only)
fn rss_bytes() -> Option<u64> {
    let proc_status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = proc_status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

// space available to unprivileged users on the filesystem holding `path`
fn disk_free_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is NUL-terminated and stat is a valid out-pointer
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[get("/version")]
async fn version_handler() -> HttpResponse {
    HttpResponse::Ok().body(version())
}

// returns the top keywords
#[get("/keywords")]
async fn keywords(queue: web::Data<IndexQueue>) -> HttpResponse {
    HttpResponse::Ok().body(format!("Keywords: {:?}", queue.top_keywords(10)))
}

// one line per root CID: "cid<TAB>indexed<TAB>queued<TAB>title", most indexed first (#232)
#[get("/roots")]
async fn roots(queue: web::Data<IndexQueue>) -> HttpResponse {
    let lines: Vec<String> = queue
        .root_counts()
        .into_iter()
        .map(|(cid, indexed, queued, title)| {
            format!(
                "{}\t{}\t{}\t{}",
                cid,
                indexed,
                queued,
                title.replace(['\t', '\n'], " ")
            )
        })
        .collect();
    HttpResponse::Ok().body(lines.join("\n"))
}

// {item:.*} so "cid/path" submissions match too. A name (a DNSLink domain, a .eth name or an
// IPNS key) is recorded for the resolver, which queues its root (#213); the rest is a CID as before.
#[get("/enqueue/{item:.*}")]
async fn enqueue(data: web::Data<IndexQueue>, item: web::Path<String>) -> HttpResponse {
    let item = item.into_inner();
    if let Some(Entry::Name(name)) = entry::parse(&item) {
        return match data.discover(&name, "submitted") {
            Ok(true) => HttpResponse::Ok().body(format!("Queued {} to resolve", name)),
            Ok(false) => HttpResponse::Ok().body(format!("Already known: {}", name)),
            Err(err) => HttpResponse::InternalServerError()
                .body(format!("Couldn't record {}: {}", name, err)),
        };
    }
    match data.enqueue(item.clone()) {
        Enqueued::Queued => HttpResponse::Ok().body(format!("Enqueued {}", item)),
        Enqueued::InvalidCid => {
            HttpResponse::BadRequest().body(format!("Not a valid CID or name: {}", item))
        }
        Enqueued::Blocked => {
            HttpResponse::Forbidden().body(format!("Blocked by denylist: {}", item))
        }
    }
}

/// One line per discovered entry (#213): entry, source, root, last checked (unix time) and the
/// last error, tab-separated ("-" for none), most recently checked first
#[get("/discovered")]
async fn discovered(queue: web::Data<IndexQueue>) -> HttpResponse {
    let dash = |v: Option<String>| v.unwrap_or_else(|| "-".to_string());
    let lines: Vec<String> = queue
        .discovered()
        .into_iter()
        .map(|d| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                d.entry,
                d.source,
                dash(d.root),
                dash(d.checked_at.map(|t| t.to_string())),
                dash(d.error.map(|e| e.replace(['\t', '\n'], " ")))
            )
        })
        .collect();
    HttpResponse::Ok().body(lines.join("\n"))
}

// ranked results; ?limit=n&offset=m pages through them (#209): 100 by default, the first 1,000 at most (#248)
#[get("/search/{query}")]
async fn search_handler(
    data: web::Data<IndexQueue>,
    item: web::Path<String>,
    params: web::Query<HashMap<String, String>>,
) -> HttpResponse {
    let query = item.into_inner();
    let number = |name: &str, default: usize| match params.get(name) {
        None => Ok(default),
        Some(value) => value.parse::<usize>().map_err(|_| name.to_string()),
    };
    let (offset, limit) = match (number("offset", 0), number("limit", 100)) {
        (Ok(offset), Ok(limit)) => (offset, limit),
        (Err(name), _) | (_, Err(name)) => {
            return HttpResponse::BadRequest().body(format!("{} must be a number", name))
        }
    };
    debug!("Searching for {}", query);
    let (total, results) = data.search(&query, offset, limit);
    if total == 0 {
        HttpResponse::Ok().body(format!("No results found for {}", query))
    } else {
        HttpResponse::Ok().body(format!(
            "Results for {} ({} total): {:?}",
            query, total, results
        ))
    }
}

/// How many crawl workers to run: `CRAWL_WORKERS`, or 10 when unset
fn crawl_workers(value: Option<String>) -> Result<usize, String> {
    match value {
        None => Ok(10),
        Some(value) => match value.parse::<usize>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!(
                "CRAWL_WORKERS must be a whole number above 0, got {:?}",
                value
            )),
        },
    }
}

/// The lowest healthy indexing rate: `HEALTH_MIN_RATE` pages/min, or 500 when unset; 0 turns the
/// check off (#285)
fn health_min_rate(value: Option<String>) -> Result<u64, String> {
    match value {
        None => Ok(500),
        Some(value) => value.parse::<u64>().map_err(|_| {
            format!(
                "HEALTH_MIN_RATE must be a whole number of pages a minute (0 turns the check off), got {:?}",
                value
            )
        }),
    }
}

/// New discovered roots queued an hour: `DISCOVERY_PER_HOUR`, or 500 when unset; 0 is no limit
/// (#213)
fn discovery_per_hour(value: Option<String>) -> Result<u32, String> {
    match value {
        None => Ok(500),
        Some(value) => value.parse::<u32>().map_err(|_| {
            format!(
                "DISCOVERY_PER_HOUR must be a whole number of roots an hour (0 = no limit), got {:?}",
                value
            )
        }),
    }
}

/// The most blocks one ENS eth_getLogs request asks for: `ETH_LOGS_MAX_BLOCKS`, or 10,000 when
/// unset; set it to the provider's limit (10 on Alchemy's free plan) (#213)
fn eth_logs_max_blocks(value: Option<String>) -> Result<u64, String> {
    match value {
        None => Ok(10_000),
        Some(value) => match value.parse::<u64>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!(
                "ETH_LOGS_MAX_BLOCKS must be a whole number of blocks above 0, got {:?}",
                value
            )),
        },
    }
}

/// Seconds between ENS requests: `ETH_REQUEST_SECS`, or 1 when unset; raise it to stay within a
/// provider's monthly allowance (6 keeps Alchemy's free plan under its 30M compute units) (#213)
fn eth_request_secs(value: Option<String>) -> Result<u64, String> {
    match value {
        None => Ok(1),
        Some(value) => value.parse::<u64>().map_err(|_| {
            format!(
                "ETH_REQUEST_SECS must be a whole number of seconds, got {:?}",
                value
            )
        }),
    }
}

/// Starts the ENS scan through `url` (#213); returns the URL's host, the one part safe to log.
/// The client is built on the scan's own thread: reqwest's blocking client panics when built
/// inside actix's async main (v0.5.0 crashed at start with ETH_RPC_URL set).
fn start_ens(
    queue: Arc<IndexQueue>,
    url: String,
    limits: discovery::EnsLimits,
) -> Result<String, String> {
    let host = ens::host_of(&url).ok_or("ETH_RPC_URL isn't a URL with a host")?;
    keep_running("ENS scan", move || {
        let rpc = ens::HttpRpc::new(url.clone()).expect("an HTTP client for ETH_RPC_URL");
        discovery::run_ens(&queue, &rpc, &limits)
    });
    Ok(host)
}

// runs a background loop on its own thread; one that panics is logged and started again a
// minute later
fn keep_running(name: &'static str, run: impl Fn() + Send + 'static) {
    std::thread::spawn(move || loop {
        if std::panic::catch_unwind(AssertUnwindSafe(&run)).is_err() {
            error!("{} panicked; starting it again in a minute", name);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

// the rate is a 10-minute average, so it means little until this long after a start
const HEALTH_GRACE: Duration = Duration::from_secs(15 * 60);

/**
 * Whether the crawl is working, for a monitor (#285): Err with the reason while it's paused after
 * repeated gateway errors, or indexing fewer than min_rate pages a minute with work queued. While
 * starting, or with nothing queued, it's Ok.
 */
fn health(
    rate: u64,
    queued: usize,
    paused: Option<String>,
    min_rate: u64,
    up_for: Duration,
) -> Result<String, String> {
    if let Some(paused) = paused {
        return Err(paused);
    }
    let counts = format!("{} pages/min, {} queued", rate, queued);
    if up_for < HEALTH_GRACE {
        return Ok(format!("starting: {}", counts));
    }
    if queued > 0 && rate < min_rate {
        return Err(format!(
            "slow: {} pages/min (minimum {}), {} queued",
            rate, min_rate, queued
        ));
    }
    Ok(format!("ok: {}", counts))
}

// when the process started and HEALTH_MIN_RATE, for /health
struct Health {
    started: Instant,
    min_rate: u64,
}

/// 200 while the crawl is working, 503 when it's paused or slow, with the reason (#285)
#[get("/health")]
async fn health_handler(queue: web::Data<IndexQueue>, config: web::Data<Health>) -> HttpResponse {
    match health(
        queue.indexed_per_minute(),
        queue.queue_length(),
        queue.crawl_paused(),
        config.min_rate,
        config.started.elapsed(),
    ) {
        Ok(why) => HttpResponse::Ok().body(why),
        Err(why) => HttpResponse::ServiceUnavailable().body(why),
    }
}

/// Drops pages past the max age, then re-seeds: at startup and daily after the denylist refresh
fn expire_and_reseed(queue: &IndexQueue, max_age_secs: i64) {
    let expired = queue.expire(max_age_secs);
    if expired > 0 {
        info!("Expired {} pages older than the max age", expired);
    }
    // expired pages aren't known any more, so the crawl from the seed reaches them again
    queue.reseed();
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    //uncomment to enable hardcoded logging
    //simple_logger::init_with_level(log::Level::Info).unwrap();

    //otherwise run with log level set via RUST_LOG=info ./ipfs_indexer
    SimpleLogger::new().env().init().unwrap();

    // ipfs.io no longer serves non-browser clients, see #197
    let mut gateway: String = "ipfs.filebase.io".to_string();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        warn!(
            "Running with {} gateway. Usage: ipfs_indexer <ipfs_node_address>",
            gateway
        );
    } else {
        info!("Running with ifps gateway {}", args[1]);
        gateway = args[1].clone();
    }

    let index_queue = web::Data::new(IndexQueue::new());

    // self-throttle on the public gateway (Filebase allows 200/min; stay well under it, leaving
    // room for redirect fetches). Your own node (gateway argument): no limit unless configured.
    let default_rate = if args.len() < 2 { 100 } else { 0 };
    let gateway_rate = match std::env::var("GATEWAY_REQUESTS_PER_MIN") {
        Ok(value) => value.parse::<u32>().unwrap_or_else(|_| {
            error!(
                "GATEWAY_REQUESTS_PER_MIN must be a whole number, got {:?}",
                value
            );
            std::process::exit(1);
        }),
        Err(_) => default_rate,
    };
    index_queue.set_gateway_rate(gateway_rate);
    if gateway_rate == 0 {
        info!("Gateway requests: no limit");
    } else {
        info!("Gateway requests: at most {} per minute", gateway_rate);
    }

    // fail closed: don't crawl anything until the badbits denylist is loaded. Fetched on a plain
    // thread since the blocking client can't run inside the actix runtime.
    match std::thread::spawn(Denylist::fetch).join().unwrap() {
        Ok(denylist) => {
            info!("Loaded {} denylist entries", denylist.len());
            index_queue.set_denylist(denylist);
        }
        Err(err) => {
            error!(
                "Couldn't load denylist from {}: {}",
                denylist::BADBITS_URL,
                err
            );
            std::process::exit(1);
        }
    }
    // optional hand-maintained takedown list (plain CIDs / cid/path lines). Loaded once now, so
    // the store's startup check below sees it, then re-read every 5 min.
    let local_denylist = std::env::var("LOCAL_DENYLIST").ok();
    // fail closed like badbits: starting without the takedown list would serve
    // stored pages that are on it
    if let Some(path) = &local_denylist {
        match Denylist::load_file(path) {
            Ok(denylist) => {
                index_queue.set_local_denylist(denylist);
            }
            Err(err) => {
                error!("Couldn't read local denylist {}: {}", path, err);
                std::process::exit(1);
            }
        }
    }

    // the darksearch admin area's takedown endpoints; off (404) without a token
    let admin = std::env::var("ADMIN_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
        .map(|token| {
            web::Data::new(admin::Admin {
                token,
                local_denylist: local_denylist.clone().map(std::path::PathBuf::from),
            })
        });
    info!(
        "Admin endpoints: {}",
        if admin.is_some() {
            "on"
        } else {
            "off (no ADMIN_TOKEN)"
        }
    );

    // the persisted index (#106): nothing is loaded from it (#231); blocked and expired pages
    // are erased by the purge and expiry threads below. A configured but unusable DATA_DIR is fatal.
    let max_age_secs = std::env::var("INDEX_MAX_AGE_DAYS")
        .ok()
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(30)
        * 24
        * 60
        * 60;
    // the most of the index SQLite keeps in memory; the rest stays on disk (#231)
    let cache_mb = match std::env::var("INDEX_CACHE_MB") {
        Ok(value) => value.parse::<u32>().unwrap_or_else(|_| {
            error!("INDEX_CACHE_MB must be a whole number, got {:?}", value);
            std::process::exit(1);
        }),
        Err(_) => 1024,
    };
    if let Ok(dir) = std::env::var("DATA_DIR") {
        info!("Index cache: at most {} MB in memory", cache_mb);
        let attached = index_queue.open_store(std::path::Path::new(&dir), cache_mb, max_age_secs);
        match attached {
            Ok((pages, queued)) => info!(
                "Opened {}/index.sqlite: {} pages, {} queued",
                dir, pages, queued
            ),
            Err(err) => {
                error!("Couldn't use DATA_DIR {}: {}", dir, err);
                std::process::exit(1);
            }
        }
    }

    // after the store: the queue lives in it (#231)
    index_queue.reseed();

    // index upkeep (#247): flushes and erasing taken-down pages from disk (#249) on one thread,
    // merges on another, so a long merge holds up neither. A panicked round is logged and the
    // loop goes on: without it the buffer would grow unflushed and takedowns stay on disk.
    let upkeep_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || loop {
        let round = std::panic::catch_unwind(AssertUnwindSafe(|| upkeep_queue.maintain_store()));
        if round.is_err() {
            error!("Index upkeep panicked; carrying on");
        }
    });
    let merge_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || loop {
        let round = std::panic::catch_unwind(AssertUnwindSafe(|| merge_queue.merge_store()));
        if round.is_err() {
            error!("Index merge panicked; carrying on");
        }
    });
    // erases pages a denylist change blocks (#231): search hides them at once, this takes them
    // off disk; started after attach_store, which queues one full purge
    let purge_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || loop {
        let round = std::panic::catch_unwind(AssertUnwindSafe(|| purge_queue.run_purges()));
        if round.is_err() {
            error!("Denylist purge panicked; carrying on");
        }
    });
    // pages that expired while the indexer was down, then a reseed in case the seed was one;
    // the refresh thread does the same daily
    let expire_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || expire_and_reseed(&expire_queue, max_age_secs));

    let refresh_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(24 * 60 * 60));
        match Denylist::fetch() {
            Ok(denylist) => {
                info!("Refreshed denylist: {} entries", denylist.len());
                refresh_queue.set_denylist(denylist);
            }
            Err(err) => warn!("Couldn't refresh denylist, keeping the old one: {}", err),
        }
        expire_and_reseed(&refresh_queue, max_age_secs);
    });

    if let Some(path) = local_denylist {
        let local_queue = Arc::clone(&index_queue);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5 * 60));
            if let Err(err) = admin::reload_local(&local_queue, std::path::Path::new(&path)) {
                warn!("Couldn't read local denylist {}: {}", path, err);
            }
        });
    }

    // fetches through Kubo take ~2 s, so the worker count caps the crawl rate (#246)
    let n_workers = crawl_workers(std::env::var("CRAWL_WORKERS").ok()).unwrap_or_else(|err| {
        error!("{}", err);
        std::process::exit(1);
    });
    info!("Crawl workers: {}", n_workers);
    let min_rate = health_min_rate(std::env::var("HEALTH_MIN_RATE").ok()).unwrap_or_else(|err| {
        error!("{}", err);
        std::process::exit(1);
    });
    // crawling starts now: /health's grace period counts from here
    let health_config = web::Data::new(Health {
        started: Instant::now(),
        min_rate,
    });
    let per_hour =
        discovery_per_hour(std::env::var("DISCOVERY_PER_HOUR").ok()).unwrap_or_else(|err| {
            error!("{}", err);
            std::process::exit(1);
        });
    // per-page lines are debug (about 3 GB a day at info): one line a minute says how the crawl is going
    let summary_queue = Arc::clone(&index_queue);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        info!(
            "Indexed per minute: {} Index size: {} Queue length: {}",
            summary_queue.indexed_per_minute(),
            summary_queue.index_length(),
            summary_queue.queue_length()
        );
        // drops the pauses of roots crawling normally with nothing queued (#286)
        summary_queue.prune_pauses();
    });
    let pool = ThreadPool::new(n_workers);

    // todo: find some way to shutdown the pool when the server stops
    for _ in 0..n_workers {
        let inner_config = Arc::clone(&index_queue);
        let inner_gateway = gateway.clone();
        pool.execute(move || {
            inner_config.start(inner_gateway);
        });
    }

    // discovery (#213): the resolver always runs, since submitted names need it; each source runs
    // only when configured
    let cap = Arc::new(discovery::HourlyCap::new(per_hour));
    {
        let (queue, gateway, cap) = (Arc::clone(&index_queue), gateway.clone(), Arc::clone(&cap));
        keep_running("Discovery resolver", move || {
            discovery::run_resolver(&queue, &gateway, &cap)
        });
    }
    match std::env::var("SEEDS_FILE").ok().filter(|p| !p.is_empty()) {
        Some(path) => {
            info!("Seeds file: {}", path);
            let queue = Arc::clone(&index_queue);
            keep_running("Seeds file", move || {
                discovery::run_seeds(&queue, std::path::Path::new(&path))
            });
        }
        None => info!("Seeds file: none (no SEEDS_FILE)"),
    }
    // the URL carries an API key: only its host is ever logged
    match std::env::var("ETH_RPC_URL").ok().filter(|u| !u.is_empty()) {
        Some(url) => {
            let exit = |err: String| -> ! {
                error!("{}", err);
                std::process::exit(1);
            };
            let limits = discovery::EnsLimits {
                max_blocks: eth_logs_max_blocks(std::env::var("ETH_LOGS_MAX_BLOCKS").ok())
                    .unwrap_or_else(|err| exit(err)),
                pause: Duration::from_secs(
                    eth_request_secs(std::env::var("ETH_REQUEST_SECS").ok())
                        .unwrap_or_else(|err| exit(err)),
                ),
            };
            let host =
                start_ens(Arc::clone(&index_queue), url, limits).unwrap_or_else(|err| exit(err));
            info!(
                "ENS scan: through {}, at most {} blocks a request, {:?} apart",
                host, limits.max_blocks, limits.pause
            );
        }
        None => info!("ENS scan: off (no ETH_RPC_URL)"),
    }

    let gateway_data = web::Data::new(Gateway(gateway));
    let flush_queue = Arc::clone(&index_queue);
    let served = HttpServer::new(move || {
        App::new()
            .app_data(index_queue.clone())
            .app_data(gateway_data.clone())
            .app_data(health_config.clone())
            .service(status)
            .service(health_handler)
            .service(version_handler)
            .service(resources)
            .service(enqueue)
            .service(search_handler)
            .service(keywords)
            .service(roots)
            .service(discovered)
            .configure(|cfg| admin::configure(cfg, admin.clone()))
    })
    // localhost by default; set BIND_ADDR=0.0.0.0:9090 to accept remote connections
    .bind(std::env::var("BIND_ADDR").unwrap_or("127.0.0.1:9090".to_string()))?
    .run()
    .await;
    // what's still buffered is written; a crash instead re-queues it on the next start
    flush_queue.flush_store();
    served
}

#[cfg(test)]
mod tests {
    use actix_web::{test, web, App};

    use super::*;

    async fn get_resources(queue: IndexQueue) -> String {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(queue))
                .service(resources),
        )
        .await;
        let req = test::TestRequest::get().uri("/resources").to_request();
        let body = test::call_and_read_body(&app, req).await;
        String::from_utf8(body.to_vec()).unwrap()
    }

    // "Memory: <bytes> Index file: <bytes|-> Disk free: <bytes|->"
    fn fields(body: &str) -> Vec<String> {
        let parts: Vec<&str> = body.split(": ").collect();
        assert_eq!(parts.len(), 4, "{}", body);
        parts[1..]
            .iter()
            .map(|p| p.split(' ').next().unwrap().to_string())
            .collect()
    }

    const MIN: Duration = Duration::from_secs(60);

    #[actix_web::test]
    async fn health_is_ok_while_starting_even_with_no_rate() {
        assert_eq!(
            health(0, 1000, None, 500, 14 * MIN),
            Ok("starting: 0 pages/min, 1000 queued".to_string())
        );
    }

    #[actix_web::test]
    async fn health_fails_when_indexing_is_slow_with_work_queued() {
        assert_eq!(
            health(87, 1857261, None, 500, 15 * MIN),
            Err("slow: 87 pages/min (minimum 500), 1857261 queued".to_string())
        );
        assert_eq!(
            health(500, 1857261, None, 500, 15 * MIN),
            Ok("ok: 500 pages/min, 1857261 queued".to_string())
        );
    }

    #[actix_web::test]
    async fn health_is_ok_with_nothing_to_crawl() {
        assert_eq!(
            health(0, 0, None, 500, 60 * MIN),
            Ok("ok: 0 pages/min, 0 queued".to_string())
        );
    }

    #[actix_web::test]
    async fn health_fails_while_the_crawl_is_paused() {
        let paused = "paused 4m: 16 of the last 20 fetches failed; last: 504".to_string();
        assert_eq!(
            health(3000, 1000, Some(paused.clone()), 500, MIN),
            Err(paused)
        );
    }

    #[actix_web::test]
    async fn a_min_rate_of_zero_turns_the_rate_check_off() {
        assert_eq!(
            health(0, 1000, None, 0, 60 * MIN),
            Ok("ok: 0 pages/min, 1000 queued".to_string())
        );
    }

    #[actix_web::test]
    async fn health_min_rate_defaults_to_500_and_rejects_non_numbers() {
        assert_eq!(health_min_rate(None), Ok(500));
        assert_eq!(health_min_rate(Some("0".to_string())), Ok(0));
        assert_eq!(health_min_rate(Some("1200".to_string())), Ok(1200));
        for bad in ["", "-1", "fast", "1.5"] {
            assert!(health_min_rate(Some(bad.to_string())).is_err(), "{:?}", bad);
        }
    }

    #[actix_web::test]
    async fn discovery_per_hour_defaults_to_500_and_rejects_non_numbers() {
        assert_eq!(discovery_per_hour(None), Ok(500));
        assert_eq!(discovery_per_hour(Some("0".to_string())), Ok(0));
        assert_eq!(discovery_per_hour(Some("2000".to_string())), Ok(2000));
        for bad in ["", "-1", "lots", "1.5"] {
            assert!(
                discovery_per_hour(Some(bad.to_string())).is_err(),
                "{:?}",
                bad
            );
        }
    }

    // main runs inside actix's async runtime, where building reqwest's blocking client panics
    // (v0.5.0 crashed at start with ETH_RPC_URL set)
    #[actix_web::test]
    async fn the_ens_scan_starts_from_the_async_main() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let started = start_ens(
            Arc::new(IndexQueue::new()),
            format!("http://127.0.0.1:{}/v2/SECRETKEY", port),
            discovery::EnsLimits::default(),
        );
        assert_eq!(started, Ok("127.0.0.1".to_string()));
        assert!(start_ens(
            Arc::new(IndexQueue::new()),
            "not a url".to_string(),
            discovery::EnsLimits::default()
        )
        .is_err());
    }

    #[actix_web::test]
    async fn eth_limits_default_to_a_paid_plan_and_reject_bad_values() {
        assert_eq!(eth_logs_max_blocks(None), Ok(10_000));
        assert_eq!(eth_logs_max_blocks(Some("10".to_string())), Ok(10));
        assert_eq!(eth_request_secs(None), Ok(1));
        assert_eq!(eth_request_secs(Some("6".to_string())), Ok(6));
        for bad in ["", "0", "-1", "ten", "1.5"] {
            assert!(
                eth_logs_max_blocks(Some(bad.to_string())).is_err(),
                "{:?}",
                bad
            );
        }
        for bad in ["", "-1", "six"] {
            assert!(
                eth_request_secs(Some(bad.to_string())).is_err(),
                "{:?}",
                bad
            );
        }
    }

    #[actix_web::test]
    async fn enqueue_records_names_for_the_resolver() {
        let index_queue = web::Data::new(IndexQueue::new());
        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(enqueue)).await;
        for (uri, code, body) in [
            (
                "/enqueue/docs.ipfs.tech",
                200,
                "Queued docs.ipfs.tech to resolve",
            ),
            (
                "/enqueue/Docs.IPFS.tech",
                200,
                "Already known: docs.ipfs.tech",
            ),
            ("/enqueue/vitalik.eth", 200, "Queued vitalik.eth to resolve"),
            (
                "/enqueue/two%20words.com",
                400,
                "Not a valid CID or name: two words.com",
            ),
        ] {
            let resp =
                test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
            assert_eq!(resp.status().as_u16(), code, "{}", uri);
            let got = test::read_body(resp).await;
            assert_eq!(std::str::from_utf8(&got).unwrap(), body, "{}", uri);
        }
        assert_eq!(index_queue.queue_length(), 0, "names wait for the resolver");
        let sources: Vec<String> = index_queue
            .discovered()
            .into_iter()
            .map(|d| d.source)
            .collect();
        assert_eq!(sources, vec!["submitted", "submitted"]);
    }

    #[actix_web::test]
    async fn enqueue_refuses_a_key_hidden_in_a_path() {
        use cid::multihash::Multihash;
        let mut bytes = vec![8, 1, 18, 32];
        bytes.extend([7u8; 32]);
        let key = cid::CidGeneric::<64>::new_v1(0x72, Multihash::wrap(0, &bytes).unwrap())
            .to_string_of_base(cid::multibase::Base::Base36Lower)
            .unwrap();
        let index_queue = web::Data::new(IndexQueue::new());
        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(enqueue)).await;
        for uri in [
            format!("/enqueue/junk/ipfs/{}", key),
            format!("/enqueue/junk%2Fipfs%2F{}", key),
            format!("/enqueue/%252e%252e%5Cipfs%5C{}%3Fx=/ipfs/{}", key, key),
        ] {
            let resp =
                test::call_service(&app, test::TestRequest::get().uri(&uri).to_request()).await;
            assert_eq!(resp.status().as_u16(), 400, "{}", uri);
        }
        assert_eq!(index_queue.discovered_count(), 0);
        assert_eq!(index_queue.queue_length(), 0);
    }

    #[actix_web::test]
    async fn discovered_lists_entries_and_status_counts_them() {
        let index_queue = web::Data::new(IndexQueue::new());
        index_queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let app = test::init_service(
            App::new()
                .app_data(index_queue.clone())
                .app_data(web::Data::new(Gateway("gw.example".to_string())))
                .service(discovered)
                .service(status),
        )
        .await;
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get().uri("/discovered").to_request(),
        )
        .await;
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "docs.ipfs.tech\tseeds\t-\t-\t-"
        );
        let body =
            test::call_and_read_body(&app, test::TestRequest::get().uri("/status").to_request())
                .await;
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains(" Discovered: 1 Gateway: gw.example"),
            "{:?}",
            body
        );
    }

    #[actix_web::test]
    async fn health_get_answers_503_when_slow_and_200_when_starting() {
        let queue = web::Data::new(IndexQueue::new());
        queue.enqueue("bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze".to_string());
        for (started, code, body) in [
            (
                Instant::now() - 20 * MIN,
                503,
                "slow: 0 pages/min (minimum 500), 1 queued",
            ),
            (Instant::now(), 200, "starting: 0 pages/min, 1 queued"),
        ] {
            let app = test::init_service(
                App::new()
                    .app_data(queue.clone())
                    .app_data(web::Data::new(Health {
                        started,
                        min_rate: 500,
                    }))
                    .service(health_handler),
            )
            .await;
            let req = test::TestRequest::get().uri("/health").to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status().as_u16(), code);
            let got = test::read_body(resp).await;
            assert_eq!(std::str::from_utf8(&got).unwrap(), body);
        }
    }

    #[actix_web::test]
    async fn crawl_workers_default_to_ten() {
        assert_eq!(crawl_workers(None), Ok(10));
    }

    #[actix_web::test]
    async fn crawl_workers_take_the_configured_count() {
        assert_eq!(crawl_workers(Some("30".to_string())), Ok(30));
    }

    #[actix_web::test]
    async fn crawl_workers_reject_zero_and_non_numbers() {
        for bad in ["0", "-1", "ten", ""] {
            assert!(crawl_workers(Some(bad.to_string())).is_err(), "{:?}", bad);
        }
    }

    #[actix_web::test]
    async fn resources_without_a_store_report_memory_only() {
        let body = get_resources(IndexQueue::new()).await;
        assert!(
            body.starts_with("Memory: ") && body.contains(" Index file: - Disk free: -"),
            "{}",
            body
        );
        assert!(
            fields(&body)[0].parse::<u64>().unwrap() > 1_000_000,
            "{}",
            body
        );
    }

    #[actix_web::test]
    async fn resources_report_the_index_file_and_free_disk() {
        let dir = store::tests::temp_dir("resources");
        let queue = IndexQueue::new();
        queue
            .attach_store(store::Store::open(&dir, 0).unwrap(), 86400)
            .unwrap();
        assert!(queue.persist(
            "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze",
            &store::tests::page(
                "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze",
                "a",
                &[("alpha", 1)]
            )
        ));
        queue.flush_for_test();
        let body = get_resources(queue).await;
        let f = fields(&body);
        // the SQLite file plus the segment files (#247)
        let segments: u64 = std::fs::read_dir(dir.join("segments"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(segments > 0);
        assert_eq!(
            f[1].parse::<u64>().unwrap(),
            std::fs::metadata(dir.join("index.sqlite")).unwrap().len() + segments,
            "{}",
            body
        );
        assert!(f[2].parse::<u64>().unwrap() > 0, "{}", body);
    }

    #[actix_web::test]
    async fn test_status_get() {
        let index_queue = web::Data::new(IndexQueue::new());

        let req = test::TestRequest::get().uri("/status").to_request();

        let app = test::init_service(
            App::new()
                .app_data(index_queue.clone())
                .app_data(web::Data::new(Gateway("gw.example".to_string())))
                .service(status),
        )
        .await;

        let body = test::call_and_read_body(&app, req).await;
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            body.contains("Gateway: gw.example (no fetches yet)"),
            "{}",
            body
        );
        assert!(
            body.ends_with(&format!("Version: {}", version())),
            "{}",
            body
        );
    }

    #[actix_web::test]
    async fn test_enqueue_get() {
        let index_queue = web::Data::new(IndexQueue::new());
        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(enqueue)).await;

        let cid = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        for uri in [
            format!("/enqueue/{}", cid),
            format!("/enqueue/{}/wiki", cid),
        ] {
            let req = test::TestRequest::get().uri(&uri).to_request();
            assert!(test::call_service(&app, req).await.status().is_success());
        }
        assert_eq!(index_queue.queue_length(), 2);

        for uri in ["/enqueue/enqueueItem", "/enqueue/notacid/wiki"] {
            let req = test::TestRequest::get().uri(uri).to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
        }
        assert_eq!(index_queue.queue_length(), 2);
    }

    #[actix_web::test]
    async fn test_enqueue_denylisted_is_forbidden() {
        use sha2::Digest;
        let blocked = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let hash: String = sha2::Sha256::digest(format!("{}/", blocked).as_bytes())
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        let index_queue = web::Data::new(IndexQueue::new());
        index_queue.set_denylist(Denylist::parse(&format!("//{}\n", hash)));
        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(enqueue)).await;

        let req = test::TestRequest::get()
            .uri(&format!("/enqueue/{}/page", blocked))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
        assert_eq!(index_queue.queue_length(), 0);
    }

    #[actix_web::test]
    async fn test_search_get() {
        let index_queue = web::Data::new(IndexQueue::new());

        let req = test::TestRequest::get()
            .uri("/search/searchItem")
            .to_request();

        let app = test::init_service(
            App::new()
                .app_data(index_queue.clone())
                .service(search_handler),
        )
        .await;

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_search_defaults_to_a_hundred_results() {
        let index_queue = web::Data::new(IndexQueue::new());
        let cid = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        for i in 0..101 {
            let item = format!("{}/p{}", cid, i);
            let counts = HashMap::from([("word".to_string(), 1)]);
            index_queue.index_for_test(
                &item,
                &crate::index_result::IndexResult::new(
                    item.clone(),
                    "t".into(),
                    "e".into(),
                    counts,
                ),
            );
        }
        let app = test::init_service(
            App::new()
                .app_data(index_queue.clone())
                .service(search_handler),
        )
        .await;
        let req = test::TestRequest::get().uri("/search/word").to_request();
        let body = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(
            body.starts_with("Results for word (101 total): "),
            "{}",
            body
        );
        assert_eq!(body.matches("CID: ").count(), 100);
    }

    #[actix_web::test]
    async fn test_search_limit_and_bad_params() {
        let index_queue = web::Data::new(IndexQueue::new());
        // real CIDs: search serves no other (#231)
        let cid = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
        for item in [
            format!("{}/A", cid),
            format!("{}/B", cid),
            format!("{}/C", cid),
        ] {
            let item = item.as_str();
            let counts = HashMap::from([("word".to_string(), 1)]);
            index_queue.index_for_test(
                item,
                &crate::index_result::IndexResult::new(item.into(), "t".into(), "e".into(), counts),
            );
        }
        let app = test::init_service(
            App::new()
                .app_data(index_queue.clone())
                .service(search_handler),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/search/Word?limit=2")
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.starts_with("Results for Word (3 total): "), "{}", body);
        assert_eq!(body.matches("CID: ").count(), 2);

        let req = test::TestRequest::get()
            .uri("/search/word?offset=x")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
    }
}
