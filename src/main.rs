use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use threadpool::ThreadPool;

use actix_web::{get, web, App, HttpResponse, HttpServer};
use log::{error, info, warn};
use simple_logger::SimpleLogger;

use crate::denylist::Denylist;
use crate::index_queue::{Enqueued, IndexQueue};

mod admin;
mod denylist;
mod index_queue;
mod index_result;
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
        "Queue length: {} Index size: {} Mirrors skipped: {} Number of Keywords: {} Gateway: {} ({}) Version: {}",
        queue.queue_length(),
        queue.index_length(),
        queue.mirror_length(),
        queue.keyword_length(),
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

// {item:.*} so "cid/path" submissions match too
#[get("/enqueue/{item:.*}")]
async fn enqueue(data: web::Data<IndexQueue>, item: web::Path<String>) -> HttpResponse {
    let item = item.into_inner();
    match data.enqueue(item.clone()) {
        Enqueued::Queued => HttpResponse::Ok().body(format!("Enqueued {}", item)),
        Enqueued::InvalidCid => {
            HttpResponse::BadRequest().body(format!("Not a valid CID: {}", item))
        }
        Enqueued::Blocked => {
            HttpResponse::Forbidden().body(format!("Blocked by denylist: {}", item))
        }
    }
}

// ranked results; ?limit=n&offset=m pages through them (#209), all of them by default
#[get("/search/{query}")]
async fn search(
    data: web::Data<IndexQueue>,
    item: web::Path<String>,
    params: web::Query<HashMap<String, String>>,
) -> HttpResponse {
    let query = item.into_inner();
    let number = |name: &str, default: usize| match params.get(name) {
        None => Ok(default),
        Some(value) => value.parse::<usize>().map_err(|_| name.to_string()),
    };
    let (offset, limit) = match (number("offset", 0), number("limit", usize::MAX)) {
        (Ok(offset), Ok(limit)) => (offset, limit),
        (Err(name), _) | (_, Err(name)) => {
            return HttpResponse::BadRequest().body(format!("{} must be a number", name))
        }
    };
    info!("Searching for {}", query);
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
    // fail closed like badbits: starting without the takedown list would load and serve
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

    // the persisted index (#106): only pages that aren't denylisted or expired are loaded, and
    // nothing is crawled or served until that's done. A configured but unusable DATA_DIR is fatal.
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
        let expired = refresh_queue.expire(max_age_secs);
        if expired > 0 {
            info!("Expired {} pages older than the max age", expired);
        }
        // expired pages aren't in the map any more, so the crawl from the seed reaches them again
        refresh_queue.reseed();
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
    let pool = ThreadPool::new(n_workers);

    // todo: find some way to shutdown the pool when the server stops
    for _ in 0..n_workers {
        let inner_config = Arc::clone(&index_queue);
        let inner_gateway = gateway.clone();
        pool.execute(move || {
            inner_config.start(inner_gateway);
        });
    }

    let gateway_data = web::Data::new(Gateway(gateway));
    let flush_queue = Arc::clone(&index_queue);
    let served = HttpServer::new(move || {
        App::new()
            .app_data(index_queue.clone())
            .app_data(gateway_data.clone())
            .service(status)
            .service(version_handler)
            .service(resources)
            .service(enqueue)
            .service(search)
            .service(keywords)
            .service(roots)
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

        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(search)).await;

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
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
        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(search)).await;

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
