use std::borrow::BorrowMut;
use std::cell::{Cell, RefCell, RefMut};
use std::sync::{Arc, Mutex, RwLock};
use futures::mpsc::{channel, Receiver, Sender};
use actix_web::{get, web, HttpResponse};
use cid::Cid;
use futures::channel::mpsc::{Receiver, Sender};
use futures::{SinkExt, TryFutureExt};
use log::{info, warn};
use mime::APPLICATION_JSON;
use crate::index_queue;

#[derive(Clone)]
pub struct IndexQueue {
    queue_size: Arc<RwLock<u32>>,
    sender: Sender<String>,
}

impl IndexQueue {

    pub fn new(queue_size: Arc<RwLock<u32>>, sender: Sender<String>) -> IndexQueue {
        IndexQueue {
            queue_size,
            sender,
        }
    }

    pub fn enqueue(&mut self, cid: Cid, relative_path: String) {
        let key = cid.to_string() + "/" + relative_path.as_str();
        sender.send(key.clone());
        *self.queue_size.write().unwrap() += 1;
        info!("SENT");
    }
}

#[get("queue")]
pub async fn get_queue(data: web::Data<Arc<RwLock<u32>>>) -> HttpResponse {
    info!("IN GET QUEUE");
    HttpResponse::Ok()
        .content_type(APPLICATION_JSON)
        .json(*data.read().unwrap())
}

pub struct IndexQueueConsumer {
    queue_size: Arc<RwLock<u32>>,
    receiver: Receiver<String>,
}

impl IndexQueueConsumer {
    pub fn new(queue_size: Arc<RwLock<u32>>, receiver: Receiver<String>) -> IndexQueueConsumer {
        IndexQueueConsumer {
            queue_size,
            receiver,
        }
    }

    fn decrease_queue(&mut self) {
        *self.queue_size.write().unwrap() -= 1;
    }

    pub async fn process_queue(&mut self) {
        loop {
            let cid = self.receiver.try_next().unwrap();
            info!("RECV");
            self.decrease_queue();
        }
        // match &self.receiver {
        //     Some(receiver) => {
        //         while let Some(msg) = receiver.recv() {
        //             self.queue_size = self.queue_size - 1;
        //         }
        //     }
        //     None => {
        //         warn!("Queue is closed, can't receive")
        //     }
        // }
    }
}