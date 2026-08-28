//! Typed production observations and deterministic trace construction.

use super::*;

pub(super) struct TraceDriver {
    pub(super) state: SimState,
    root: SimEventId,
    pub(super) last_event: SimEventId,
    admissions: BTreeMap<u64, (SimEventId, SimNodeId)>,
    dispatches: BTreeMap<u64, (SimEventId, SimNodeId)>,
    delivered_controls: BTreeMap<(String, uuid::Uuid), SimEventId>,
    pub(super) endpoints: BTreeMap<String, (String, String)>,
    node_ids: BTreeMap<String, SimNodeId>,
    last_node_event: BTreeMap<SimNodeId, SimEventId>,
    failure: FailureState,
}

impl TraceDriver {
    pub(super) fn new(
        expected_entries: usize,
        endpoints: BTreeMap<String, (String, String)>,
        node_ids: BTreeMap<String, SimNodeId>,
        initial_virtual_ms: u64,
        failure: FailureState,
    ) -> Self {
        let (state, output) = SimState::default()
            .transition(SimAction::InjectSync {
                node: SimNodeId(0),
                entries: expected_entries as u64,
            })
            .expect("sync-storm root action must be valid");
        let mut driver = Self {
            state,
            root: output.event_id,
            last_event: output.event_id,
            admissions: BTreeMap::new(),
            dispatches: BTreeMap::new(),
            delivered_controls: BTreeMap::new(),
            endpoints,
            node_ids,
            last_node_event: BTreeMap::new(),
            failure,
        };
        if initial_virtual_ms > 0 {
            driver.advance_virtual(initial_virtual_ms);
        }
        driver.observe_lifecycle(SimConnectionState::Active);
        driver.refresh_failure();
        driver
    }

    pub(super) fn observe_pending(
        &mut self,
        runtime: &SimulationRuntimeGuard,
        deliveries: &[ScheduledDelivery],
    ) {
        for delivery in deliveries {
            let Some(class) = model_class(delivery.class) else {
                continue;
            };
            if self.admissions.contains_key(&delivery.sequence) {
                continue;
            }
            let transaction_id = delivery
                .transaction_id
                .expect("production frame must retain its transaction identity");
            assert_eq!(
                runtime
                    .outbound_submission_ms(transaction_id)
                    .expect("production submission observation must remain visible"),
                Some(delivery.enqueued_virtual_ms),
                "SubmitFrame must originate at the real outbound submission boundary"
            );
            let bytes = u64::try_from(delivery.bytes).expect("wire size must fit u64");
            let (local_did, peer_did) = self
                .endpoints
                .get(&delivery.connection_generation)
                .cloned()
                .expect("queued generation must belong to the established topology");
            let node = *self
                .node_ids
                .get(&local_did)
                .expect("queued local DID must identify one scenario node");
            let causal_parent = self.node_parent(node);
            let identity = SimFrameIdentity {
                local_did,
                peer_did,
                connection_generation: delivery.connection_generation.clone(),
                transaction_id: delivery.transaction_id,
            };
            let (next, submitted) = self
                .take_state()
                .transition(SimAction::SubmitFrame {
                    transfer_id: delivery.sequence,
                    node,
                    class,
                    identity: identity.clone(),
                    causal_parent: Some(causal_parent),
                })
                .expect("production submission observation must be valid");
            self.state = next;
            let (next, output) = self
                .take_state()
                .transition(SimAction::AdmitFrame {
                    transfer_id: delivery.sequence,
                    node,
                    class,
                    bytes,
                    enqueued_virtual_ms: delivery.enqueued_virtual_ms,
                    deadline_virtual_ms: delivery.deadline_virtual_ms,
                    identity,
                    causal_parent: Some(submitted.event_id),
                })
                .expect("queue observation must be structurally valid");
            self.state = next;
            self.last_event = output.event_id;
            self.last_node_event.insert(node, output.event_id);
            self.admissions
                .insert(delivery.sequence, (output.event_id, node));
            self.refresh_failure();
        }
    }

    pub(super) fn observe_delivery(
        &mut self,
        runtime: &SimulationRuntimeGuard,
        delivery: &ScheduledDelivery,
    ) {
        let Some((_, node)) = self.admissions.get(&delivery.sequence).copied() else {
            return;
        };
        let dispatched = self.observe_dispatch(delivery);
        let parent = self.observe_reassembly(runtime, delivery, node, dispatched);
        let (state, output) = self
            .take_state()
            .transition(SimAction::DeliverFrame {
                transfer_id: delivery.sequence,
                causal_parent: Some(parent),
            })
            .expect("delivery must match its dispatched frame");
        self.state = state;
        if delivery.class == ScheduledDeliveryClass::Control {
            let transaction_id = delivery
                .transaction_id
                .expect("delivered control must retain its transaction identity");
            self.delivered_controls.insert(
                (delivery.connection_generation.clone(), transaction_id),
                output.event_id,
            );
        }
        self.admissions.remove(&delivery.sequence);
        self.dispatches.remove(&delivery.sequence);
        self.record_node_event(node, output.event_id);
    }

