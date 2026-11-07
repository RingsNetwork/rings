//! Liveness/routing sampling and offline held-inbox delivery helpers for churn scenarios (#773).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use futures::FutureExt as _;

use super::driver::ChurnTraceDriver;
use super::runner::drain_churn_traced;
use super::runner::ensure_connected_pair;
use crate::dht::entry::EntryKind;
use crate::dht::topology;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageKey;
use crate::message::CustomMessage;
use crate::simulation::model::SimNodeId;
use crate::simulation::SimulationRuntimeGuard;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationInbox;
use crate::tests::default::Node;

const HELD_INBOX_PREFIX: &[u8] = b"churn-held-inbox-";

#[derive(Default)]
pub(super) struct LivenessSampleCounters {
    pub(super) orphan_samples: u64,
    pub(super) live_node_samples: u64,
    pub(super) lookup_successes: u64,
    pub(super) lookup_attempts: u64,
    pub(super) population_sum: u64,
    pub(super) population_probes: u64,
}

pub(super) fn sample_ring_liveness_and_lookups(
    nodes: &[Node],
    driver: &ChurnTraceDriver,
    counters: &mut LivenessSampleCounters,
) {
    let live_ids: Vec<SimNodeId> = driver.state.live_nodes().iter().copied().collect();
    if live_ids.len() <= 1 {
        return;
    }
    counters.population_sum = counters
        .population_sum
        .saturating_add(live_ids.len() as u64);
    counters.population_probes = counters.population_probes.saturating_add(1);

    let live_dids: Vec<Did> = live_ids
        .iter()
        .map(|id| nodes[usize::from(id.0)].did())
        .collect();
    let live_did_map: BTreeMap<Did, usize> = live_ids
        .iter()
        .map(|id| {
            let idx = usize::from(id.0);
            (nodes[idx].did(), idx)
        })
        .collect();

    for &node_id in &live_ids {
        let idx = usize::from(node_id.0);
        let state = nodes[idx]
            .dht()
            .topology_state()
            .expect("live node topology state must be readable");
        assert!(
            state.is_well_formed(nodes[idx].dht().successors().capacity()),
            "live node {:?} violated TopologyState::is_well_formed: {state:?}",
            node_id
        );
        let succ_list = nodes[idx]
            .dht()
            .successors()
            .list()
            .expect("live node successor list must be readable");
        counters.live_node_samples = counters.live_node_samples.saturating_add(1);
        if succ_list.is_empty() {
            counters.orphan_samples = counters.orphan_samples.saturating_add(1);
        }
    }

    for &origin_id in &live_ids {
        let origin_idx = usize::from(origin_id.0);
        let origin_did = nodes[origin_idx].did();
        for &target_id in &live_ids {
            let target_did = nodes[usize::from(target_id.0)].did();
            if target_did == origin_did {
                continue;
            }
            let probe_key = StorageKey::inbox_of(target_did).placement();
            let Some(true_successor) = topology::successors(&live_dids, probe_key, 1)
                .first()
                .copied()
            else {
                continue;
            };
            let state = nodes[origin_idx]
                .dht()
                .topology_state()
                .expect("origin topology state must be readable");
            assert!(
                state.routes_clockwise_toward(probe_key),
                "live node {:?} violated routes_clockwise_toward",
                origin_id
            );
            counters.lookup_attempts = counters.lookup_attempts.saturating_add(1);
            if resolve_multihop_find_successor(
                nodes,
                &live_did_map,
                origin_idx,
                probe_key,
                live_ids.len(),
            ) == Some(true_successor)
            {
                counters.lookup_successes = counters.lookup_successes.saturating_add(1);
            }
        }
    }
}

