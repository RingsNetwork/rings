//! Checked pure transitions for the sync-storm model.

use super::*;

struct AdmitObservation<'a> {
    transfer_id: u64,
    node: SimNodeId,
    class: SimTransferClass,
    bytes: u64,
    enqueued_at_ms: u64,
    deadline_virtual_ms: Option<u64>,
    identity: &'a SimFrameIdentity,
    parent: Option<SimEventId>,
}

struct BarrierObservation<'a> {
    node: SimNodeId,
    control_transfer_id: u64,
    generation: &'a str,
    transaction_id: Option<uuid::Uuid>,
    blocked: bool,
    parent: Option<SimEventId>,
}

struct LivenessObservation<'a> {
    node: SimNodeId,
    peer: SimNodeId,
    generation: &'a str,
    transaction_id: Option<uuid::Uuid>,
    removed: bool,
    parent: Option<SimEventId>,
}

impl SimState {
    /// Apply one action without side effects, returning the next state and output.
    pub(crate) fn transition(
        mut self,
        action: SimAction,
    ) -> Result<(Self, SimOutput), SimTransitionError> {
        let event_id = SimEventId(self.next_event);
        validate_parent(action.causal_parent(), event_id)?;
        let event_node = action
            .explicit_node()
            .or_else(|| self.transfer_node(&action));
        self.next_event = checked_add(self.next_event, 1)?;
        let node_sequence = event_node
            .map(|node| self.allocate_node_sequence(node))
            .transpose()?;
        let control_latency_ms = self.apply(&action, event_id)?;
        let output = SimOutput {
            event_id,
            control_latency_ms,
        };
        self.trace.events.push(SimTraceEvent {
            id: event_id,
            node_sequence,
            virtual_ms: self.virtual_ms,
            causal_parent: action.causal_parent(),
            action,
            output: output.clone(),
        });
        Ok((self, output))
    }

    fn allocate_node_sequence(&mut self, node: SimNodeId) -> Result<u64, SimTransitionError> {
        let sequence = self.next_node_sequence.entry(node).or_default();
        let current = *sequence;
        *sequence = checked_add(*sequence, 1)?;
        Ok(current)
    }

    fn transfer_node(&self, action: &SimAction) -> Option<SimNodeId> {
        let transfer_id = match action {
            SimAction::DispatchFrame { transfer_id, .. }
            | SimAction::DeliverFrame { transfer_id, .. } => *transfer_id,
            _ => return None,
        };
        self.submitted
            .get(&transfer_id)
            .map(|transfer| transfer.node)
            .or_else(|| self.pending.get(&transfer_id).map(|transfer| transfer.node))
            .or_else(|| {
                self.in_flight
                    .get(&transfer_id)
                    .map(|transfer| transfer.pending.node)
            })
    }

    fn apply(
        &mut self,
        action: &SimAction,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        match action {
            SimAction::InjectSync { entries, .. } => self.inject_sync(*entries),
            SimAction::AdvanceVirtualTime { delta_ms } => self.advance_virtual(*delta_ms),
            SimAction::SubmitFrame { .. }
            | SimAction::AdmitFrame { .. }
            | SimAction::DispatchFrame { .. }
            | SimAction::DeliverFrame { .. }
            | SimAction::AdvanceReassembly { .. }
            | SimAction::ObserveReassemblyBarrier { .. } => {
                self.apply_frame_action(action, event_id)
            }
            SimAction::PersistOneEntry { node, .. } => self.persist(*node, event_id),
            SimAction::YieldActor {
                node,
                causal_parent,
            } => self.actor_yield(*node, *causal_parent),
            SimAction::ConnectionLifecycle {
                node,
                peer,
                local_did,
                peer_did,
                generation,
                state,
                ..
            } => self.lifecycle(*node, *peer, local_did, peer_did, generation, *state),
            SimAction::RunLiveness {
                node,
                peer,
                generation,
                transaction_id,
                removed,
                causal_parent,
            } => self.liveness(
                LivenessObservation {
                    node: *node,
                    peer: *peer,
                    generation,
                    transaction_id: *transaction_id,
                    removed: *removed,
                    parent: *causal_parent,
                },
                event_id,
            ),
            SimAction::RunMaintenance {
                node,
                outcome,
                repair_cursor,
                ..
            } => self.maintenance(*node, *outcome, repair_cursor),
            SimAction::StopStorm { .. } => self.stop_storm(),
            SimAction::InjectPeerFailure { node, peer } => {
                self.inject_failure(*node, *peer, event_id)
            }
            SimAction::Disconnect {
                node,
                peer,
                causal_parent,
            } => self.disconnect(*node, *peer, *causal_parent, event_id),
            SimAction::ScheduleRepair {
                node,
                entries,
                causal_parent,
            } => self.schedule_repair(*node, *entries, *causal_parent),
        }
    }

