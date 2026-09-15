//! Overlay reachability probe and HTTP redial over a live [`Processor`].
//!
//! Reachability is a routed Chord lookup, not a direct-edge check. The probe sends
//! `FindSuccessorSend { did: t, strict: false }` toward the target `t` and accepts `t` as
//! reachable iff the report that returns under the same transaction id names `t` itself as the
//! successor of its own identifier. In a Chord ring every present node is the successor of its
//! own identifier, so
//!
//! ```text
//!   reachable(t)  ⟺  is_peer_connected(t)  ∨  report(tx).successor = t
//! ```
//!
//! A partition that lost `t` answers with the node now succeeding `t`'s position, and a node
//! with no overlay at all cannot route the request; both read as unreachable. The report is
//! authored by `t`'s predecessor, so the probe trusts on-path nodes exactly as far as Chord
//! routing already does. A direct edge short-circuits the probe, so a target that is a plain
//! neighbour is never redialed merely because it is not a finger.
//!
//! Reports reach the node through [`BackendObserver::lookup_report`]; the ledger below is the
//! rendezvous between a probe awaiting its report and the report arriving, in either order:
//!
//! ```text
//!   send ─▶ tx ─▶ await_report(tx) ─┬─ early report buffered? ─▶ resolved at once
//!                                   └─ else register waiter ─▶ observe(tx, s) ─▶ resolved
//!   observe(tx, s) with no waiter ─▶ buffered as an early report (bounded FIFO)
//!   forget(tx) ─▶ waiter dropped (after the probe timeout)
//! ```
//!
//! [`BackendObserver::lookup_report`]: crate::extension::BackendObserver::lookup_report

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::message::FindSuccessorReportHandler;
use rings_core::message::FindSuccessorSend;
use rings_core::message::FindSuccessorThen;
use rings_core::message::Message;
use rings_rpc::protos::rings_node::ConnectPeerViaHttpRequest;
use rings_rpc::protos::rings_node::ConnectPeerViaHttpResponse;
use rings_rpc::protos::rings_node_handler::HandleRpc;
use tokio::sync::oneshot;

use super::BootstrapPort;
use super::ManagedTarget;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;
use crate::sync_lock::lock;

/// Longest a routed lookup probe waits for its report before the target counts as unreachable.
pub(crate) const LOOKUP_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Reports retained while no probe has registered for them, so a report that overtakes the
/// return path of its own send is not lost. Older entries are evicted first.
pub(crate) const EARLY_REPORT_CAPACITY: usize = 32;

/// Rendezvous between lookup probes and the reports they wait for.
#[derive(Default)]
pub struct LookupReportLedger {
    state: Mutex<LedgerState>,
}

/// Ledger state: one waiter per outstanding transaction and a bounded FIFO of early reports.
#[derive(Default)]
struct LedgerState {
    awaiting: HashMap<uuid::Uuid, oneshot::Sender<Did>>,
    early: VecDeque<(uuid::Uuid, Did)>,
}

impl LookupReportLedger {
    /// Deliver the report for `tx_id`: to its waiter when one is registered, otherwise into
    /// the early buffer.
    pub fn observe(&self, tx_id: uuid::Uuid, successor: Did) -> Result<()> {
        let mut state = lock(&self.state)?;
        match state.awaiting.remove(&tx_id) {
            // A waiter that already timed out and dropped its receiver is simply satisfied late.
            Some(waiter) => drop(waiter.send(successor)),
            None => {
                if state.early.len() >= EARLY_REPORT_CAPACITY {
                    state.early.pop_front();
                }
                state.early.push_back((tx_id, successor));
            }
        }
        Ok(())
    }

