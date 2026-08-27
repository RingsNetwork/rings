use serde::Deserialize;
use serde::Serialize;

use super::compress_iter;
use crate::dht::entry::Entry;
use crate::dht::EntryStorage;
use crate::dht::PeerRing;
use crate::swarm::Swarm;

/// Full runtime inspection snapshot for a swarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmInspect {
    /// Active peer connections known by the swarm.
    pub peers: Vec<ConnectionInspect>,
    /// DHT routing state for the local peer.
    pub dht: DHTInspect,
    /// Persistent DHT storage contents.
    pub persistence_storage: StorageInspect,
    /// Cache DHT storage contents.
    pub cache_storage: StorageInspect,
}

/// Inspection snapshot for a single peer connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInspect {
    /// Remote DID as a display string.
    pub did: String,
    /// Connection state as a display string.
    pub state: String,
}

/// Inspection snapshot for local DHT routing state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DHTInspect {
    /// Local node DID.
    pub did: String,
    /// Current successor list.
    pub successors: Vec<String>,
    #[serde(default)]
    /// Current predecessor, when known.
    pub predecessor: Option<String>,
    /// Compressed finger table ranges with optional DID values.
    pub finger_table: Vec<(Option<String>, u64, u64)>,
}

/// Inspection snapshot for key value storage contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageInspect {
    /// Stored entries as `(key, entry)` pairs.
    pub items: Vec<(String, Entry)>,
}

impl SwarmInspect {
    /// Build a full inspection snapshot from `swarm`.
    pub async fn inspect(swarm: &Swarm) -> Self {
        let dht = DHTInspect::inspect(&swarm.dht());
        let peers = swarm.peers();
        let persistence_storage = StorageInspect::inspect_kv_storage(&swarm.dht().storage).await;
        let cache_storage = StorageInspect::inspect_kv_storage(&swarm.dht().cache).await;

        Self {
            peers,
            dht,
            persistence_storage,
            cache_storage,
        }
    }
}

impl DHTInspect {
    /// Build a DHT inspection snapshot from a peer ring.
    pub fn inspect(dht: &PeerRing) -> Self {
        let did = dht.did.to_string();
        let topology = dht.topology_state().ok();
        let successors = topology
            .as_ref()
            .map(|state| {
                state
                    .successors
                    .iter()
                    .copied()
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let predecessor = topology
            .as_ref()
            .and_then(|state| state.predecessor)
            .map(|predecessor| predecessor.to_string());
        let finger_table = topology
            .map(|state| {
                compress_iter(
                    state
                        .fingers
                        .into_iter()
                        .map(|finger| finger.map(|did| did.to_string())),
                )
            })
            .unwrap_or_default();

        Self {
            did,
            successors,
            predecessor,
            finger_table,
        }
    }
}

impl StorageInspect {
    /// Build a storage inspection snapshot from an entry storage handle.
    pub async fn inspect_kv_storage(storage: &EntryStorage) -> Self {
        Self {
            items: storage
                .get_all()
                .await
                .unwrap_or_default()
                .into_iter()
                .collect(),
        }
    }
}