fn resolve_multihop_find_successor(
    nodes: &[Node],
    live_did_map: &BTreeMap<Did, usize>,
    origin_idx: usize,
    target_key: Did,
    max_hops: usize,
) -> Option<Did> {
    let mut current_idx = origin_idx;
    for _ in 0..=max_hops {
        let succs = nodes[current_idx].dht().successors().list().ok()?;
        if succs.is_empty() {
            return None;
        }
        match nodes[current_idx].dht().find_successor(target_key).ok()? {
            PeerRingAction::Some(resolved) => {
                return live_did_map.contains_key(&resolved).then_some(resolved);
            }
            PeerRingAction::RemoteAction(next_did, PeerRingRemoteAction::FindSuccessor(_)) => {
                let &next_idx = live_did_map.get(&next_did)?;
                if !nodes[current_idx]
                    .swarm
                    .transport
                    .has_active_connection(next_did)
                {
                    return None;
                }
                current_idx = next_idx;
            }
            _ => return None,
        }
    }
    None
}

pub(super) async fn inject_offline_inbox_messages(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    driver: &mut ChurnTraceDriver,
    pending_inbox_targets: &mut BTreeSet<SimNodeId>,
    active_held_messages: &mut BTreeMap<SimNodeId, Vec<u8>>,
    held_messages_injected: &mut u64,
) {
    let live_ids: Vec<SimNodeId> = driver.state.live_nodes().iter().copied().collect();
    if live_ids.len() < 2 {
        return;
    }
    let live_dids: Vec<Did> = live_ids
        .iter()
        .map(|id| nodes[usize::from(id.0)].did())
        .collect();
    let targets: Vec<SimNodeId> = pending_inbox_targets.iter().copied().collect();
    for target in targets {
        if driver.state.live_nodes().contains(&target) {
            pending_inbox_targets.remove(&target);
            continue;
        }
        let target_did = nodes[usize::from(target.0)].did();
        let Some(owner_did) = topology::predecessor(&live_dids, target_did) else {
            continue;
        };
        let Some(holder_did) = topology::successors(&live_dids, target_did, 1)
            .first()
            .copied()
        else {
            continue;
        };
        if owner_did == holder_did {
            continue;
        }
        let Some(owner_idx) = nodes.iter().position(|n| n.did() == owner_did) else {
            continue;
        };
        let Some(holder_idx) = nodes.iter().position(|n| n.did() == holder_did) else {
            continue;
        };
        ensure_connected_pair(runtime, nodes, owner_idx, holder_idx, driver).await;
        let _ = nodes[holder_idx].dht().notify(owner_did);
        let mut payload = HELD_INBOX_PREFIX.to_vec();
        payload.extend_from_slice(target.0.to_string().as_bytes());
        if nodes[owner_idx]
            .swarm
            .send_message(ApplicationData::custom(&payload), target_did)
            .await
            .is_ok()
        {
            drain_churn_traced(runtime, nodes, driver).await;
            let inbox_key = StorageKey::inbox_of(target_did).to_string();
            let stored_at_owner = has_relay_inbox_entry(&nodes[owner_idx], &inbox_key).await;
            let stored_at_holder = has_relay_inbox_entry(&nodes[holder_idx], &inbox_key).await;
            if stored_at_owner || stored_at_holder {
                pending_inbox_targets.remove(&target);
                active_held_messages.insert(target, payload);
                *held_messages_injected = held_messages_injected.saturating_add(1);
            }
        }
    }
}

async fn has_relay_inbox_entry(node: &Node, key: &str) -> bool {
    let dht = node.dht();
    let Some(store) = dht.carrier_storage(EntryKind::RelayMessage) else {
        return false;
    };
    store.get(key).await.ok().flatten().is_some()
}