    fn apply_frame_action(
        &mut self,
        action: &SimAction,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        match action {
            SimAction::SubmitFrame {
                transfer_id,
                node,
                class,
                identity,
                ..
            } => self.submit(*transfer_id, *node, *class, identity, event_id),
            SimAction::AdmitFrame {
                transfer_id,
                node,
                class,
                bytes,
                enqueued_virtual_ms,
                deadline_virtual_ms,
                identity,
                causal_parent,
            } => self.admit(
                AdmitObservation {
                    transfer_id: *transfer_id,
                    node: *node,
                    class: *class,
                    bytes: *bytes,
                    enqueued_at_ms: *enqueued_virtual_ms,
                    deadline_virtual_ms: *deadline_virtual_ms,
                    identity,
                    parent: *causal_parent,
                },
                event_id,
            ),
            SimAction::DispatchFrame {
                transfer_id,
                causal_parent,
            } => self.dispatch(*transfer_id, *causal_parent, event_id),
            SimAction::DeliverFrame {
                transfer_id,
                causal_parent,
            } => self.deliver(*transfer_id, *causal_parent, event_id),
            SimAction::AdvanceReassembly {
                node,
                transfer_id,
                causal_parent,
            } => self.advance_reassembly(*node, *transfer_id, *causal_parent, event_id),
            SimAction::ObserveReassemblyBarrier {
                node,
                control_transfer_id,
                generation,
                transaction_id,
                blocked_control,
                causal_parent,
            } => self.observe_barrier(
                BarrierObservation {
                    node: *node,
                    control_transfer_id: *control_transfer_id,
                    generation,
                    transaction_id: *transaction_id,
                    blocked: *blocked_control,
                    parent: *causal_parent,
                },
                event_id,
            ),
            _ => unreachable!("only frame actions are delegated"),
        }
    }

    fn inject_sync(&mut self, entries: u64) -> Result<Option<u64>, SimTransitionError> {
        if self.storm_stopped {
            return Err(SimTransitionError::ActionAfterStop {
                action: "InjectSync",
            });
        }
        self.initial_entries = checked_add(self.initial_entries, entries)?;
        Ok(None)
    }

    fn advance_virtual(&mut self, delta_ms: u64) -> Result<Option<u64>, SimTransitionError> {
        self.virtual_ms = self
            .virtual_ms
            .checked_add(delta_ms)
            .ok_or(SimTransitionError::TimeOverflow)?;
        Ok(None)
    }

