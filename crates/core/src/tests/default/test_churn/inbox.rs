//! Pure ring-liveness / multi-hop routing observers and offline held-inbox delivery helpers
//! for churn scenarios (#773).
//!
//! # Functional Architecture
//!
//! - **Commutative Monoid ([`LivenessSampleCounters`])**: Aggregates ring-liveness and routing
//!   probe observations across stabilize ticks via [`LivenessSampleCounters::combine`].
//! - **Pure Observation & Planning**: [`sample_ring_liveness_and_lookups`],
//!   [`resolve_multihop_find_successor`], [`plan_offline_inbox_injection`], and
//!   [`rejoined_border_neighbors`] inspect topology snapshots without mutating node or transport
//!   state.
//! - **Effectful Transport / Storage Shell**: [`inject_offline_inbox_messages`],
//!   [`deliver_rejoined_inbox_messages`], and [`poll_held_inbox_delivery`] execute the planned
//!   connections, storage-repair passes, and inbox drains.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ops::ControlFlow;

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

/// Commutative monoid of liveness and multi-hop routing sample counts.
///
/// # Monoid Laws
///
/// - **Identity**: `LivenessSampleCounters::default().combine(x) == x == x.combine(LivenessSampleCounters::default())`.
/// - **Associativity**: `a.combine(b).combine(c) == a.combine(b.combine(c))`.
/// - **Commutativity**: `a.combine(b) == b.combine(a)` (under `u64::saturating_add`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct LivenessSampleCounters {
    /// Live nodes observed with an empty successor list.
    pub(super) orphan_samples: u64,
    /// Total live-node observations across all stabilize ticks.
    pub(super) live_node_samples: u64,
    /// Multi-hop `find_successor` probes that resolved the exact true live successor.
    pub(super) lookup_successes: u64,
    /// Total multi-hop `find_successor` probes attempted.
    pub(super) lookup_attempts: u64,
    /// Sum of live population counts across all stabilize ticks.
    pub(super) population_sum: u64,
    /// Number of stabilize ticks sampled.
    pub(super) population_probes: u64,
}

impl LivenessSampleCounters {
    /// Monoidal binary operation combining two sample observations.
    pub(super) const fn combine(self, other: Self) -> Self {
        Self {
            orphan_samples: self.orphan_samples.saturating_add(other.orphan_samples),
            live_node_samples: self
                .live_node_samples
                .saturating_add(other.live_node_samples),
            lookup_successes: self.lookup_successes.saturating_add(other.lookup_successes),
            lookup_attempts: self.lookup_attempts.saturating_add(other.lookup_attempts),
            population_sum: self.population_sum.saturating_add(other.population_sum),
            population_probes: self
                .population_probes
                .saturating_add(other.population_probes),
        }
    }
}

/// Pure observation of ring well-formedness, orphan rate, and multi-hop routing convergence
/// across `live_nodes`.
pub(super) fn sample_ring_liveness_and_lookups(
    nodes: &[Node],
    live_nodes: &BTreeSet<SimNodeId>,
) -> LivenessSampleCounters {
    let live_ids: Vec<SimNodeId> = live_nodes.iter().copied().collect();
    if live_ids.len() <= 1 {
        return LivenessSampleCounters::default();
    }

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

    let orphan_samples = live_ids
        .iter()
        .filter(|&&node_id| {
            let idx = usize::from(node_id.0);
            let state = nodes[idx]
                .dht()
                .topology_state()
                .expect("live node topology state must be readable");
            assert!(
                state.is_well_formed(nodes[idx].dht().successors().capacity()),
                "live node {node_id:?} violated TopologyState::is_well_formed: {state:?}",
            );
            nodes[idx]
                .dht()
                .successors()
                .list()
                .expect("live node successor list must be readable")
                .is_empty()
        })
        .count() as u64;

    let (lookup_attempts, lookup_successes) = live_ids
        .iter()
        .flat_map(|&origin_id| {
            live_ids
                .iter()
                .map(move |&target_id| (origin_id, target_id))
        })
        .filter_map(|(origin_id, target_id)| {
            let origin_idx = usize::from(origin_id.0);
            let origin_did = nodes[origin_idx].did();
            let target_did = nodes[usize::from(target_id.0)].did();
            (target_did != origin_did).then_some((origin_id, origin_idx, target_did))
        })
        .filter_map(|(origin_id, origin_idx, target_did)| {
            let probe_key = StorageKey::inbox_of(target_did).placement();
            let true_successor = topology::successors(&live_dids, probe_key, 1)
                .first()
                .copied()?;
            let state = nodes[origin_idx]
                .dht()
                .topology_state()
                .expect("origin topology state must be readable");
            assert!(
                state.routes_clockwise_toward(probe_key),
                "live node {origin_id:?} violated routes_clockwise_toward",
            );
            let succeeded = resolve_multihop_find_successor(
                nodes,
                &live_did_map,
                origin_idx,
                probe_key,
                live_ids.len(),
            ) == Some(true_successor);
            Some(succeeded)
        })
        .fold((0_u64, 0_u64), |(attempts, successes), succeeded| {
            (
                attempts.saturating_add(1),
                successes.saturating_add(u64::from(succeeded)),
            )
        });

    LivenessSampleCounters {
        orphan_samples,
        live_node_samples: live_ids.len() as u64,
        lookup_successes,
        lookup_attempts,
        population_sum: live_ids.len() as u64,
        population_probes: 1,
    }
}

