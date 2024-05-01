use std::convert::TryFrom;
use std::sync::Arc;

use threadpool::ThreadPool;

use actix_web::{get, web, App, HttpResponse, HttpServer};
use cid::Cid;
use log::{info, warn};
use simple_logger::SimpleLogger;

use crate::index_queue::IndexQueue;

mod index_queue;
mod index_result;

#[get("/status")]
async fn status(queue: web::Data<IndexQueue>) -> HttpResponse {
    HttpResponse::Ok().body(format!(
        "Queue length: {} Index size: {} Number of Keywords: {}",
        queue.queue_length(),
        queue.index_length(),
        queue.keyword_length()
    ))
}

#[get("keywords")]
async fn keywords(queue: web::Data<IndexQueue>) -> HttpResponse {
    HttpResponse::Ok().body(format!("ok"))
}

#[get("/enqueue/{item}")]
async fn enqueue(data: web::Data<IndexQueue>, item: web::Path<String>) -> HttpResponse {
    let item = item.into_inner();
    data.enqueue(item.clone());
    HttpResponse::Ok().body(format!("Enqueued {}", item))
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

    let mut gateway: String = "ipfs.io".to_string();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        warn!("Running with ipfs.io gateway. Usage: ipfs_indexer <ipfs_node_address>");
    } else {
        info!("Running with ifps gateway {}", args[1]);
        gateway = args[1].clone();
    }

    let index_queue = web::Data::new(IndexQueue::new());
    let wikipedia_cid = Cid::try_from("bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze").unwrap();
    index_queue.enqueue(wikipedia_cid.to_string());

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

    HttpServer::new(move || {
        App::new()
            .app_data(index_queue.clone())
            .service(status)
            .service(enqueue)
            .service(search)
    })
    .bind("0.0.0.0:9090")?
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

        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(status)).await;

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_enqueue_get() {
        let index_queue = web::Data::new(IndexQueue::new());

        let req = test::TestRequest::get()
            .uri("/enqueue/enqueueItem")
            .to_request();

        let app =
            test::init_service(App::new().app_data(index_queue.clone()).service(enqueue)).await;

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
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
