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

impl ChurnTraceDriver {
    pub(super) fn new(nodes: &[Node]) -> Self {
        let mut did_to_node = BTreeMap::new();
        let mut node_to_did = BTreeMap::new();
        for (index, node) in nodes.iter().enumerate() {
            let sim_node = SimNodeId(u16::try_from(index).expect("node index must fit SimNodeId"));
            let did = node.did().to_string();
            did_to_node.insert(did.clone(), sim_node);
            node_to_did.insert(sim_node, did);
        }
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
        let event = self.apply(SimAction::InjectSync { node, entries });
        self.last_node_event.insert(node, event);
        event
    }

    pub(super) fn advance_virtual(&mut self, delta_ms: u64) {
        if delta_ms == 0 {
            return;
        }
        let _ = self.apply(SimAction::AdvanceVirtualTime { delta_ms });
    }

    pub(super) fn record_join(&mut self, node: SimNodeId, via: SimNodeId) -> SimEventId {
        let event = self.apply(SimAction::Join { node, via });
        self.last_node_event.insert(node, event);
        event
    }

    pub(super) fn record_leave(&mut self, node: SimNodeId, graceful: bool) -> SimEventId {
        let event = self.apply(SimAction::Leave { node, graceful });
        self.pending_repair_intent.remove(&node);
        self.last_node_event.insert(node, event);
        event
    }

    pub(super) fn record_crash(&mut self, node: SimNodeId) -> SimEventId {
        let event = self.apply(SimAction::Crash { node });
        self.pending_repair_intent.remove(&node);
        self.last_node_event.insert(node, event);
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
        if let Some(existing_gen) = self.state.active_generation(node, peer).map(str::to_owned) {
            if existing_gen != generation {
                let parent = self
                    .active_generations
                    .get(&edge)
                    .map(|(_, ev)| *ev)
                    .or(self.last_node_event.get(&node).copied())
                    .or(self.last_global_event);
                let close_event = self.apply(SimAction::ConnectionLifecycle {
                    node,
                    peer,
                    local_did: local_did.clone(),
                    peer_did: peer_did.clone(),
                    generation: existing_gen,
                    state: SimConnectionState::Closed,
                    causal_parent: parent,
                });
                self.last_node_event.insert(node, close_event);
            }
        }
        let parent = self
            .last_node_event
            .get(&node)
            .copied()
            .or(self.last_global_event);
        let active_event = self.apply(SimAction::ConnectionLifecycle {
            node,
            peer,
            local_did,
            peer_did,
            generation: generation.to_owned(),
            state: SimConnectionState::Active,
            causal_parent: parent,
        });
        self.active_generations
            .insert(edge, (generation.to_owned(), active_event));
        self.last_node_event.insert(node, active_event);
        active_event
    }

    pub(super) fn sync_node_connections(&mut self, nodes: &[Node]) {
        for (local_idx, local) in nodes.iter().enumerate() {
            let local_id = SimNodeId(local_idx as u16);
            for (peer_idx, peer) in nodes.iter().enumerate() {
                if local_idx == peer_idx {
                    continue;
                }
                let Some(connection) = local.swarm.transport.get_connection(peer.did()) else {
                    continue;
                };
                let Ok(generation) = connection.dummy_generation_id() else {
                    continue;
                };
                let peer_id = SimNodeId(peer_idx as u16);
                let _ = self.ensure_active_generation(local_id, peer_id, &generation);
            }
        }
    }

    pub(super) fn close_edge_if_active(&mut self, node: SimNodeId, peer: SimNodeId) {
        let Some(generation) = self.state.active_generation(node, peer).map(str::to_owned) else {
            return;
        };
        let local_did = self.node_did(node).to_owned();
        let peer_did = self.node_did(peer).to_owned();
        let parent = self
            .last_node_event
            .get(&node)
            .copied()
            .or(self.last_global_event);
        let close_event = self.apply(SimAction::ConnectionLifecycle {
            node,
            peer,
            local_did,
            peer_did,
            generation,
            state: SimConnectionState::Closed,
            causal_parent: parent,
        });
        self.active_generations.remove(&(node, peer));
        self.last_node_event.insert(node, close_event);
    }

