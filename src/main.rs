use futures::executor::block_on;
use futures::prelude::*;
use libp2p::ping::{Ping, PingConfig};
use libp2p::swarm::{Swarm, SwarmEvent};
use libp2p::{identity, PeerId};
use std::error::Error;
use std::task::Poll;
use cid::multihash::{Code, MultihashDigest};
use cid::Cid;
use log::{info, warn};
use simple_logger::SimpleLogger;
use chashmap::CHashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::{sync, thread, time};
use std::sync::atomic::{AtomicBool, Ordering};

const RAW: u64 = 0x55;

struct IndexResult {
    pub cid: Cid,
    pub title: String,
    pub excerpt: String
}

struct Indexer {
    map: sync::Arc<CHashMap<Cid, IndexResult>>,
    queue: (Option<Sender<Cid>>, Option<Receiver<Cid>>),
    running: sync::Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    POISON_PILL: Cid
}

// took some ideas from here: https://stackoverflow.com/questions/42043823/design-help-threading-within-a-struct
impl Indexer {
    pub fn new() -> Indexer {
        let (tx, rx) = channel();
        Indexer {
            map: sync::Arc::new(CHashMap::new()),
            queue: (Some(tx), Some(rx)),
            running: sync::Arc::new(AtomicBool::new(false)),
            handle: None,
            POISON_PILL: Cid::new_v1(RAW, Code::Sha2_256.digest(b"Poison Pill"))
        }
    }

    pub fn enqueue_cid(&mut self, cid: Cid) {
        if self.map.contains_key(&cid) {
            info!("cid {} already in map", cid);
            return;
        } else {
            info!("enqueueing cid {}", cid);
            match &self.queue.0 {
                Some(queue) => {
                    if let Err(e) = queue.send(cid) {
                        warn!("error sending cid {} to queue: {}", cid, e);
                    }
                }
                None => {
                    warn!("queue is closed");
                }
            }
        }
    }

    pub fn start(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            warn!("indexer already running");
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let rx = self.queue.1.take().unwrap();
        let POISON_PILL = self.POISON_PILL.clone();
        let map = sync::Arc::clone(&self.map);
        self.handle = Some(thread::spawn(move || {
            info!("indexer thread started");
            while running.load(Ordering::SeqCst) {
                let cid = rx.recv().unwrap();
                info!("processing cid {}", cid);
                if cid == POISON_PILL {
                    info!("received poison pill, stopping indexer thread");
                    break;
                }
                if map.contains_key(&cid) {
                    info!("cid {} already in queue", cid);
                    return;
                } else {
                    // todo: implement retreival and indexing here
                    map.insert(cid, IndexResult {
                        cid: cid.clone(),
                        title: "test".to_string(),
                        excerpt: "test".to_string()
                    });
                    info!("indexed cid {}", cid);
                }
            }
            info!("indexer thread stopped");
        }));
        while !self.running.load(Ordering::SeqCst) {
            info!("waiting for indexer to start");
            thread::sleep(time::Duration::from_millis(100));
        }
        info!("indexer started");
    }
    
    pub fn stop(&mut self) {
        if !self.running.load(Ordering::SeqCst) {
            warn!("trying to stop before indexer started");
            return;
        }
        self.enqueue_cid(self.POISON_PILL);
        self.running.store(false, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    //uncomment to enable hardcoded logging
    //simple_logger::init_with_level(log::Level::Info).unwrap();

    //otherwise run with log level set via RUST_LOG=info ./ipfs_indexer
    simple_logger::init_with_env().unwrap();

    let mut indexer = Indexer::new();
    indexer.start();

    indexer.enqueue_cid(Cid::new_v1(RAW, Code::Sha2_256.digest(b"Hello World")));

    thread::sleep(time::Duration::from_millis(100));

    indexer.enqueue_cid(Cid::new_v1(RAW, Code::Sha2_256.digest(b"Hello World")));

    thread::sleep(time::Duration::from_millis(100));

    indexer.stop();
    
    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    println!("Local peer id: {:?}", local_peer_id);

    let transport = block_on(libp2p::development_transport(local_key))?;

    // Create a ping network behaviour.
    //
    // For illustrative purposes, the ping protocol is configured to
    // keep the connection alive, so a continuous sequence of pings
    // can be observed.
    let behaviour = Ping::new(PingConfig::new().with_keep_alive(true));

    let mut swarm = Swarm::new(transport, behaviour, local_peer_id);

    // Tell the swarm to listen on all interfaces and a random, OS-assigned
    // port.
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    // Dial the peer identified by the multi-address given as the second
    // command-line argument, if any.
    if let Some(addr) = std::env::args().nth(1) {
        let remote = addr.parse()?;
        swarm.dial_addr(remote)?;
        println!("Dialed {}", addr)
    }

    block_on(future::poll_fn(move |cx| loop {
        match swarm.poll_next_unpin(cx) {
            Poll::Ready(Some(event)) => match event {
                SwarmEvent::NewListenAddr { address, .. } => println!("Listening on {:?}", address),
                SwarmEvent::Behaviour(event) => println!("{:?}", event),
                _ => {}
            },
            Poll::Ready(None) => return Poll::Ready(()),
            Poll::Pending => return Poll::Pending
        }
    }));

    Ok(())
}
