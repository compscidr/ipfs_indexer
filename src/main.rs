use futures::executor::block_on;
use futures::prelude::*;
use libp2p::ping::{Ping, PingConfig};
use libp2p::swarm::{Swarm, SwarmEvent};
use libp2p::{identity, PeerId};
use std::error::Error;
use std::task::Poll;
use cid::multihash::{Code, MultihashDigest};
use cid::Cid;
use log::info;
use std::{thread, time};
use simple_logger::SimpleLogger;

mod indexer;
use indexer::Indexer;

fn main() -> Result<(), Box<dyn Error>> {
    //uncomment to enable hardcoded logging
    //simple_logger::init_with_level(log::Level::Info).unwrap();

    //otherwise run with log level set via RUST_LOG=info ./ipfs_indexer
    SimpleLogger::new().env().init().unwrap();

    let mut index = Indexer::new();
    index.start();

    index.enqueue_cid(Cid::new_v1(indexer::RAW, Code::Sha2_256.digest(b"Hello World")));

    thread::sleep(time::Duration::from_millis(100));

    index.enqueue_cid(Cid::new_v1(indexer::RAW, Code::Sha2_256.digest(b"Hello World")));

    thread::sleep(time::Duration::from_millis(100));

    index.stop();
    
    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    info!("Local peer id: {:?}", local_peer_id);

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
        info!("Dialed {}", addr)
    }

    block_on(future::poll_fn(move |cx| loop {
        match swarm.poll_next_unpin(cx) {
            Poll::Ready(Some(event)) => match event {
                SwarmEvent::NewListenAddr { address, .. } => info!("Listening on {:?}", address),
                SwarmEvent::Behaviour(event) => info!("{:?}", event),
                _ => {}
            },
            Poll::Ready(None) => return Poll::Ready(()),
            Poll::Pending => return Poll::Pending
        }
    }));

    Ok(())
}
