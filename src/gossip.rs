use actix_web::web::Data;
use libp2p::{identity,PeerId};
use log::info;
use crate::IndexQueue;

pub struct Gossip {
    pub index_queue: Data<IndexQueue>,
}

impl Gossip {
    pub fn new(index_queue: Data<IndexQueue>) -> Gossip {
        Gossip {
            index_queue,
        }
    }

    pub fn start(&self) {
        let local_key = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        info!("Local peer id: {:?}", local_peer_id);
        loop {
            //println!("Gossiping");
        }
    }
}