pub mod rings_node;
pub mod rings_node_handler;

use rings_core::inspect::StorageInspect;
use rings_core::inspect::SwarmInspect;

impl From<SwarmInspect> for rings_node::SwarmInfo {
    fn from(inspect: SwarmInspect) -> Self {
        let peers = inspect
            .connections
            .into_iter()
            .map(|conn| rings_node::PeerInfo {
                did: conn.did,
                state: conn.state,
            })
            .collect();

        let dht = rings_node::DhtInfo {
            did: inspect.dht.did,
            successors: inspect.dht.successors,
            predecessor: inspect.dht.predecessor,
            finger_table_ranges: inspect
                .dht
                .finger_table
                .into_iter()
                .map(|(did, start, end)| rings_node::FingerTableRange { did, start, end })
                .collect(),
        };

        Self {
            peers,
            dht: Some(dht),
            persistence_storage: Some(inspect.persistence_storage.into()),
            cache_storage: Some(inspect.cache_storage.into()),
        }
    }
}

impl From<StorageInspect> for rings_node::StorageInfo {
    fn from(inspect: StorageInspect) -> Self {
        Self {
            items: inspect
                .items
                .into_iter()
                .map(|(key, vnode)| rings_node::StorageItem {
                    key,
                    value: Some(rings_node::StorageValue {
                        did: vnode.did.to_string(),
                        kind: format!("{:?}", vnode.kind),
                        data: vnode.data.into_iter().map(|x| x.value().clone()).collect(),
                    }),
                })
                .collect(),
        }
    }
}
