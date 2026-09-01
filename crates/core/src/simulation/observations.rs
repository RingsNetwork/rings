//! Production-boundary observations collected by deterministic simulations.

use super::ProtectionLayer;
use super::RUNTIME;

/// Ordered facts emitted directly by production storage actor boundaries.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) enum ProductionTraceObservation {
    /// One durable join completed for the named local node.
    PersistOneEntry { node_did: String },
    /// The storage actor yielded after that durable effect.
    YieldActor { node_did: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct DeadlineMissWitness {
    pub(crate) observed_virtual_ms: u64,
    pub(crate) deadline_virtual_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
struct CapacityMetric {
    peak: usize,
    limit: usize,
}

impl CapacityMetric {
    fn observe(&mut self, current: usize, limit: usize) {
        self.peak = self.peak.max(current);
        self.limit = limit;
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub(crate) struct ProductionCapacityObservations {
    outbound_peer_transfers: CapacityMetric,
    outbound_peer_bytes: CapacityMetric,
    outbound_global_bytes: CapacityMetric,
    inbound_peer_transfers: CapacityMetric,
    inbound_peer_bytes: CapacityMetric,
    inbound_node_transfers: CapacityMetric,
    inbound_node_bytes: CapacityMetric,
    reassembly_node_bytes: CapacityMetric,
    reassembly_peer_bytes: CapacityMetric,
    reassembly_pending_messages: CapacityMetric,
}

impl ProductionCapacityObservations {
    /// Require every storm-relevant metric to be observed and within its production limit.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let metrics = [
            ("outbound_peer_transfers", self.outbound_peer_transfers),
            ("outbound_peer_bytes", self.outbound_peer_bytes),
            ("outbound_global_bytes", self.outbound_global_bytes),
            ("inbound_peer_transfers", self.inbound_peer_transfers),
            ("inbound_peer_bytes", self.inbound_peer_bytes),
            ("inbound_node_transfers", self.inbound_node_transfers),
            ("inbound_node_bytes", self.inbound_node_bytes),
            ("reassembly_node_bytes", self.reassembly_node_bytes),
            ("reassembly_peer_bytes", self.reassembly_peer_bytes),
            (
                "reassembly_pending_messages",
                self.reassembly_pending_messages,
            ),
        ];
        for (name, metric) in metrics {
            if metric.peak == 0 || metric.limit == 0 {
                return Err(format!(
                    "production capacity metric {name} was not observed"
                ));
            }
            if metric.peak > metric.limit {
                return Err(format!(
                    "production capacity metric {name} exceeded its limit: {} > {}",
                    metric.peak, metric.limit
                ));
            }
        }
        Ok(())
    }
}

/// Record an observed protection failure at a production effect boundary.
pub(crate) fn record_protection_violation(layer: ProtectionLayer) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime.observations.record(layer);
        }
    });
}

/// Record that the production inbound barrier blocked a control lane.
pub(crate) fn record_barrier_control_blocked() {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime.observations.record_barrier_blocked();
        }
    });
}

/// Combine a real production barrier block with an explicit scheduler deadline miss.
pub(crate) fn record_barrier_control_deadline_miss(
    observed_virtual_ms: u64,
    deadline_virtual_ms: u64,
) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime
                .observations
                .record_barrier_deadline_miss(DeadlineMissWitness {
                    observed_virtual_ms,
                    deadline_virtual_ms,
                });
        }
    });
}

/// Record repair entries only after a production repair delivery is admitted.
pub(crate) fn record_repair_entries(entries: usize) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime.repair_entries_observed = runtime
                .repair_entries_observed
                .saturating_add(entries as u64);
        }
    });
}

/// Record an accepted chunk at the real reassembly boundary.
pub(crate) fn record_reassembly_advance(transaction_id: uuid::Uuid) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            let advances = runtime
                .reassembly_advances
                .entry(transaction_id)
                .or_default();
            *advances = advances.saturating_add(1);
        }
    });
}

