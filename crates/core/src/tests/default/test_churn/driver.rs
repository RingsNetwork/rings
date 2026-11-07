//! Trace driver bridging production dummy-transport churn into [`SimState`].

use std::collections::BTreeMap;

use crate::simulation::model::SimAction;
use crate::simulation::model::SimConnectionState;
use crate::simulation::model::SimEventId;
use crate::simulation::model::SimFrameIdentity;
use crate::simulation::model::SimMaintenanceOutcome;
use crate::simulation::model::SimNodeId;
use crate::simulation::model::SimState;
use crate::simulation::model::SimTransferClass;
use crate::simulation::ProductionTraceObservation;
use crate::simulation::ScheduledDelivery;
use crate::simulation::ScheduledDeliveryClass;
use crate::simulation::SimulationRuntimeGuard;
use crate::tests::default::Node;

pub(super) struct ChurnTraceDriver {
    pub(super) state: SimState,
    did_to_node: BTreeMap<String, SimNodeId>,
    node_to_did: BTreeMap<SimNodeId, String>,
    generation_endpoints: BTreeMap<String, (SimNodeId, SimNodeId)>,
    active_generations: BTreeMap<(SimNodeId, SimNodeId), (String, SimEventId)>,
    pending_repair_intent: BTreeMap<SimNodeId, SimEventId>,
    last_node_event: BTreeMap<SimNodeId, SimEventId>,
    last_global_event: Option<SimEventId>,
}

/// Monadic causal-event builder threading `(ChurnTraceDriver, SimNodeId, Option<SimEventId>)`
/// across a sequence of Kleisli transitions `Option<SimEventId> -> SimAction`.
struct NodeEventChain<'a> {
    driver: &'a mut ChurnTraceDriver,
    node: SimNodeId,
    causal_parent: Option<SimEventId>,
}

impl<'a> NodeEventChain<'a> {
    /// Apply `build(causal_parent)` and advance `causal_parent` to the resulting [`SimEventId`].
    fn then<F>(mut self, build: F) -> Self
    where F: FnOnce(Option<SimEventId>) -> SimAction {
        let event = self.driver.apply(build(self.causal_parent));
        self.driver.last_node_event.insert(self.node, event);
        self.causal_parent = Some(event);
        self
    }

    /// Conditionally apply `build(causal_parent)` when `cond` holds, otherwise preserve state.
    fn then_if<F>(self, cond: bool, build: F) -> Self
    where F: FnOnce(Option<SimEventId>) -> SimAction {
        if cond {
            self.then(build)
        } else {
            self
        }
    }

    /// Return the most recent [`SimEventId`] produced along this chain.
    fn finish(self) -> SimEventId {
        self.causal_parent
            .expect("NodeEventChain::finish requires at least one applied transition")
    }
}