/// Pure bounded Kleisli unfold of `find_successor(target_key)` across up to `max_hops` live hops.
fn resolve_multihop_find_successor(
    nodes: &[Node],
    live_did_map: &BTreeMap<Did, usize>,
    origin_idx: usize,
    target_key: Did,
    max_hops: usize,
) -> Option<Did> {
    let step = (0..=max_hops).try_fold(origin_idx, |current_idx, _| {
        let succs = nodes[current_idx]
            .dht()
            .successors()
            .list()
            .ok()
            .filter(|list| !list.is_empty());
        let Some(_) = succs else {
            return ControlFlow::Break(None);
        };
        match nodes[current_idx].dht().find_successor(target_key).ok() {
            Some(PeerRingAction::Some(resolved)) => {
                ControlFlow::Break(live_did_map.contains_key(&resolved).then_some(resolved))
            }
            Some(PeerRingAction::RemoteAction(
                next_did,
                PeerRingRemoteAction::FindSuccessor(_),
            )) => match live_did_map.get(&next_did).copied().filter(|_| {
                nodes[current_idx]
                    .swarm
                    .transport
                    .has_active_connection(next_did)
            }) {
                Some(next_idx) => ControlFlow::Continue(next_idx),
                None => ControlFlow::Break(None),
            },
            _ => ControlFlow::Break(None),
        }
    });
    match step {
        ControlFlow::Break(resolved) => resolved,
        ControlFlow::Continue(_) => None,
    }
}

/// Pure plan for injecting one offline held-inbox message for a departed `target` node.
struct OfflineInboxInjectionPlan {
    /// Departed recipient node ID.
    target: SimNodeId,
    /// Canonical DID of `target`.
    target_did: Did,
    /// Live predecessor DID of `target` acting as the message sender/owner.
    owner_did: Did,
    /// Node pool index of `owner_did`.
    owner_idx: usize,
    /// Node pool index of the live successor of `target` responsible for holding the relay inbox.
    holder_idx: usize,
    /// Canonical `StorageKey::inbox_of(target_did)` string.
    inbox_key: String,
    /// Deterministic custom payload bytes injected for `target`.
    payload: Vec<u8>,
}

/// Purely compute the owner/holder routing plan for `target` without performing any I/O.
fn plan_offline_inbox_injection(
    nodes: &[Node],
    live_dids: &[Did],
    target: SimNodeId,
) -> Option<OfflineInboxInjectionPlan> {
    let target_did = nodes[usize::from(target.0)].did();
    let owner_did = topology::predecessor(live_dids, target_did)?;
    let holder_did = topology::successors(live_dids, target_did, 1)
        .first()
        .copied()
        .filter(|&holder_did| holder_did != owner_did)?;
    let owner_idx = nodes.iter().position(|n| n.did() == owner_did)?;
    let holder_idx = nodes.iter().position(|n| n.did() == holder_did)?;
    let mut payload = HELD_INBOX_PREFIX.to_vec();
    payload.extend_from_slice(target.0.to_string().as_bytes());
    Some(OfflineInboxInjectionPlan {
        target,
        target_did,
        owner_did,
        owner_idx,
        holder_idx,
        inbox_key: StorageKey::inbox_of(target_did).to_string(),
        payload,
    })
}

