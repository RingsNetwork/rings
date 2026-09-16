//! Seed and SeedLoader use for getting peers from endpoint.

use std::str::FromStr;

use rings_core::dht::Did;
use rings_rpc::protos::rings_node::ConnectWithSeedRequest;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::remote_endpoint::RemoteRpcEndpoint;

/// A list contains SeedPeer.
#[derive(Deserialize, Serialize, Debug)]
pub struct Seed {
    /// Peers loaded from seed configuration.
    pub peers: Vec<SeedPeer>,
}

/// SeedPeer contain `Did` and `endpoint`.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SeedPeer {
    /// an unique identify.
    pub did: String,
    /// remote client endpoint
    pub url: String,
    /// Optional Bearer token for a seed peer that gates its handshake; absent by default,
    /// since the external handshake is public.
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

/// Why a seed entry cannot be dialed.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SeedPeerError {
    /// The entry's `did` does not parse.
    #[error("seed peer did is not a DID: {0}")]
    NotADid(String),
    /// The entry's `url` fails the remote RPC endpoint policy.
    #[error("seed peer {did} has an unusable endpoint: {reason}")]
    UnusableEndpoint {
        /// The entry's DID.
        did: Did,
        /// The policy's refusal.
        reason: String,
    },
    /// The DID is listed again with a different endpoint or token.
    #[error("seed peer {0} is listed with conflicting entries")]
    ConflictingEntries(Did),
    /// The endpoint is listed again under a different DID.
    #[error("seed endpoint {0} is listed under two DIDs")]
    EndpointUnderTwoDids(RemoteRpcEndpoint),
}

/// How two validated seed peers overlap, ordered by agreement: a verbatim repeat agrees more
/// than a shared DID, which agrees more than a shared endpoint.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Overlap {
    /// Same endpoint claimed for another DID.
    SameEndpoint,
    /// Same DID behind another endpoint or token.
    SameDid,
    /// Same DID, endpoint and token: one peer listed twice.
    Verbatim,
}

/// Validate `peers` as a set: every entry validates, an entry repeated verbatim is merged, and
/// a DID listed with a different endpoint or token, or an endpoint listed under two DIDs, is
/// refused as ambiguous, since one endpoint answers as exactly one DID. Endpoint identity is
/// the full URL: two nodes may share an origin behind path routing, and an entry whose
/// endpoint answers as another node is refused at dial time by the DID pin. Against the peers
/// already accepted, the overlap of highest agreement decides.
pub(crate) fn validate_seed_peers(
    peers: impl IntoIterator<Item = SeedPeer>,
) -> Result<Vec<ValidatedSeedPeer>, SeedPeerError> {
    let mut validated: Vec<ValidatedSeedPeer> = Vec::new();
    for peer in peers {
        let peer = ValidatedSeedPeer::try_from(peer)?;
        match validated
            .iter()
            .filter_map(|known| known.overlap(&peer))
            .max()
        {
            Some(Overlap::Verbatim) => continue,
            Some(Overlap::SameDid) => return Err(SeedPeerError::ConflictingEntries(peer.did)),
            Some(Overlap::SameEndpoint) => {
                return Err(SeedPeerError::EndpointUnderTwoDids(peer.endpoint));
            }
            None => validated.push(peer),
        }
    }
    Ok(validated)
}

/// A seed entry that passed validation: a parsed DID and a public HTTP(S) handshake endpoint.
/// The proof travels as a type, so every dial of a seed peer runs the same checks once, where
/// the entry enters.
#[derive(Eq, PartialEq)]
pub(crate) struct ValidatedSeedPeer {
    did: Did,
    endpoint: RemoteRpcEndpoint,
    api_token: Option<String>,
}

impl ValidatedSeedPeer {
    /// DID the endpoint must answer as.
    pub(crate) fn did(&self) -> Did {
        self.did
    }

    /// Validated handshake endpoint.
    pub(crate) fn endpoint(&self) -> &RemoteRpcEndpoint {
        &self.endpoint
    }

    /// Bearer token for a peer that gates its handshake, never logged.
    pub(crate) fn api_token(&self) -> Option<&str> {
        self.api_token.as_deref()
    }