impl ChurnTraceDriver {
    pub(super) fn new(nodes: &[Node]) -> Self {
        let (did_to_node, node_to_did) = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let sim_node =
                    SimNodeId(u16::try_from(index).expect("node index must fit SimNodeId"));
                let did = node.did().to_string();
                ((did.clone(), sim_node), (sim_node, did))
            })
            .unzip();
        Self {
            state: SimState::default(),
            did_to_node,
            node_to_did,
            generation_endpoints: BTreeMap::new(),
            active_generations: BTreeMap::new(),
            pending_repair_intent: BTreeMap::new(),
            last_node_event: BTreeMap::new(),
            last_global_event: None,
        }
    }

    fn begin_node_chain(
        &mut self,
        node: SimNodeId,
        initial_parent: Option<SimEventId>,
    ) -> NodeEventChain<'_> {
        let causal_parent = initial_parent
            .or_else(|| self.last_node_event.get(&node).copied())
            .or(self.last_global_event);
        NodeEventChain {
            driver: self,
            node,
            causal_parent,
        }
    }

    pub(super) fn node_id(&self, did: &str) -> SimNodeId {
        *self
            .did_to_node
            .get(did)
            .unwrap_or_else(|| panic!("unknown DID in churn trace driver: {did}"))
    }

    pub(super) fn node_did(&self, node: SimNodeId) -> &str {
        self.node_to_did
            .get(&node)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("unknown SimNodeId in churn trace driver: {node:?}"))
    }

    pub(super) fn record_inject_sync(&mut self, node: SimNodeId, entries: u64) -> SimEventId {
        self.begin_node_chain(node, None)
            .then(|_| SimAction::InjectSync { node, entries })
            .finish()
    }

    pub(super) fn advance_virtual(&mut self, delta_ms: u64) {
        if delta_ms == 0 {
            return;
        }
        let _ = self.apply(SimAction::AdvanceVirtualTime { delta_ms });
    }

    pub(super) fn record_join(&mut self, node: SimNodeId, via: SimNodeId) -> SimEventId {
        self.begin_node_chain(node, None)
            .then(|_| SimAction::Join { node, via })
            .finish()
    }

    pub(super) fn record_leave(&mut self, node: SimNodeId, graceful: bool) -> SimEventId {
        let event = self
            .begin_node_chain(node, None)
            .then(|_| SimAction::Leave { node, graceful })
            .finish();
        self.pending_repair_intent.remove(&node);
        event
    }

    pub(super) fn record_crash(&mut self, node: SimNodeId) -> SimEventId {
        let event = self
            .begin_node_chain(node, None)
            .then(|_| SimAction::Crash { node })
            .finish();
        self.pending_repair_intent.remove(&node);
        event
    }

    pub(super) fn ensure_active_generation(
        &mut self,
        node: SimNodeId,
        peer: SimNodeId,
        generation: &str,
    ) -> SimEventId {
        let edge = (node, peer);
        self.generation_endpoints
            .insert(generation.to_owned(), edge);
        if self.state.active_generation(node, peer) == Some(generation) {
            if let Some((_, event)) = self.active_generations.get(&edge) {
                return *event;
            }
        }
        let local_did = self.node_did(node).to_owned();
        let peer_did = self.node_did(peer).to_owned();
        let stale_gen = self
            .state
            .active_generation(node, peer)
            .filter(|&existing| existing != generation)
            .map(str::to_owned);
        let initial_parent = stale_gen
            .as_ref()
            .and_then(|_| self.active_generations.get(&edge).map(|(_, ev)| *ev));
        let active_event = self
            .begin_node_chain(node, initial_parent)
            .then_if(stale_gen.is_some(), |causal_parent| {
                SimAction::ConnectionLifecycle {
                    node,
                    peer,
                    local_did: local_did.clone(),
                    peer_did: peer_did.clone(),
                    generation: stale_gen.unwrap_or_default(),
                    state: SimConnectionState::Closed,
                    causal_parent,
                }
            })
            .then(|causal_parent| SimAction::ConnectionLifecycle {
                node,
                peer,
                local_did,
                peer_did,
                generation: generation.to_owned(),
                state: SimConnectionState::Active,
                causal_parent,
            })
            .finish();
        self.active_generations
            .insert(edge, (generation.to_owned(), active_event));
        active_event
    }

    pub(super) fn sync_node_connections(&mut self, nodes: &[Node]) {
        let active_edges: Vec<(SimNodeId, SimNodeId, String)> = nodes
            .iter()
            .enumerate()
            .flat_map(|(local_idx, local)| {
                nodes
                    .iter()
                    .enumerate()
                    .filter(move |&(peer_idx, _)| local_idx != peer_idx)
                    .filter_map(move |(peer_idx, peer)| {
                        local
                            .swarm
                            .transport
                            .get_connection(peer.did())
                            .and_then(|conn| conn.dummy_generation_id().ok())
                            .map(|gen| {
                                (SimNodeId(local_idx as u16), SimNodeId(peer_idx as u16), gen)
                            })
                    })
            })
            .collect();
        for (local_id, peer_id, generation) in active_edges {
            let _ = self.ensure_active_generation(local_id, peer_id, &generation);
        }
    }

    pub(super) fn close_edge_if_active(&mut self, node: SimNodeId, peer: SimNodeId) {
        let Some(generation) = self.state.active_generation(node, peer).map(str::to_owned) else {
            return;
        };
        let local_did = self.node_did(node).to_owned();
        let peer_did = self.node_did(peer).to_owned();
        let _ = self
            .begin_node_chain(node, None)
            .then(|causal_parent| SimAction::ConnectionLifecycle {
                node,
                peer,
                local_did,
                peer_did,
                generation,
                state: SimConnectionState::Closed,
                causal_parent,
            })
            .finish();
        self.active_generations.remove(&(node, peer));
    }

    pub(super) fn observe_peer_removal_if_due(&mut self, observer: SimNodeId, departed: SimNodeId) {
        if self.state.has_disconnected(observer, departed) {
            self.close_edge_if_active(observer, departed);
            return;
        }
        let Some(failure_event) = self.state.failure_event(observer, departed) else {
            return;
        };
        let disconnect_event = self
            .begin_node_chain(observer, Some(failure_event))
            .then(|causal_parent| SimAction::Disconnect {
                node: observer,
                peer: departed,
                causal_parent,
            })
            .finish();
        self.pending_repair_intent
            .insert(observer, disconnect_event);
        self.close_edge_if_active(observer, departed);
    }

    pub(super) fn record_delivered_frame(
        &mut self,
        runtime: &SimulationRuntimeGuard,
        nodes: &[Node],
        delivery: &ScheduledDelivery,
    ) {
        let Some(class) = model_class(delivery.class) else {
            return;
        };
        if !self
            .generation_endpoints
            .contains_key(&delivery.connection_generation)
        {
            self.sync_node_connections(nodes);
        }
        let Some(&(node, peer)) = self
            .generation_endpoints
            .get(&delivery.connection_generation)
        else {
            return;
        };
        let lifecycle_event =
            self.ensure_active_generation(node, peer, &delivery.connection_generation);
        let identity = SimFrameIdentity {
            local_did: self.node_did(node).to_owned(),
            peer_did: self.node_did(peer).to_owned(),
            connection_generation: delivery.connection_generation.clone(),
            transaction_id: delivery.transaction_id,
        };
        let initial_parent = self
            .last_node_event
            .get(&node)
            .copied()
            .or(Some(lifecycle_event));
        let transfer_id = delivery.sequence;
        let bytes = u64::try_from(delivery.bytes).expect("frame bytes must fit u64");
        let enqueued_virtual_ms = delivery.enqueued_virtual_ms;
        let deadline_virtual_ms = delivery.deadline_virtual_ms;

        let _ = self
            .begin_node_chain(node, initial_parent)
            .then(|causal_parent| SimAction::SubmitFrame {
                transfer_id,
                node,
                class,
                identity: identity.clone(),
                causal_parent,
            })
            .then(|causal_parent| SimAction::AdmitFrame {
                transfer_id,
                node,
                class,
                bytes,
                enqueued_virtual_ms,
                deadline_virtual_ms,
                identity,
                causal_parent,
            })
            .then(|causal_parent| SimAction::DispatchFrame {
                transfer_id,
                causal_parent,
            })
            .then_if(class == SimTransferClass::Reassembly, |causal_parent| {
                SimAction::AdvanceReassembly {
                    node,
                    transfer_id,
                    causal_parent,
                }
            })
            .then(|causal_parent| SimAction::DeliverFrame {
                transfer_id,
                causal_parent,
            })
            .finish();
        self.drain_production_observations(runtime);
    }

    pub(super) fn drain_production_observations(&mut self, runtime: &SimulationRuntimeGuard) {
        let observations = runtime
            .take_production_trace_observations()
            .expect("production trace observations must remain readable");
        for observation in observations {
            match observation {
                ProductionTraceObservation::PersistOneEntry { node_did } => {
                    let node = self.node_id(&node_did);
                    let _ = self
                        .begin_node_chain(node, None)
                        .then(|causal_parent| SimAction::PersistOneEntry {
                            node,
                            causal_parent,
                        })
                        .finish();
                }
                ProductionTraceObservation::YieldActor { node_did } => {
                    let node = self.node_id(&node_did);
                    let _ = self
                        .begin_node_chain(node, None)
                        .then(|causal_parent| SimAction::YieldActor {
                            node,
                            causal_parent,
                        })
                        .finish();
                }
            }
        }
    }

    pub(super) fn observe_maintenance(
        &mut self,
        node: SimNodeId,
        outcome: SimMaintenanceOutcome,
        repair_cursor: Option<String>,
    ) {
        let repair_intent = self.pending_repair_intent.remove(&node);
        let _ = self
            .begin_node_chain(node, repair_intent)
            .then_if(repair_intent.is_some(), |causal_parent| {
                SimAction::ScheduleRepair {
                    node,
                    entries: 0,
                    causal_parent,
                }
            })
            .then(|causal_parent| SimAction::RunMaintenance {
                node,
                outcome,
                repair_cursor,
                causal_parent,
            })
            .finish();
    }

    fn apply(&mut self, action: SimAction) -> SimEventId {
        let (next, output) = std::mem::take(&mut self.state)
            .transition(action)
            .expect("churn trace transition must be structurally valid");
        self.state = next;
        self.last_global_event = Some(output.event_id);
        output.event_id
    }
}

fn model_class(class: ScheduledDeliveryClass) -> Option<SimTransferClass> {
    match class {
        ScheduledDeliveryClass::Lifecycle | ScheduledDeliveryClass::LinkControl => None,
        ScheduledDeliveryClass::Control => Some(SimTransferClass::Control),
        ScheduledDeliveryClass::Storage => Some(SimTransferClass::Storage),
        ScheduledDeliveryClass::Reassembly => Some(SimTransferClass::Reassembly),
        ScheduledDeliveryClass::E2e => Some(SimTransferClass::E2e),
        ScheduledDeliveryClass::Application => Some(SimTransferClass::Application),
    }
}
