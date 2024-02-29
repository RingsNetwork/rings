use rings_transport::core::transport::ConnectionInterface;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::vnode::VirtualNode;
use crate::dht::PeerRing;
use crate::dht::SuccessorReader;
use crate::dht::VNodeStorage;
use crate::swarm::Swarm;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmInspect {
    pub connections: Vec<ConnectionInspect>,
    pub dht: DHTInspect,
    pub persistence_storage: StorageInspect,
    pub cache_storage: StorageInspect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInspect {
    pub did: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DHTInspect {
    pub did: String,
    pub successors: Vec<String>,
    #[serde(default)]
    pub predecessor: Option<String>,
    pub finger_table: Vec<(Option<String>, u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageInspect {
    pub items: Vec<(String, VirtualNode)>,
}

impl SwarmInspect {
    pub async fn inspect(swarm: &Swarm) -> Self {
        let dht = DHTInspect::inspect(&swarm.dht());
        let connections = {
            let connections = swarm.transport.get_connections();

            connections
                .iter()
                .map(|(did, c)| ConnectionInspect {
                    did: did.to_string(),
                    state: format!("{:?}", c.webrtc_connection_state()),
                })
                .collect()
        };
        let persistence_storage = StorageInspect::inspect_kv_storage(&swarm.dht().storage).await;
        let cache_storage = StorageInspect::inspect_kv_storage(&swarm.dht().cache).await;

        Self {
            connections,
            dht,
            persistence_storage,
            cache_storage,
        }
    }
}

impl DHTInspect {
    pub fn inspect(dht: &PeerRing) -> Self {
        let did = dht.did.to_string();
        let successors = {
            dht.successors()
                .list()
                .unwrap_or_default()
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        };

        let predecessor = {
            dht.lock_predecessor()
                .map(|x| *x)
                .ok()
                .flatten()
                .map(|x| x.to_string())
        };

        let finger_table = {
            dht.lock_finger()
                .map(|ft| {
                    let finger = ft.list().iter().map(|x| x.map(|did| did.to_string()));
                    compress_iter(finger)
                })
                .unwrap_or_default()
        };

        Self {
            did,
            successors,
            predecessor,
            finger_table,
        }
    }
}

impl StorageInspect {
    pub async fn inspect_kv_storage(storage: &VNodeStorage) -> Self {
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

pub fn compress_iter<T>(iter: impl Iterator<Item = T>) -> Vec<(T, u64, u64)>
where T: PartialEq {
    let mut result = vec![];
    let mut start = 0u64;
    let mut count = 0u64;
    let mut prev: Option<T> = None;

    for (i, x) in iter.enumerate() {
        match prev {
            Some(p) if p == x => {
                count += 1;
            }
            _ => {
                if let Some(p) = prev {
                    result.push((p, start, start + count - 1));
                }
                start = i as u64;
                count = 1;
            }
        }
        prev = Some(x);
    }

    if let Some(p) = prev {
        result.push((p, start, start + count - 1));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_iter() {
        let v = vec!['a', 'a', 'f', 'a', 'b', 'b', 'c', 'c', 'c', 'd', 'e'];
        assert_eq!(
            vec![
                ('a', 0, 1),
                ('f', 2, 2),
                ('a', 3, 3),
                ('b', 4, 5),
                ('c', 6, 8),
                ('d', 9, 9),
                ('e', 10, 10),
            ],
            compress_iter(v.into_iter())
        );
    }
}
