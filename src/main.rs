use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use cid::Cid;
use futures::executor::block_on;
use libp2p::{identity, PeerId};
use log::{info, warn};
use simple_logger::SimpleLogger;
use std::convert::TryFrom;
use std::env;
use futures::mpsc::channel;
use std::sync::{Arc, Mutex, RwLock};
use actix_rt::task;
use futures::{future, io};
use actix_web::{get, App, HttpServer, web::{self, Data}, HttpResponse};
use futures::channel::mpsc::channel;
use crate::index_queue::{IndexQueue, IndexQueueConsumer};
use mime::APPLICATION_JSON;

mod index_result;
mod index_queue;
//mod indexer;

//use indexer::Indexer;

#[actix_rt::main]
async fn main() {
    // uncomment to enable hardcoded logging
    // simple_logger::init_with_level(log::Level::Info).unwrap();

    // otherwise run with log level set via RUST_LOG=info ./ipfs_indexer
    SimpleLogger::new().env().init().unwrap();

    let args: Vec<String> = env::args().collect();
    let mut gateway = "ipfs.io"; // another good and fast one is: ipfs-gateway.cloud
    if args.len() < 2 {
        warn!("Running with ipfs.io gateway. Usage: ipfs_indexer <ipfs_node_address>");
    } else {
        gateway = &args[1];
    }

    // let mut index = Indexer::new(gateway.to_string());
    // index.start();
    //
    // // enqueue the same cid twice to make sure we get the output that it's already in the map
    // // note: delays are so that we don't stop before the indexer has a chance to work, in reality we don't need them
    // // we need to start from some known page, so for now we're starting from a known hash for
    // // a wikipedia mirror
    let wikipedia_cid = Cid::try_from("QmXoypizjW3WknFiJnKLwHCnL72vedxjQkDDP1mXWo6uco").unwrap();
    // index.enqueue_cid(wikipedia_cid);

    let queue_size = Arc::new(RwLock::new(0));
    let (tx, rx) = channel(100);
    let mut index_queue = IndexQueue::new(queue_size.clone(), tx);
    index_queue.enqueue(wikipedia_cid, "".to_string());

    // this will run and block until control-c
    // let server_result = HttpServer::new(move || {
    //     App::new()
    //         .app_data(web::Data::new(queue_size.clone()))
    //         // enable logger - always register actix-web Logger middleware last
    //         .service(index_queue::get_queue)
    // }).bind("0.0.0.0:9090")?.run().await;

    let mut index_queue_consumer = IndexQueueConsumer::new(queue_size.clone(), rx);
    task::spawn_blocking(move || {
        index_queue_consumer.process_queue()
    });

    let f2 = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(queue_size.clone()))
                // enable logger - always register actix-web Logger middleware last
                .service(index_queue::get_queue)
        }).bind("0.0.0.0:9090").expect("").run().await;

    //return server_result;
}