    pub(super) fn observe_dispatch(&mut self, delivery: &ScheduledDelivery) -> SimEventId {
        if let Some((event, _)) = self.dispatches.get(&delivery.sequence) {
            return *event;
        }
        let (parent, node) = self
            .admissions
            .get(&delivery.sequence)
            .copied()
            .expect("dispatch must have an observed production admission");
        let (state, dispatched) = self
            .take_state()
            .transition(SimAction::DispatchFrame {
                transfer_id: delivery.sequence,
                causal_parent: Some(parent),
            })
            .expect("dispatch must match its observed queue admission");
        self.state = state;
        self.dispatches
            .insert(delivery.sequence, (dispatched.event_id, node));
        self.record_node_event(node, dispatched.event_id);
        dispatched.event_id
    }

    fn observe_reassembly(
        &mut self,
        runtime: &SimulationRuntimeGuard,
        delivery: &ScheduledDelivery,
        node: SimNodeId,
        parent: SimEventId,
    ) -> SimEventId {
        if delivery.class != ScheduledDeliveryClass::Reassembly {
            return parent;
        }
        let transaction_id = delivery
            .transaction_id
            .expect("real reassembly frame must retain its transaction identity");
        assert!(
            runtime
                .take_reassembly_advance_observed(transaction_id)
                .expect("production reassembly observation must remain visible"),
            "trace cannot synthesize a reassembly advance for {transaction_id}"
        );
        let (state, advanced) = self
            .take_state()
            .transition(SimAction::AdvanceReassembly {
                node,
                transfer_id: delivery.sequence,
                causal_parent: Some(parent),
            })
            .expect("reassembly advance must follow its dispatched frame");
        self.state = state;
        advanced.event_id
    }

    pub(super) fn observe_production_handlers(&mut self, runtime: &SimulationRuntimeGuard) {
        let observations = runtime
            .take_production_trace_observations()
            .expect("production handler observations must remain visible");
        for observation in observations {
            let (node_did, persist) = match observation {
                crate::simulation::ProductionTraceObservation::PersistOneEntry { node_did } => {
                    (node_did, true)
                }
                crate::simulation::ProductionTraceObservation::YieldActor { node_did } => {
                    (node_did, false)
                }
            };
            let node = *self
                .node_ids
                .get(&node_did)
                .expect("production storage node must belong to the scenario");
            let parent = self.node_parent(node);
            let action = if persist {
                SimAction::PersistOneEntry {
                    node,
                    causal_parent: Some(parent),
                }
            } else {
                SimAction::YieldActor {
                    node,
                    causal_parent: Some(parent),
                }
            };
            let (state, output) = self
                .take_state()
                .transition(action)
                .expect("production storage observation must be structurally valid");
            self.state = state;
            self.record_node_event(node, output.event_id);
        }
    }

    pub(super) fn observe_barrier(
        &mut self,
        control: &ScheduledDelivery,
        blocked_control: bool,
    ) -> SimEventId {
        let (local, _) = self
            .endpoints
            .get(&control.connection_generation)
            .expect("barrier control generation must belong to the scenario");
        let node = *self
            .node_ids
            .get(local)
            .expect("barrier control local DID must map");
        let parent = self
            .dispatches
            .get(&control.sequence)
            .map(|(event, _)| *event)
            .expect("barrier verdict must follow exact control dispatch");
        let (state, output) = self
            .take_state()
            .transition(SimAction::ObserveReassemblyBarrier {
                node,
                control_transfer_id: control.sequence,
                generation: control.connection_generation.clone(),
                transaction_id: control.transaction_id,
                blocked_control,
                causal_parent: Some(parent),
            })
            .expect("reassembly barrier observation must be valid");
        self.state = state;
        self.record_node_event(node, output.event_id);
        output.event_id
    }

    pub(super) fn advance_virtual(&mut self, delta_ms: u64) {
        let (state, output) = self
            .take_state()
            .transition(SimAction::AdvanceVirtualTime { delta_ms })
            .expect("model virtual time must advance");
        self.state = state;
        self.last_event = output.event_id;
        self.refresh_failure();
    }