    /// How `other` overlaps with this peer, if at all.
    fn overlap(&self, other: &Self) -> Option<Overlap> {
        if self == other {
            Some(Overlap::Verbatim)
        } else if self.did == other.did {
            Some(Overlap::SameDid)
        } else if self.endpoint == other.endpoint {
            Some(Overlap::SameEndpoint)
        } else {
            None
        }
    }
}

impl std::fmt::Debug for ValidatedSeedPeer {
    /// Render every field but the bearer token, which is replaced by `[REDACTED]`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedSeedPeer")
            .field("did", &self.did)
            .field("endpoint", &self.endpoint.as_str())
            .field("api_token", &self.api_token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl TryFrom<SeedPeer> for ValidatedSeedPeer {
    type Error = SeedPeerError;

    /// Parse the DID and apply the remote RPC endpoint policy to the URL.
    fn try_from(peer: SeedPeer) -> Result<Self, SeedPeerError> {
        let did = Did::from_str(peer.did.as_str()).map_err(|_| SeedPeerError::NotADid(peer.did))?;
        let endpoint = RemoteRpcEndpoint::parse(peer.url.as_str()).map_err(|error| {
            SeedPeerError::UnusableEndpoint {
                did,
                reason: error.to_string(),
            }
        })?;
        Ok(Self {
            did,
            endpoint,
            api_token: peer.api_token,
        })
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

    /// The `Debug` rendering of a validated seed peer never contains its bearer token.
    #[test]
    fn validated_seed_peer_debug_redacts_the_token() {
        let peer = ValidatedSeedPeer::try_from(SeedPeer {
            did: Did::from(1).to_string(),
            url: "https://seed.example.com/".to_string(),
            api_token: Some("0123456789abcdef".to_string()),
        })
        .expect("a token-bearing peer validates");
        let rendered = format!("{peer:?}");
        assert!(!rendered.contains("0123456789abcdef"));
        assert!(rendered.contains("[REDACTED]"));
    }

    /// A seed entry for `did` at `url`.
    fn entry(did: Did, url: &str) -> SeedPeer {
        SeedPeer {
            did: did.to_string(),
            url: url.to_string(),
            api_token: None,
        }
    }

    /// Distinct peers validate; a verbatim repeat merges; a DID with two endpoints and an
    /// endpoint under two DIDs are refused; against several accepted peers the overlap of
    /// highest agreement names the refusal.
    #[test]
    fn seed_peer_sets_merge_repeats_and_refuse_conflicts() {
        let a = entry(Did::from(1), "https://a.example.com/");
        let b = entry(Did::from(2), "https://b.example.com/");
        assert_eq!(
            validate_seed_peers([a.clone(), b.clone(), a.clone()])
                .expect("distinct peers validate")
                .len(),
            2,
            "a verbatim repeat is merged"
        );
        assert_eq!(
            validate_seed_peers([a.clone(), entry(Did::from(1), "https://other.example.com/")])
                .err(),
            Some(SeedPeerError::ConflictingEntries(Did::from(1)))
        );
        assert_eq!(
            validate_seed_peers([a.clone(), entry(Did::from(3), "https://a.example.com/")]).err(),
            Some(SeedPeerError::EndpointUnderTwoDids(
                RemoteRpcEndpoint::parse("https://a.example.com/").expect("a public endpoint")
            ))
        );
        assert_eq!(
            validate_seed_peers([a, b, entry(Did::from(2), "https://a.example.com/")]).err(),
            Some(SeedPeerError::ConflictingEntries(Did::from(2))),
            "a shared DID outranks a shared endpoint"
        );
    }

    /// A malformed DID and a non-public endpoint are refused by typed reason.
    #[test]
    fn validated_seed_peer_refuses_bad_did_and_local_endpoint() {
        let entry = |did: &str, url: &str| SeedPeer {
            did: did.to_string(),
            url: url.to_string(),
            api_token: None,
        };
        assert_eq!(
            ValidatedSeedPeer::try_from(entry("not-a-did", "https://seed.example.com/")).err(),
            Some(SeedPeerError::NotADid("not-a-did".to_string()))
        );
        assert!(matches!(
            ValidatedSeedPeer::try_from(entry(&Did::from(1).to_string(), "http://127.0.0.1:50001/")),
            Err(SeedPeerError::UnusableEndpoint { did, .. }) if did == Did::from(1)
        ));
    }

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