    fn submit(
        &mut self,
        transfer_id: u64,
        node: SimNodeId,
        class: SimTransferClass,
        identity: &SimFrameIdentity,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        if self.storm_stopped {
            return Err(SimTransitionError::ActionAfterStop {
                action: "SubmitFrame",
            });
        }
        if self.transfer_exists(transfer_id) {
            return Err(SimTransitionError::DuplicateTransfer { transfer_id });
        }
        if !self.frame_generation_active(node, identity) {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id,
                expected: "active-generation",
                observed: "inactive-generation",
            });
        }
        if let Some(transaction_id) = identity.transaction_id {
            let evidence = self
                .frame_transactions
                .entry(transaction_id)
                .or_default()
                .entry((identity.connection_generation.clone(), class))
                .or_default();
            evidence.submitted = checked_add(evidence.submitted, 1)?;
        }
        self.submitted.insert(transfer_id, SubmittedTransfer {
            node,
            class,
            identity: identity.clone(),
            event_id,
        });
        Ok(None)
    }

    fn admit(
        &mut self,
        observation: AdmitObservation<'_>,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let submitted = self.submitted.remove(&observation.transfer_id).ok_or(
            SimTransitionError::InvalidTransferPhase {
                transfer_id: observation.transfer_id,
                expected: "submitted",
                observed: "missing",
            },
        )?;
        require_parent(observation.parent, submitted.event_id)?;
        if submitted.node != observation.node
            || submitted.class != observation.class
            || submitted.identity != *observation.identity
        {
            return Err(SimTransitionError::FrameIdentityMismatch {
                transfer_id: observation.transfer_id,
            });
        }
        if !self.frame_generation_active(observation.node, observation.identity) {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id: observation.transfer_id,
                expected: "active-generation",
                observed: "closed-generation",
            });
        }
        let queue = (
            observation.node,
            observation.identity.connection_generation.clone(),
            observation.class,
        );
        self.outbound_queues
            .entry(queue)
            .or_default()
            .insert(observation.transfer_id);
        self.reserve_bytes(observation.node, observation.class, observation.bytes)?;
        self.pending
            .insert(observation.transfer_id, PendingTransfer {
                node: observation.node,
                class: observation.class,
                bytes: observation.bytes,
                enqueued_at_ms: observation.enqueued_at_ms,
                deadline_virtual_ms: observation.deadline_virtual_ms,
                identity: observation.identity.clone(),
                admission_event: event_id,
            });
        Ok(None)
    }

    fn dispatch(
        &mut self,
        transfer_id: u64,
        parent: Option<SimEventId>,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let pending = self
            .pending
            .remove(&transfer_id)
            .ok_or(SimTransitionError::UnknownTransfer { transfer_id })?;
        require_parent(parent, pending.admission_event)?;
        if !self.frame_generation_active(pending.node, &pending.identity) {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id,
                expected: "active-generation",
                observed: "closed-generation",
            });
        }
        let queue = (
            pending.node,
            pending.identity.connection_generation.clone(),
            pending.class,
        );
        remove_from_index(&mut self.outbound_queues, &queue, transfer_id);
        self.inbound_lanes
            .entry((pending.node, pending.class))
            .or_default()
            .insert(transfer_id);
        self.in_flight.insert(transfer_id, DispatchedTransfer {
            pending,
            dispatch_event: event_id,
            reassembly_event: None,
        });
        Ok(None)
    }

    fn advance_reassembly(
        &mut self,
        node: SimNodeId,
        transfer_id: u64,
        parent: Option<SimEventId>,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let generations = &self.connection_generations;
        let transfer = self
            .in_flight
            .get_mut(&transfer_id)
            .ok_or(SimTransitionError::UnknownTransfer { transfer_id })?;
        if !frame_generation_active(
            generations,
            transfer.pending.node,
            &transfer.pending.identity,
        ) {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id,
                expected: "active-generation",
                observed: "closed-generation",
            });
        }
        if transfer.pending.node != node
            || transfer.pending.class != SimTransferClass::Reassembly
            || transfer.reassembly_event.is_some()
        {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id,
                expected: "dispatched-reassembly",
                observed: "different-phase",
            });
        }
        require_parent(parent, transfer.dispatch_event)?;
        transfer.reassembly_event = Some(event_id);
        self.reassembly_advances = checked_add(self.reassembly_advances, 1)?;
        Ok(None)
    }

    fn deliver(
        &mut self,
        transfer_id: u64,
        parent: Option<SimEventId>,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let transfer = self
            .in_flight
            .remove(&transfer_id)
            .ok_or(SimTransitionError::UnknownTransfer { transfer_id })?;
        if !self.frame_generation_active(transfer.pending.node, &transfer.pending.identity) {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id,
                expected: "active-generation",
                observed: "closed-generation",
            });
        }
        let expected_parent = if transfer.pending.class == SimTransferClass::Reassembly {
            transfer
                .reassembly_event
                .ok_or(SimTransitionError::InvalidTransferPhase {
                    transfer_id,
                    expected: "reassembly-advanced",
                    observed: "dispatched",
                })?
        } else {
            transfer.dispatch_event
        };
        require_parent(parent, expected_parent)?;
        remove_from_index(
            &mut self.inbound_lanes,
            &(transfer.pending.node, transfer.pending.class),
            transfer_id,
        );
        self.release_bytes(&transfer.pending);
        if let Some(transaction_id) = transfer.pending.identity.transaction_id {
            let evidence = self
                .frame_transactions
                .get_mut(&transaction_id)
                .and_then(|frames| {
                    frames.get_mut(&(
                        transfer.pending.identity.connection_generation.clone(),
                        transfer.pending.class,
                    ))
                })
                .ok_or(SimTransitionError::UnknownTransfer { transfer_id })?;
            evidence.delivered = checked_add(evidence.delivered, 1)?;
            evidence.delivered_event = Some(event_id);
        }
        self.record_delivery(&transfer.pending)
    }

    fn observe_barrier(
        &mut self,
        observation: BarrierObservation<'_>,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let Some(control) = self.in_flight.get(&observation.control_transfer_id) else {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id: observation.control_transfer_id,
                expected: "dispatched-control",
                observed: "missing",
            });
        };
        require_parent(observation.parent, control.dispatch_event)?;
        let valid_control = control.pending.node == observation.node
            && control.pending.class == SimTransferClass::Control
            && control.pending.identity.connection_generation == observation.generation
            && control.pending.identity.transaction_id == observation.transaction_id
            && self.frame_generation_active(observation.node, &control.pending.identity);
        if !valid_control {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id: observation.control_transfer_id,
                expected: "active-control-frame",
                observed: "identity-mismatch",
            });
        }
        let has_reassembly_backlog = self.in_flight.values().any(|transfer| {
            transfer.pending.node == observation.node
                && transfer.pending.class == SimTransferClass::Reassembly
                && transfer.dispatch_event < control.dispatch_event
                && transfer.reassembly_event.is_none()
        });
        if !has_reassembly_backlog {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id: observation.control_transfer_id,
                expected: "earlier-in-flight-reassembly",
                observed: "missing",
            });
        }
        self.reassembly_barriers = checked_add(self.reassembly_barriers, 1)?;
        if self
            .barrier_events
            .insert(
                observation.control_transfer_id,
                (event_id, observation.blocked),
            )
            .is_some()
        {
            return Err(SimTransitionError::InvalidTransferPhase {
                transfer_id: observation.control_transfer_id,
                expected: "first-barrier-verdict",
                observed: "duplicate",
            });
        }
        if observation.blocked {
            self.blocked_control_barriers = checked_add(self.blocked_control_barriers, 1)?;
        }
        Ok(None)
    }

    fn persist(
        &mut self,
        node: SimNodeId,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        self.persisted_entries = checked_add(self.persisted_entries, 1)?;
        self.unyielded_persistence.insert(node, event_id);
        Ok(None)
    }

    fn actor_yield(
        &mut self,
        node: SimNodeId,
        parent: Option<SimEventId>,
    ) -> Result<Option<u64>, SimTransitionError> {
        let persist_event = self
            .unyielded_persistence
            .remove(&node)
            .ok_or(SimTransitionError::YieldWithoutPersistence { node })?;
        require_parent(parent, persist_event)?;
        self.actor_yields = checked_add(self.actor_yields, 1)?;
        Ok(None)
    }

    fn lifecycle(
        &mut self,
        node: SimNodeId,
        peer: SimNodeId,
        local_did: &str,
        peer_did: &str,
        generation: &str,
        state: SimConnectionState,
    ) -> Result<Option<u64>, SimTransitionError> {
        let edge = (node, peer);
        let valid = match (self.connection_generations.get(&edge), state) {
            (None, SimConnectionState::Active) => !self
                .connection_generations
                .values()
                .any(|known| known.generation == generation),
            (Some(known), SimConnectionState::Closed) => {
                matches!(
                    known.state,
                    SimConnectionState::Active | SimConnectionState::Removed
                ) && known.generation == generation
                    && known.local_did == local_did
                    && known.peer_did == peer_did
            }
            _ => false,
        };
        if !valid {
            return Err(SimTransitionError::InvalidLifecycle {
                node,
                peer,
                generation: generation.to_owned(),
            });
        }
        self.connection_generations
            .insert(edge, SimConnectionGeneration {
                generation: generation.to_owned(),
                local_did: local_did.to_owned(),
                peer_did: peer_did.to_owned(),
                state,
            });
        if state == SimConnectionState::Active {
            self.peer_health.insert(edge, SimPeerHealth::Healthy);
        }
        Ok(None)
    }

    fn liveness(
        &mut self,
        observation: LivenessObservation<'_>,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let edge = (observation.node, observation.peer);
        let active = self.connection_generations.get(&edge).is_some_and(|known| {
            known.generation == observation.generation && known.state == SimConnectionState::Active
        });
        let probe_evidence = observation.transaction_id.and_then(|transaction| {
            self.frame_transactions.get(&transaction)?.iter().find_map(
                |((frame_generation, class), evidence)| {
                    (self.generation_matches_edge(
                        observation.peer,
                        observation.node,
                        frame_generation,
                    ) && *class == SimTransferClass::Control)
                        .then_some((transaction, frame_generation.as_str(), evidence))
                },
            )
        });
        let probe_valid =
            probe_evidence.is_some_and(|(transaction, frame_generation, evidence)| {
                if observation.removed {
                    evidence.delivered == 0
                        && self.transaction_deadline_missed(transaction, frame_generation)
                } else {
                    evidence.delivered > 0 && evidence.delivered_event.is_some()
                }
            });
        if !active {
            return Err(SimTransitionError::InactiveGeneration {
                node: observation.node,
                peer: observation.peer,
                generation: observation.generation.to_owned(),
            });
        }
        if !probe_valid || self.liveness_verdicts.contains_key(&edge) {
            return Err(SimTransitionError::InvalidLivenessVerdict {
                node: observation.node,
                peer: observation.peer,
                generation: observation.generation.to_owned(),
            });
        }
        if observation.removed {
            let probe_transfer = observation.transaction_id.and_then(|transaction| {
                self.in_flight.iter().find_map(|(transfer_id, transfer)| {
                    (transfer.pending.identity.transaction_id == Some(transaction)
                        && transfer.pending.class == SimTransferClass::Control
                        && self.generation_matches_edge(
                            observation.peer,
                            observation.node,
                            &transfer.pending.identity.connection_generation,
                        ))
                    .then_some(*transfer_id)
                })
            });
            let barrier = probe_transfer.and_then(|transfer| self.barrier_events.get(&transfer));
            let expected = barrier
                .filter(|(_, blocked)| *blocked)
                .map(|(event, _)| *event)
                .ok_or(SimTransitionError::InvalidLivenessVerdict {
                    node: observation.node,
                    peer: observation.peer,
                    generation: observation.generation.to_owned(),
                })?;
            require_parent(observation.parent, expected)?;
            if let Some(connection) = self.connection_generations.get_mut(&edge) {
                connection.state = SimConnectionState::Removed;
            }
        } else {
            let expected = probe_evidence
                .and_then(|(_, _, evidence)| evidence.delivered_event)
                .ok_or(SimTransitionError::InvalidLivenessVerdict {
                    node: observation.node,
                    peer: observation.peer,
                    generation: observation.generation.to_owned(),
                })?;
            require_parent(observation.parent, expected)?;
        }
        self.liveness_verdicts
            .insert(edge, (observation.removed, event_id));
        self.peer_health.insert(
            edge,
            if observation.removed {
                SimPeerHealth::Removed
            } else {
                SimPeerHealth::Healthy
            },
        );
        Ok(None)
    }

    fn maintenance(
        &mut self,
        node: SimNodeId,
        outcome: SimMaintenanceOutcome,
        repair_cursor: &Option<String>,
    ) -> Result<Option<u64>, SimTransitionError> {
        if outcome == SimMaintenanceOutcome::Idle && self.repair_intents.contains_key(&node) {
            return Err(SimTransitionError::MissingRepairIntent { node });
        }
        if outcome == SimMaintenanceOutcome::Complete {
            self.repair_intents.remove(&node);
        }
        self.repair_cursors.insert(node, repair_cursor.clone());
        self.maintenance_runs = checked_add(self.maintenance_runs, 1)?;
        Ok(None)
    }

    fn stop_storm(&mut self) -> Result<Option<u64>, SimTransitionError> {
        if self.storm_stopped {
            return Err(SimTransitionError::ActionAfterStop {
                action: "StopStorm",
            });
        }
        self.storm_stopped = true;
        Ok(None)
    }

    fn inject_failure(
        &mut self,
        node: SimNodeId,
        peer: SimNodeId,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let edge = (node, peer);
        if !self
            .connection_generations
            .get(&edge)
            .is_some_and(|generation| generation.state == SimConnectionState::Active)
        {
            return Err(SimTransitionError::InvalidFailureInjection { node, peer });
        }
        self.injected_failures.insert(edge);
        self.failure_events.insert(edge, event_id);
        self.peer_health.insert(edge, SimPeerHealth::Failed);
        Ok(None)
    }

    fn disconnect(
        &mut self,
        node: SimNodeId,
        peer: SimNodeId,
        parent: Option<SimEventId>,
        event_id: SimEventId,
    ) -> Result<Option<u64>, SimTransitionError> {
        let edge = (node, peer);
        if !matches!(
            self.peer_health.get(&edge),
            Some(SimPeerHealth::Failed | SimPeerHealth::Removed)
        ) {
            return Err(SimTransitionError::InvalidDisconnect {
                node,
                peer,
                reason: super::SimDisconnectReason::HealthyPeer,
            });
        }
        let expected_parent = match self.peer_health.get(&edge) {
            Some(SimPeerHealth::Removed) => {
                self.liveness_verdicts.get(&edge).map(|(_, event)| *event)
            }
            Some(SimPeerHealth::Failed) => self.failure_events.get(&edge).copied(),
            _ => None,
        }
        .ok_or(SimTransitionError::InvalidDisconnect {
            node,
            peer,
            reason: super::SimDisconnectReason::MissingCause,
        })?;
        require_parent(parent, expected_parent)?;
        if !self.disconnects.insert(edge) {
            return Err(SimTransitionError::InvalidDisconnect {
                node,
                peer,
                reason: super::SimDisconnectReason::Duplicate,
            });
        }
        self.repair_intents.insert(node, event_id);
        Ok(None)
    }

    fn schedule_repair(
        &mut self,
        node: SimNodeId,
        entries: u64,
        parent: Option<SimEventId>,
    ) -> Result<Option<u64>, SimTransitionError> {
        let intent = self
            .repair_intents
            .remove(&node)
            .ok_or(SimTransitionError::MissingRepairIntent { node })?;
        require_parent(parent, intent)?;
        self.repair_entries = checked_add(self.repair_entries, entries)?;
        Ok(None)
    }

    fn reserve_bytes(
        &mut self,
        node: SimNodeId,
        class: SimTransferClass,
        bytes: u64,
    ) -> Result<(), SimTransitionError> {
        let node_bytes = self.current_node_bytes.entry(node).or_default();
        *node_bytes = checked_add(*node_bytes, bytes)?;
        let class_bytes = self.current_class_bytes.entry((node, class)).or_default();
        *class_bytes = checked_add(*class_bytes, bytes)?;
        let peak = self.peak_node_bytes.entry(node).or_default();
        *peak = (*peak).max(*node_bytes);
        self.current_global_bytes = checked_add(self.current_global_bytes, bytes)?;
        self.peak_global_bytes = self.peak_global_bytes.max(self.current_global_bytes);
        Ok(())
    }

    fn release_bytes(&mut self, pending: &PendingTransfer) {
        let node_bytes = self.current_node_bytes.entry(pending.node).or_default();
        *node_bytes = node_bytes.saturating_sub(pending.bytes);
        let class_bytes = self
            .current_class_bytes
            .entry((pending.node, pending.class))
            .or_default();
        *class_bytes = class_bytes.saturating_sub(pending.bytes);
        self.current_global_bytes = self.current_global_bytes.saturating_sub(pending.bytes);
    }

    fn record_delivery(
        &mut self,
        pending: &PendingTransfer,
    ) -> Result<Option<u64>, SimTransitionError> {
        let delivered = &mut self.class_deliveries[pending.class.index()];
        *delivered = checked_add(*delivered, 1)?;
        if pending.class != SimTransferClass::Control {
            return Ok(None);
        }
        let latency = self.virtual_ms.saturating_sub(pending.enqueued_at_ms);
        self.max_control_latency_ms = self.max_control_latency_ms.max(latency);
        if pending
            .deadline_virtual_ms
            .is_some_and(|deadline| self.virtual_ms > deadline)
        {
            let deadline = pending.deadline_virtual_ms.unwrap_or_default();
            let relative_deadline = deadline.saturating_sub(pending.enqueued_at_ms);
            self.max_missed_control_latency_ms = self
                .max_missed_control_latency_ms
                .max(Some((latency, relative_deadline)));
        }
        Ok(Some(latency))
    }

    fn transaction_deadline_missed(&self, transaction_id: uuid::Uuid, generation: &str) -> bool {
        self.pending
            .values()
            .chain(self.in_flight.values().map(|transfer| &transfer.pending))
            .any(|pending| {
                pending.identity.transaction_id == Some(transaction_id)
                    && pending.identity.connection_generation == generation
                    && pending.class == SimTransferClass::Control
                    && pending
                        .deadline_virtual_ms
                        .is_some_and(|deadline| self.virtual_ms > deadline)
            })
    }

    fn frame_generation_active(&self, node: SimNodeId, identity: &SimFrameIdentity) -> bool {
        frame_generation_active(&self.connection_generations, node, identity)
    }

    fn generation_matches_edge(&self, node: SimNodeId, peer: SimNodeId, generation: &str) -> bool {
        self.connection_generations
            .get(&(node, peer))
            .is_some_and(|known| {
                known.generation == generation && known.state == SimConnectionState::Active
            })
    }

    fn transfer_exists(&self, transfer_id: u64) -> bool {
        self.submitted.contains_key(&transfer_id)
            || self.pending.contains_key(&transfer_id)
            || self.in_flight.contains_key(&transfer_id)
    }
}