    pub(super) fn advance_virtual_to(&mut self, virtual_ms: u64) {
        let current = self.state.snapshot().virtual_ms;
        self.advance_virtual(virtual_ms.saturating_sub(current));
    }

    pub(super) fn stop_storm(&mut self) {
        let node = SimNodeId(0);
        let parent = self.node_parent(node);
        let (state, output) = self
            .take_state()
            .transition(SimAction::StopStorm {
                node,
                causal_parent: Some(parent),
            })
            .expect("storm stop observation must be valid");
        self.state = state;
        self.record_node_event(node, output.event_id);
    }

    pub(super) fn observe_lifecycle(&mut self, state: SimConnectionState) {
        for (generation, (local_did, peer_did)) in self.endpoints.clone() {
            let node = *self
                .node_ids
                .get(&local_did)
                .expect("local lifecycle DID must map");
            let peer = *self
                .node_ids
                .get(&peer_did)
                .expect("peer lifecycle DID must map");
            let parent = self.node_parent(node);
            let (next, output) = self
                .take_state()
                .transition(SimAction::ConnectionLifecycle {
                    node,
                    peer,
                    local_did,
                    peer_did,
                    generation,
                    state,
                    causal_parent: Some(parent),
                })
                .expect("connection lifecycle observation must be valid");
            self.state = next;
            self.record_node_event(node, output.event_id);
        }
    }

    pub(super) fn observe_liveness(
        &mut self,
        current: &BTreeMap<String, (String, String)>,
        probes: &BTreeMap<(String, String), uuid::Uuid>,
    ) {
        for (generation, (local, peer)) in self.endpoints.clone() {
            let node = *self
                .node_ids
                .get(&local)
                .expect("local liveness DID must map");
            let peer_node = *self
                .node_ids
                .get(&peer)
                .expect("peer liveness DID must map");
            self.observe_liveness_verdict(
                node,
                peer_node,
                generation.clone(),
                probes.get(&(local, peer)).copied(),
                !current.contains_key(&generation),
                None,
            );
        }
    }

    pub(super) fn observe_liveness_verdict(
        &mut self,
        node: SimNodeId,
        peer: SimNodeId,
        generation: String,
        transaction_id: Option<uuid::Uuid>,
        removed: bool,
        causal_parent: Option<SimEventId>,
    ) -> SimEventId {
        let parent = causal_parent.unwrap_or_else(|| {
            let (local, peer_did) = self
                .endpoints
                .get(&generation)
                .expect("liveness generation must belong to the topology");
            let probe_generation = self.endpoints.iter().find_map(|(candidate, endpoints)| {
                (endpoints == &(peer_did.clone(), local.clone())).then_some(candidate)
            });
            transaction_id
                .zip(probe_generation)
                .and_then(|(transaction, probe_generation)| {
                    self.delivered_controls
                        .get(&(probe_generation.clone(), transaction))
                        .copied()
                })
                .expect("healthy liveness verdict must name its exact delivered probe")
        });
        let (state, output) = self
            .take_state()
            .transition(SimAction::RunLiveness {
                node,
                peer,
                generation,
                transaction_id,
                removed,
                causal_parent: Some(parent),
            })
            .expect("liveness verdict observation must be valid");
        self.state = state;
        self.record_node_event(node, output.event_id);
        output.event_id
    }

    pub(super) fn observe_maintenance(
        &mut self,
        node: usize,
        outcome: SimMaintenanceOutcome,
        repair_cursor: Option<String>,
    ) {
        let node = SimNodeId(u16::try_from(node).expect("node index must fit trace id"));
        let parent = self.node_parent(node);
        let (state, output) = self
            .take_state()
            .transition(SimAction::RunMaintenance {
                node,
                outcome,
                repair_cursor,
                causal_parent: Some(parent),
            })
            .expect("maintenance observation must be valid");
        self.state = state;
        self.record_node_event(node, output.event_id);
    }

    pub(super) fn record_node_event(&mut self, node: SimNodeId, event: SimEventId) {
        self.last_event = event;
        self.last_node_event.insert(node, event);
        self.refresh_failure();
    }

    fn node_parent(&self, node: SimNodeId) -> SimEventId {
        self.last_node_event
            .get(&node)
            .copied()
            .unwrap_or(self.root)
    }

    fn refresh_failure(&self) {
        self.failure.borrow_mut().observe(&self.state);
    }

    pub(super) fn take_state(&mut self) -> SimState {
        std::mem::take(&mut self.state)
    }
}
