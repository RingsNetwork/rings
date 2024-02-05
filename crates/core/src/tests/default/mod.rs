use std::sync::Arc;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;

mod test_connection;
mod test_message_handler;
mod test_stabilization;

pub async fn prepare_node(key: SecretKey) -> Arc<Swarm> {
    let stun = "stun://stun.l.google.com:19302";
    let storage = Box::new(MemStorage::new());

    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let swarm = Arc::new(SwarmBuilder::new(stun, storage, session_sk).build());

    println!("key: {:?}", key.to_string());
    println!("did: {:?}", swarm.did());

    swarm
}

pub async fn gen_pure_dht(did: Did) -> Result<PeerRing> {
    let storage = Box::new(MemStorage::new());
    Ok(PeerRing::new_with_storage(did, 3, storage))
}

pub async fn gen_sorted_dht(s: usize) -> Vec<PeerRing> {
    let mut keys: Vec<crate::ecc::SecretKey> = vec![];
    for _i in 0..s {
        keys.push(crate::ecc::SecretKey::random());
    }
    keys.sort_by_key(|a| a.address());

    #[allow(clippy::needless_collect)]
    let dids: Vec<crate::dht::Did> = keys
        .iter()
        .map(|sk| crate::dht::Did::from(sk.address()))
        .collect();

    let mut iter = dids.into_iter();
    let mut ret: Vec<crate::dht::PeerRing> = vec![];
    for _ in 0..s {
        ret.push(
            crate::tests::default::gen_pure_dht(iter.next().unwrap())
                .await
                .unwrap(),
        )
    }
    ret
}