fn remove_from_index<K: Ord>(index: &mut BTreeMap<K, BTreeSet<u64>>, key: &K, transfer_id: u64) {
    if let Some(entries) = index.get_mut(key) {
        entries.remove(&transfer_id);
        if entries.is_empty() {
            index.remove(key);
        }
    }
}

fn require_parent(
    observed: Option<SimEventId>,
    expected: SimEventId,
) -> Result<(), SimTransitionError> {
    if observed == Some(expected) {
        Ok(())
    } else {
        Err(SimTransitionError::UnexpectedCausalParent { expected, observed })
    }
}

fn validate_parent(parent: Option<SimEventId>, next: SimEventId) -> Result<(), SimTransitionError> {
    if let Some(parent) = parent {
        if parent >= next {
            return Err(SimTransitionError::InvalidCausalParent { parent, next });
        }
    }
    Ok(())
}

fn frame_generation_active(
    generations: &BTreeMap<(SimNodeId, SimNodeId), super::SimConnectionGeneration>,
    node: SimNodeId,
    identity: &SimFrameIdentity,
) -> bool {
    generations.iter().any(|((local, _), known)| {
        *local == node
            && known.generation == identity.connection_generation
            && known.local_did == identity.local_did
            && known.peer_did == identity.peer_did
            && known.state == SimConnectionState::Active
    })
}

fn checked_add(left: u64, right: u64) -> Result<u64, SimTransitionError> {
    left.checked_add(right)
        .ok_or(SimTransitionError::CounterOverflow)
}
