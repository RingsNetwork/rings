//! Seed and SeedLoader use for getting peers from endpoint.

use rings_rpc::protos::rings_node::ConnectWithSeedRequest;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

/// A list contains SeedPeer.
#[derive(Deserialize, Serialize, Debug)]
pub struct Seed {
    /// Peers loaded from seed configuration.
    pub peers: Vec<SeedPeer>,
}

/// SeedPeer contain `Did` and `endpoint`.
#[derive(Deserialize, Serialize)]
pub struct SeedPeer {
    /// an unique identify.
    pub did: String,
    /// remote client endpoint
    pub url: String,
    /// Optional Bearer token required by the remote endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,
}

impl std::fmt::Debug for SeedPeer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SeedPeer")
            .field("did", &self.did)
            .field("url", &self.url)
            .field("api_token", &self.api_token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl TryFrom<ConnectWithSeedRequest> for Seed {
    type Error = Error;

    fn try_from(req: ConnectWithSeedRequest) -> Result<Self, Error> {
        let mut peers = Vec::new();

        for peer in req.peers {
            peers.push(SeedPeer {
                did: peer.did,
                url: peer.url,
                api_token: peer.api_token,
            });
        }

        Ok(Seed { peers })
    }
}

impl Seed {
    /// Converts this seed list into the RPC request used by `connectWithSeed`.
    pub fn into_connect_with_seed_request(self) -> ConnectWithSeedRequest {
        let mut peers = Vec::new();

        for peer in self.peers {
            peers.push(rings_rpc::protos::rings_node::SeedPeer {
                did: peer.did,
                url: peer.url,
                api_token: peer.api_token,
            });
        }

        ConnectWithSeedRequest { peers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_debug_output_redacts_remote_api_token() {
        let secret = "0123456789abcdef0123456789abcdef";
        let seed = Seed {
            peers: vec![SeedPeer {
                did: "did:ring:test".to_string(),
                url: "https://example.com:50001/".to_string(),
                api_token: Some(secret.to_string()),
            }],
        };
        let debug = format!("{seed:?}");
        assert!(!debug.contains(secret));
        assert!(debug.contains("[REDACTED]"));
    }
}