/// Record one completed durable storage effect.
pub(crate) fn record_storage_persisted(node: crate::dht::Did) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime.production_trace_observations.push(
                ProductionTraceObservation::PersistOneEntry {
                    node_did: node.to_string(),
                },
            );
        }
    });
}

/// Record the production cooperative yield following a durable effect.
pub(crate) fn record_storage_actor_yield(node: crate::dht::Did) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime
                .production_trace_observations
                .push(ProductionTraceObservation::YieldActor {
                    node_did: node.to_string(),
                });
        }
    });
}

/// Wake independently scheduled work after the first real persistence effect.
pub(crate) fn signal_storage_progress_probe() -> Option<u64> {
    RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        let runtime = runtime.as_ref()?;
        runtime.storage_progress_notify.as_ref()?.notify_one();
        Some(runtime.storage_progress_epoch)
    })
}

/// Record that the independently scheduled storage-progress observer ran.
pub(crate) fn record_storage_progress() {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime.storage_progress_epoch = runtime.storage_progress_epoch.saturating_add(1);
        }
    });
}

/// Record a witness sampled after the actor yield and before the next entry is persisted.
pub(crate) fn record_storage_progress_between_entries() {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime
                .observations
                .record_storage_progress_between_entries();
        }
    });
}

/// Read the cooperative observer epoch without creating a synthetic wakeup.
pub(crate) fn storage_progress_epoch() -> Option<u64> {
    RUNTIME.with(|runtime| {
        runtime
            .borrow()
            .as_ref()
            .map(|runtime| runtime.storage_progress_epoch)
    })
}

/// Record the first production outbound submission time for a transaction.
pub(crate) fn record_outbound_submission(transaction_id: uuid::Uuid) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            let elapsed_ms = u64::try_from(runtime.elapsed_ms).unwrap_or(u64::MAX);
            runtime
                .outbound_submission_ms
                .entry(transaction_id)
                .or_insert(elapsed_ms);
        }
    });
}

/// Observe peer-scoped production outbound transfer and byte peaks.
pub(crate) fn observe_outbound_peer_capacity(
    transfers: usize,
    transfer_limit: usize,
    bytes: usize,
    byte_limit: usize,
) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime
                .capacity_observations
                .outbound_peer_transfers
                .observe(transfers, transfer_limit);
            runtime
                .capacity_observations
                .outbound_peer_bytes
                .observe(bytes, byte_limit);
        }
    });
}

/// Observe node-scoped production outbound retained bytes.
pub(crate) fn observe_outbound_global_capacity(bytes: usize, limit: usize) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            runtime
                .capacity_observations
                .outbound_global_bytes
                .observe(bytes, limit);
        }
    });
}

/// Observe peer and node production inbound count/byte peaks.
pub(crate) fn observe_inbound_capacity(
    peer: (usize, usize, usize, usize),
    node: (usize, usize, usize, usize),
) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            let observations = &mut runtime.capacity_observations;
            observations.inbound_peer_transfers.observe(peer.0, peer.2);
            observations.inbound_peer_bytes.observe(peer.1, peer.3);
            observations.inbound_node_transfers.observe(node.0, node.2);
            observations.inbound_node_bytes.observe(node.1, node.3);
        }
    });
}

/// Observe node, peer, and pending-message production reassembly peaks.
pub(crate) fn observe_reassembly_capacity(
    node_bytes: usize,
    node_limit: usize,
    peer_bytes: usize,
    peer_limit: usize,
    pending_messages: usize,
    pending_limit: usize,
) {
    RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().as_mut() {
            let observations = &mut runtime.capacity_observations;
            observations
                .reassembly_node_bytes
                .observe(node_bytes, node_limit);
            observations
                .reassembly_peer_bytes
                .observe(peer_bytes, peer_limit);
            observations
                .reassembly_pending_messages
                .observe(pending_messages, pending_limit);
        }
    });
}
