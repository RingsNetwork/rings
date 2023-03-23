use serde::Deserialize;
use serde::Serialize;

use crate::swarm::Swarm;
use crate::transports::manager::TransportManager;
use crate::types::ice_transport::IceTransportInterface;
use crate::utils::from_rtc_ice_connection_state;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmInspect {
    pub successors: Vec<String>,
    #[serde(default)]
    pub predecessor: Option<String>,
    pub transports: Vec<TransportInspect>,
    pub finger_table: Vec<(Option<String>, usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportInspect {
    pub did: String,
    pub transport_id: String,
    pub state: Option<String>,
}

impl SwarmInspect {
    pub async fn inspect(swarm: &Swarm) -> Self {
        let successors = {
            swarm
                .dht
                .lock_successor()
                .map(|ss| ss.list())
                .unwrap_or_default()
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        };

        let predecessor = {
            swarm
                .dht
                .lock_predecessor()
                .map(|x| *x)
                .ok()
                .flatten()
                .map(|x| x.to_string())
        };

        let finger_table = {
            swarm
                .dht
                .lock_finger()
                .map(|ft| {
                    let finger = ft.list().iter().map(|x| x.map(|did| did.to_string()));
                    compress_iter(finger)
                })
                .unwrap_or_default()
        };

        let transports = {
            let transports = swarm.get_transports();

            let states_async = transports.iter().map(|(_, t)| t.ice_connection_state());
            let states = futures::future::join_all(states_async).await;

            transports
                .iter()
                .zip(states.iter())
                .map(|((did, transport), state)| TransportInspect {
                    did: did.to_string(),
                    transport_id: transport.id.to_string(),
                    state: state.map(from_rtc_ice_connection_state),
                })
                .collect()
        };

        Self {
            successors,
            predecessor,
            finger_table,
            transports,
        }
    }
}

fn compress_iter<T>(iter: impl Iterator<Item = T>) -> Vec<(T, usize, usize)>
where T: PartialEq {
    let mut result = vec![];
    let mut start = 0;
    let mut count = 0;
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
                start = i;
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