/// Effectful interpreter executing [`OfflineInboxInjectionPlan`] for each departed target in
/// `pending_inbox_targets` and recording confirmed relay-carrier persistence in
/// `active_held_messages`.
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
        let Some(plan) = plan_offline_inbox_injection(nodes, &live_dids, target) else {
            continue;
        };
        ensure_connected_pair(runtime, nodes, plan.owner_idx, plan.holder_idx, driver).await;
        let _ = nodes[plan.holder_idx].dht().notify(plan.owner_did);
        if nodes[plan.owner_idx]
            .swarm
            .send_message(ApplicationData::custom(&plan.payload), plan.target_did)
            .await
            .is_ok()
        {
            drain_churn_traced(runtime, nodes, driver).await;
            let stored_at_owner =
                has_relay_inbox_entry(&nodes[plan.owner_idx], &plan.inbox_key).await;
            let stored_at_holder =
                has_relay_inbox_entry(&nodes[plan.holder_idx], &plan.inbox_key).await;
            if stored_at_owner || stored_at_holder {
                pending_inbox_targets.remove(&plan.target);
                active_held_messages.insert(plan.target, plan.payload);
                *held_messages_injected = held_messages_injected.saturating_add(1);
            }
        }
    }
}

/// Check whether `node`'s [`EntryKind::RelayMessage`] carrier store currently holds `key`.
async fn has_relay_inbox_entry(node: &Node, key: &str) -> bool {
    let dht = node.dht();
    let Some(store) = dht.carrier_storage(EntryKind::RelayMessage) else {
        return false;
    };
    store.get(key).await.ok().flatten().is_some()
}

type BorderNeighbor = (usize, Did);

/// Purely resolve the immediate live predecessor and successor `(index, Did)` bordering `peer_idx`.
fn rejoined_border_neighbors(
    nodes: &[Node],
    live_dids: &[Did],
    peer_idx: usize,
) -> (Option<BorderNeighbor>, Option<BorderNeighbor>) {
    let peer_did = nodes[peer_idx].did();
    let pred = topology::predecessor(live_dids, peer_did).and_then(|pred_did| {
        nodes
            .iter()
            .position(|n| n.did() == pred_did)
            .filter(|&idx| idx != peer_idx)
            .map(|idx| (idx, pred_did))
    });
    let succ = topology::successors(live_dids, peer_did, 1)
        .first()
        .copied()
        .and_then(|succ_did| {
            nodes
                .iter()
                .position(|n| n.did() == succ_did)
                .filter(|&idx| idx != peer_idx)
                .map(|idx| (idx, succ_did))
        });
    (pred, succ)
}

/// Drive bounded storage-repair convergence (`Law (Convergence)` of `dht::relay_holder`) for
/// rejoined nodes with pending held-inbox messages and drain delivered payloads from `app_inboxes`.
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
        let (pred_neighbor, succ_neighbor) = rejoined_border_neighbors(nodes, &live_dids, peer_idx);

        let mut participants = BTreeSet::from([peer_idx]);
        if let Some((pred_idx, pred_did)) = pred_neighbor {
            ensure_connected_pair(runtime, nodes, pred_idx, peer_idx, driver).await;
            let _ = nodes[pred_idx].dht().admit_connected(peer_did, None);
            let _ = nodes[peer_idx].dht().admit_connected(pred_did, None);
            let _ = nodes[pred_idx].dht().notify(peer_did);
            participants.insert(pred_idx);
        }
        if let Some((succ_idx, succ_did)) = succ_neighbor {
            ensure_connected_pair(runtime, nodes, peer_idx, succ_idx, driver).await;
            let _ = nodes[peer_idx].dht().admit_connected(succ_did, None);
            let _ = nodes[succ_idx].dht().admit_connected(peer_did, None);
            let _ = nodes[peer_idx].dht().notify(succ_did);
            participants.insert(succ_idx);
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

/// Non-blockingly drain up to 16 ready deliveries from `inbox` and return `true` if one matches
/// `expected_payload`.
async fn poll_held_inbox_delivery(inbox: &ApplicationInbox, expected_payload: &[u8]) -> bool {
    let expected = ApplicationData::Custom(CustomMessage(expected_payload.to_vec()));
    std::iter::from_fn(|| inbox.next().now_or_never().flatten())
        .take(16)
        .any(|delivery| matches!(delivery, ApplicationDelivery::Message(msg) if msg.data == expected))
}
