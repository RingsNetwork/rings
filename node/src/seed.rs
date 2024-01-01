//! Seed and SeedLoader use for getting peers from endpoint.
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_rpc::protos::rings_node::ConnectWithSeedRequest;

/// A list contains SeedPeer.
#[derive(Deserialize, Serialize, Debug)]
pub struct Seed {
    pub peers: Vec<SeedPeer>,
}

/// SeedPeer contain `Did` and `endpoint`.
#[derive(Deserialize, Serialize, Debug)]
pub struct SeedPeer {
    /// an unique identify.
    pub did: Did,
    /// remote client endpoint
    pub url: String,
}

impl TryFrom<ConnectWithSeedRequest> for Seed {
    type Error = Error;

    fn try_from(req: ConnectWithSeedRequest) -> Result<Self, Error> {
        let mut peers = Vec::new();

        for peer in req.peers {
            let did = Did::from_str(&peer.did).map_err(|_| Error::InvalidDid(peer.did.clone()))?;
            peers.push(SeedPeer { did, url: peer.url });
        }

        Ok(Seed { peers })
    }
}