    pub(super) fn observe_peer_removal_if_due(&mut self, observer: SimNodeId, departed: SimNodeId) {
        if self.state.has_disconnected(observer, departed) {
            self.close_edge_if_active(observer, departed);
            return;
        }
        let Some(failure_event) = self.state.failure_event(observer, departed) else {
            return;
        };
        let disconnect_event = self.apply(SimAction::Disconnect {
            node: observer,
            peer: departed,
            causal_parent: Some(failure_event),
        });
        self.pending_repair_intent
            .insert(observer, disconnect_event);
        self.last_node_event.insert(observer, disconnect_event);
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
        let local_did = self.node_did(node).to_owned();
        let peer_did = self.node_did(peer).to_owned();
        let identity = SimFrameIdentity {
            local_did,
            peer_did,
            connection_generation: delivery.connection_generation.clone(),
            transaction_id: delivery.transaction_id,
        };
        let parent = self
            .last_node_event
            .get(&node)
            .copied()
            .unwrap_or(lifecycle_event);
        let submit_event = self.apply(SimAction::SubmitFrame {
            transfer_id: delivery.sequence,
            node,
            class,
            identity: identity.clone(),
            causal_parent: Some(parent),
        });
        let admit_event = self.apply(SimAction::AdmitFrame {
            transfer_id: delivery.sequence,
            node,
            class,
            bytes: u64::try_from(delivery.bytes).expect("frame bytes must fit u64"),
            enqueued_virtual_ms: delivery.enqueued_virtual_ms,
            deadline_virtual_ms: delivery.deadline_virtual_ms,
            identity,
            causal_parent: Some(submit_event),
        });
        let dispatch_event = self.apply(SimAction::DispatchFrame {
            transfer_id: delivery.sequence,
            causal_parent: Some(admit_event),
        });
        let deliver_parent = if class == SimTransferClass::Reassembly {
            self.apply(SimAction::AdvanceReassembly {
                node,
                transfer_id: delivery.sequence,
                causal_parent: Some(dispatch_event),
            })
        } else {
            dispatch_event
        };
        let deliver_event = self.apply(SimAction::DeliverFrame {
            transfer_id: delivery.sequence,
            causal_parent: Some(deliver_parent),
        });
        self.last_node_event.insert(node, deliver_event);
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
                    let parent = self
                        .last_node_event
                        .get(&node)
                        .copied()
                        .or(self.last_global_event);
                    let persist_event = self.apply(SimAction::PersistOneEntry {
                        node,
                        causal_parent: parent,
                    });
                    self.last_node_event.insert(node, persist_event);
                }
                ProductionTraceObservation::YieldActor { node_did } => {
                    let node = self.node_id(&node_did);
                    let parent = self
                        .last_node_event
                        .get(&node)
                        .copied()
                        .or(self.last_global_event);
                    let yield_event = self.apply(SimAction::YieldActor {
                        node,
                        causal_parent: parent,
                    });
                    self.last_node_event.insert(node, yield_event);
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
        if let Some(intent_event) = self.pending_repair_intent.remove(&node) {
            let repair_event = self.apply(SimAction::ScheduleRepair {
                node,
                entries: 0,
                causal_parent: Some(intent_event),
            });
            self.last_node_event.insert(node, repair_event);
        }
        let parent = self
            .last_node_event
            .get(&node)
            .copied()
            .or(self.last_global_event);
        let event = self.apply(SimAction::RunMaintenance {
            node,
            outcome,
            repair_cursor,
            causal_parent: parent,
        });
        self.last_node_event.insert(node, event);
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