pub(super) async fn deliver_rejoined_inbox_messages(
    runtime: &SimulationRuntimeGuard,
    nodes: &[Node],
    app_inboxes: &[ApplicationInbox],
    driver: &mut ChurnTraceDriver,
    active_held_messages: &mut BTreeMap<SimNodeId, Vec<u8>>,
    held_messages_delivered: &mut u64,
) {
    let waiting: Vec<(SimNodeId, Vec<u8>)> = active_held_messages
        .iter()
        .filter(|(node, _)| driver.state.live_nodes().contains(node))
        .map(|(&node, payload)| (node, payload.clone()))
        .collect();
    if waiting.is_empty() {
        return;
    }

    let live_ids: Vec<SimNodeId> = driver.state.live_nodes().iter().copied().collect();
    let mut live_dids: Vec<Did> = live_ids
        .iter()
        .map(|id| nodes[usize::from(id.0)].did())
        .collect();
    live_dids.sort();

    for &(node_id, ref expected_payload) in &waiting {
        let peer_idx = usize::from(node_id.0);
        if poll_held_inbox_delivery(&app_inboxes[peer_idx], expected_payload).await {
            active_held_messages.remove(&node_id);
            *held_messages_delivered = held_messages_delivered.saturating_add(1);
            continue;
        }

        let peer_did = nodes[peer_idx].did();
        let inbox_key = StorageKey::inbox_of(peer_did).to_string();

        let mut participants = BTreeSet::from([peer_idx]);
        if let Some(pred_did) = topology::predecessor(&live_dids, peer_did) {
            if let Some(pred_idx) = nodes.iter().position(|n| n.did() == pred_did) {
                if pred_idx != peer_idx {
                    ensure_connected_pair(runtime, nodes, pred_idx, peer_idx, driver).await;
                    let _ = nodes[pred_idx].dht().admit_connected(peer_did, None);
                    let _ = nodes[peer_idx].dht().admit_connected(pred_did, None);
                    let _ = nodes[pred_idx].dht().notify(peer_did);
                    participants.insert(pred_idx);
                }
            }
        }
        if let Some(succ_did) = topology::successors(&live_dids, peer_did, 1)
            .first()
            .copied()
        {
            if let Some(succ_idx) = nodes.iter().position(|n| n.did() == succ_did) {
                if succ_idx != peer_idx {
                    ensure_connected_pair(runtime, nodes, peer_idx, succ_idx, driver).await;
                    let _ = nodes[peer_idx].dht().admit_connected(succ_did, None);
                    let _ = nodes[succ_idx].dht().admit_connected(peer_did, None);
                    let _ = nodes[peer_idx].dht().notify(succ_did);
                    participants.insert(succ_idx);
                }
            }
        }

        for &holder_id in &live_ids {
            let holder_idx = usize::from(holder_id.0);
            if holder_idx == peer_idx
                || !has_relay_inbox_entry(&nodes[holder_idx], &inbox_key).await
            {
                continue;
            }
            ensure_connected_pair(runtime, nodes, holder_idx, peer_idx, driver).await;
            let _ = nodes[holder_idx].dht().admit_connected(peer_did, None);
            let _ = nodes[peer_idx]
                .dht()
                .admit_connected(nodes[holder_idx].did(), None);
            participants.insert(holder_idx);
        }

        // Law (Convergence) of `dht::relay_holder`: previous-holder lists and chain/owner
        // relay carrier offers settle across bounded storage maintenance passes on the
        // returning peer and the live nodes holding or bordering its inbox key.
        for _ in 0..4 {
            for &idx in &participants {
                nodes[idx].swarm.transport.request_storage_repair();
                let _ = nodes[idx]
                    .swarm
                    .stabilizer()
                    .run_requested_storage_maintenance()
                    .await;
            }
            drain_churn_traced(runtime, nodes, driver).await;

            if poll_held_inbox_delivery(&app_inboxes[peer_idx], expected_payload).await {
                active_held_messages.remove(&node_id);
                *held_messages_delivered = held_messages_delivered.saturating_add(1);
                break;
            }
        }
    }
}

async fn poll_held_inbox_delivery(inbox: &ApplicationInbox, expected_payload: &[u8]) -> bool {
    for _ in 0..16 {
        let Some(delivery) = inbox.next().now_or_never().flatten() else {
            break;
        };
        if let ApplicationDelivery::Message(message) = delivery {
            if message.data == ApplicationData::Custom(CustomMessage(expected_payload.to_vec())) {
                return true;
            }
        }
    }
    false
}