    /// Await the report for `tx_id`; resolves at once when the report arrived first.
    pub fn await_report(&self, tx_id: uuid::Uuid) -> Result<oneshot::Receiver<Did>> {
        let mut state = lock(&self.state)?;
        let (sender, receiver) = oneshot::channel();
        let early = state
            .early
            .iter()
            .position(|(early_tx_id, _)| *early_tx_id == tx_id)
            .and_then(|position| state.early.remove(position));
        match early {
            Some((_, successor)) => drop(sender.send(successor)),
            None => drop(state.awaiting.insert(tx_id, sender)),
        }
        Ok(receiver)
    }

    /// Drop the waiter for `tx_id`, once its probe has given up.
    pub fn forget(&self, tx_id: uuid::Uuid) -> Result<()> {
        lock(&self.state)?.awaiting.remove(&tx_id);
        Ok(())
    }

    /// Number of outstanding waiters plus buffered early reports.
    #[cfg(test)]
    pub(crate) fn len(&self) -> Result<usize> {
        let state = lock(&self.state)?;
        Ok(state.awaiting.len() + state.early.len())
    }
}

/// [`BootstrapPort`] over a live processor: routed probe plus HTTP handshake.
pub struct ProcessorPort {
    processor: Arc<Processor>,
    reports: Arc<LookupReportLedger>,
}

impl ProcessorPort {
    /// A port whose probes rendezvous with reports in `reports`.
    pub fn new(processor: Arc<Processor>, reports: Arc<LookupReportLedger>) -> Self {
        Self { processor, reports }
    }
}

#[async_trait]
impl BootstrapPort for ProcessorPort {
    /// A direct `Connected` transport short-circuits; otherwise a routed lookup decides.
    async fn reachable(&self, target: &ManagedTarget) -> bool {
        if self.processor.swarm.is_peer_connected(target.did()) {
            return true;
        }
        lookup_reaches(self.processor.as_ref(), self.reports.as_ref(), target.did()).await
    }

    /// Run the `connectPeerViaHttp` handshake against the endpoint and pin the answering DID.
    async fn dial(&self, target: &ManagedTarget) -> Result<()> {
        let request = ConnectPeerViaHttpRequest {
            url: target.url().to_owned(),
            api_token: target.api_token().map(str::to_owned),
        };
        let response: ConnectPeerViaHttpResponse =
            HandleRpc::handle_rpc(self.processor.as_ref(), request)
                .await
                .map_err(|error| Error::BootstrapHandshake(error.to_string()))?;
        let expected = target.did();
        if response.did != expected.to_string() {
            return Err(Error::BootstrapDidMismatch {
                expected,
                actual: response.did,
            });
        }
        Ok(())
    }
}

/// Whether a routed successor lookup for `target` reports `target` itself within
/// [`LOOKUP_PROBE_TIMEOUT`].
///
/// A send that cannot even leave the node (no successor, no route) reads as unreachable, as does
/// a report naming any other successor or no report at all.
async fn lookup_reaches(processor: &Processor, reports: &LookupReportLedger, target: Did) -> bool {
    let request = Message::FindSuccessorSend(FindSuccessorSend {
        did: target,
        strict: false,
        then: FindSuccessorThen::Report(FindSuccessorReportHandler::None),
    });
    let tx_id = match processor.swarm.send_message(request, target).await {
        Ok(tx_id) => tx_id,
        Err(error) => {
            tracing::debug!(%target, %error, "bootstrap lookup probe could not be routed");
            return false;
        }
    };
    let waiter = match reports.await_report(tx_id) {
        Ok(waiter) => waiter,
        Err(error) => {
            tracing::error!(%target, %error, "bootstrap lookup ledger unavailable");
            return false;
        }
    };
    let outcome = tokio::time::timeout(LOOKUP_PROBE_TIMEOUT, waiter).await;
    if let Err(error) = reports.forget(tx_id) {
        tracing::error!(%target, %error, "bootstrap lookup ledger unavailable");
    }
    match outcome {
        Ok(Ok(successor)) => successor == target,
        Ok(Err(_)) | Err(_) => false,
    }
}
