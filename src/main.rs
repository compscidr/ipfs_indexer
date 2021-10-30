use cid::Cid;
use futures::executor::block_on;
use libp2p::{identity, PeerId};
use log::{info, warn};
use simple_logger::SimpleLogger;
use std::convert::TryFrom;
use std::env;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod index_result;
mod indexer;

use indexer::Indexer;

fn main() -> Result<(), Box<dyn Error>> {
    //uncomment to enable hardcoded logging
    //simple_logger::init_with_level(log::Level::Info).unwrap();

    //otherwise run with log level set via RUST_LOG=info ./ipfs_indexer
    SimpleLogger::new().env().init().unwrap();

    let args: Vec<String> = env::args().collect();
    let mut gateway = "ipfs.io";
    if args.len() < 2 {
        warn!("Running with ipfs.io gateway. Usage: ipfs_indexer <ipfs_node_address>");
    } else {
        gateway = &args[1];
    }

    let mut index = Indexer::new(gateway.to_string());
    index.start();

    // enqueue the same cid twice to make sure we get the output that it's already in the map
    // note: delays are so that we don't stop before the indexer has a chance to work, in reality we don't need them
    let wikipedia_cid = Cid::try_from("QmXoypizjW3WknFiJnKLwHCnL72vedxjQkDDP1mXWo6uco").unwrap();
    index.enqueue_cid(wikipedia_cid);

    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    info!("Local peer id: {:?}", local_peer_id);

    let _transport = block_on(libp2p::development_transport(local_key))?;

    // this stuff conflicts with the running ipfs node,
    // so need to rejig it otherwise it panics before indexing starts

    // Create a ping network behaviour.
    //
    // For illustrative purposes, the ping protocol is configured to
    // keep the connection alive, so a continuous sequence of pings
    // can be observed.
    // let behaviour = Ping::new(PingConfig::new().with_keep_alive(true));

    // let mut swarm = Swarm::new(transport, behaviour, local_peer_id);

    // // Tell the swarm to listen on all interfaces and a random, OS-assigned
    // // port.
    // swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    // // Dial the peer identified by the multi-address given as the second
    // // command-line argument, if any.
    // if let Some(addr) = std::env::args().nth(1) {
    //     let remote = addr.parse()?;
    //     swarm.dial_addr(remote)?;
    //     info!("Dialed {}", addr)
    // }

    // block_on(future::poll_fn(move |cx| loop {
    //     match swarm.poll_next_unpin(cx) {
    //         Poll::Ready(Some(event)) => match event {
    //             SwarmEvent::NewListenAddr { address, .. } => info!("Listening on {:?}", address),
    //             SwarmEvent::Behaviour(event) => info!("{:?}", event),
    //             _ => {}
    //         },
    //         Poll::Ready(None) => return Poll::Ready(()),
    //         Poll::Pending => return Poll::Pending
    //     }
    // }));

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    info!("Waiting for Ctrl-C...");
    while running.load(Ordering::SeqCst) {}
    info!("Got it! Exiting...");

    index.stop();
    Ok(())
}
