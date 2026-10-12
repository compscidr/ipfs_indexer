use std::sync::Arc;

use threadpool::ThreadPool;

use actix_web::{get, web, App, HttpResponse, HttpServer};
use log::{error, info, warn};
use simple_logger::SimpleLogger;

use crate::denylist::Denylist;
use crate::index_queue::{Enqueued, IndexQueue};

mod denylist;
mod index_queue;
mod index_result;
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
        "Queue length: {} Index size: {} Number of Keywords: {} Gateway: {} ({}) Version: {}",
        queue.queue_length(),
        queue.index_length(),
        queue.keyword_length(),
        gateway.0,
        queue.gateway_health(),
        version()
    ))
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

#[get("/search/{query}")]
async fn search(data: web::Data<IndexQueue>, item: web::Path<String>) -> HttpResponse {
    let query = item.into_inner();
    info!("Searching for {}", query.clone());
    let results = data.search(query.clone());
    if results.is_empty() {
        HttpResponse::Ok().body(format!("No results found for {}", query))
    } else {
        HttpResponse::Ok().body(format!("Results for {}: {:?}", query, results))
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
    index_queue.reseed();

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
            Ok(denylist) => index_queue.set_local_denylist(denylist),
            Err(err) => {
                error!("Couldn't read local denylist {}: {}", path, err);
                std::process::exit(1);
            }
        }
    }

    // the persisted index (#106): only pages that aren't denylisted or expired are loaded, and
    // nothing is crawled or served until that's done. A configured but unusable DATA_DIR is fatal.
    let max_age_secs = std::env::var("INDEX_MAX_AGE_DAYS")
        .ok()
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(30)
        * 24
        * 60
        * 60;
    if let Ok(dir) = std::env::var("DATA_DIR") {
        let attached = store::Store::open(std::path::Path::new(&dir))
            .and_then(|store| index_queue.attach_store(store, max_age_secs));
        match attached {
            Ok((loaded, blocked, expired)) => info!(
                "Loaded {} pages from {}/index.sqlite (dropped {} blocked, {} expired)",
                loaded, dir, blocked, expired
            ),
            Err(err) => {
                error!("Couldn't use DATA_DIR {}: {}", dir, err);
                std::process::exit(1);
            }
        }
    }

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
            match Denylist::load_file(&path) {
                Ok(denylist) => local_queue.set_local_denylist(denylist),
                Err(err) => warn!("Couldn't read local denylist {}: {}", path, err),
            }
        });
    }

    // if we don't have multiple workers, we can get the case where we run out of room in the
    // queue if a doc has many links
    let n_workers = 10;
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
    HttpServer::new(move || {
        App::new()
            .app_data(index_queue.clone())
            .app_data(gateway_data.clone())
            .service(status)
            .service(version_handler)
            .service(enqueue)
            .service(search)
            .service(keywords)
    })
    // localhost by default; set BIND_ADDR=0.0.0.0:9090 to accept remote connections
    .bind(std::env::var("BIND_ADDR").unwrap_or("127.0.0.1:9090".to_string()))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use actix_web::{test, web, App};

    use super::*;

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
}
