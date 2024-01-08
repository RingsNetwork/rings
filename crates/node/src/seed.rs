//! Seed and SeedLoader use for getting peers from endpoint.
use std::str::FromStr;

use rings_core::dht::Did;
use rings_rpc::protos::rings_node::ConnectWithSeedRequest;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

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

impl Seed {
    pub fn into_connect_with_seed_request(self) -> ConnectWithSeedRequest {
        let mut peers = Vec::new();

        for peer in self.peers {
            peers.push(rings_rpc::protos::rings_node::SeedPeer {
                did: peer.did.to_string(),
                url: peer.url,
            });
        }

        ConnectWithSeedRequest { peers }
    }
}
