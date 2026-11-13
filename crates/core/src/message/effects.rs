//! Explicit effects emitted by Core message handlers.
//!
//! This module is the adapter-first boundary for moving handlers away from
//! directly calling transport/DHT APIs. Handlers describe values in
//! [`CoreEffect`], and [`CoreEffectInterpreter`] applies those values to the
//! current transport implementation.

#[cfg(all(feature = "wasm", target_family = "wasm"))]
use std::cell::Cell;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
use futures::channel::oneshot;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
use wasm_bindgen::closure::Closure;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
use wasm_bindgen::JsCast;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
use wasm_bindgen::JsValue;

use crate::dht::entry::Entry;
use crate::dht::entry::PlacementMiss;
use crate::dht::Did;
use crate::dht::FingerFixRequest;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Error;
use crate::error::Result;
use crate::message::handlers::inbox::hold_for_offline_destination;
use crate::message::handlers::storage::admit_storage_lookup_reply;
use crate::message::types::FindSuccessorSend;
use crate::message::types::OperationDigest;
use crate::message::types::StoreOutcome;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::Principal;
use crate::message::QueryForTopoInfoSend;
use crate::swarm::callback::SwarmTransportCallback;
use crate::swarm::observer::MessageActivity;
use crate::swarm::observer::MessageObservation;
use crate::swarm::observer::ObservationOutcome;
use crate::swarm::transport::SwarmTransport;

/// Yield one executor poll without depending on a particular async runtime.
async fn yield_executor_once() {
    let mut yielded = false;
    poll_fn(move |context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) const CORE_ACTOR_BROWSER_YIELD_INTERVAL: u8 = 32;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
thread_local! {
    static CORE_ACTOR_STEPS_SINCE_BROWSER_YIELD: Cell<u8> = const { Cell::new(0) };
    #[cfg(test)]
    static LIVE_BROWSER_TASK_YIELD_GUARDS: Cell<usize> = const { Cell::new(0) };
    #[cfg(test)]
    static CLEARED_BROWSER_TASK_YIELD_HANDLERS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
struct BrowserTaskYieldGuard {
    channel: web_sys::MessageChannel,
    _callback: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
impl BrowserTaskYieldGuard {
    fn new(
        channel: web_sys::MessageChannel,
        callback: Closure<dyn FnMut(web_sys::MessageEvent)>,
    ) -> Self {
        channel
            .port1()
            .set_onmessage(Some(callback.as_ref().unchecked_ref()));
        #[cfg(test)]
        LIVE_BROWSER_TASK_YIELD_GUARDS.with(|live| live.set(live.get().saturating_add(1)));
        Self {
            channel,
            _callback: callback,
        }
    }

    fn post(&self) -> std::result::Result<(), JsValue> {
        self.channel.port2().post_message(&JsValue::NULL)
    }
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
impl Drop for BrowserTaskYieldGuard {
    fn drop(&mut self) {
        self.channel.port1().set_onmessage(None);
        self.channel.port1().close();
        self.channel.port2().close();
        #[cfg(test)]
        {
            LIVE_BROWSER_TASK_YIELD_GUARDS.with(|live| live.set(live.get().saturating_sub(1)));
            CLEARED_BROWSER_TASK_YIELD_HANDLERS
                .with(|cleared| cleared.set(cleared.get().saturating_add(1)));
        }
    }
}

/// Yield after one bounded core actor work item.
///
/// Native tasks yield for one executor poll. Browser tasks do the same cheap
/// yield and additionally cross a `MessageChannel` task boundary every
/// `CORE_ACTOR_BROWSER_YIELD_INTERVAL` steps, bounding event-loop starvation
/// without the nested-timer clamp of `setTimeout(0)`.
pub(crate) async fn yield_core_actor_step() {
    yield_executor_once().await;
    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    if browser_task_yield_due() {
        yield_browser_task().await;
    }
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn browser_task_yield_due() -> bool {
    CORE_ACTOR_STEPS_SINCE_BROWSER_YIELD.with(|steps| {
        let next = steps.get().saturating_add(1);
        if next >= CORE_ACTOR_BROWSER_YIELD_INTERVAL {
            steps.set(0);
            true
        } else {
            steps.set(next);
            false
        }
    })
}

#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(crate) async fn yield_browser_task() {
    let Ok(channel) = web_sys::MessageChannel::new() else {
        return;
    };
    let (sender, receiver) = oneshot::channel();
    let mut sender = Some(sender);
    let callback = Closure::wrap(Box::new(move |_event: web_sys::MessageEvent| {
        if let Some(sender) = sender.take() {
            let _ = sender.send(());
        }
    }) as Box<dyn FnMut(_)>);
    let guard = BrowserTaskYieldGuard::new(channel, callback);
    if guard.post().is_err() {
        return;
    }
    let _ = receiver.await;
}

#[cfg(all(test, feature = "wasm", target_family = "wasm"))]
pub(crate) fn reset_browser_task_yield_guard_counts_for_test() {
    LIVE_BROWSER_TASK_YIELD_GUARDS.with(|live| live.set(0));
    CLEARED_BROWSER_TASK_YIELD_HANDLERS.with(|cleared| cleared.set(0));
}

#[cfg(all(test, feature = "wasm", target_family = "wasm"))]
pub(crate) fn browser_task_yield_guard_counts_for_test() -> (usize, usize) {
    (
        LIVE_BROWSER_TASK_YIELD_GUARDS.with(Cell::get),
        CLEARED_BROWSER_TASK_YIELD_HANDLERS.with(Cell::get),
    )
}

/// Pair each work item with whether another item follows it.
pub(crate) fn core_actor_steps<T>(
    items: impl IntoIterator<Item = T>,
) -> impl Iterator<Item = (T, bool)> {
    let mut items = items.into_iter().peekable();
    std::iter::from_fn(move || {
        let item = items.next()?;
        Some((item, items.peek().is_some()))
    })
}

/// Parameters for [`CoreEffect::AdmitStorageLookupReply`], boxed to keep [`CoreEffect`] compact.
#[derive(Clone, Debug)]
pub(crate) struct AdmitStorageLookupReplyEffect {
    /// Looked-up resource DID.
    pub(crate) resource: Did,
    /// Redundancy of the lookup round.
    pub(crate) redundancy: u16,
    /// Single validated entry returned by the responder, if any.
    pub(crate) found_entry: Option<Entry>,
    /// Placement misses reported along the lookup path.
    pub(crate) misses: Vec<PlacementMiss>,
    /// Accounting principal of the edge that delivered the reply.
    pub(crate) introducer: Principal,
    /// Handler clock timestamp at which cache admission and liveness are evaluated.
    pub(crate) now_ms: u128,
}

/// One side effect requested by a Core message handler.
#[derive(Clone, Debug)]
pub(crate) enum CoreEffect<'payload> {
    /// Forward an existing payload one hop further along its Chord route.
    ForwardPayload {
        /// Payload to forward.
        payload: &'payload MessagePayload,
        /// Optional explicit next hop. `None` preserves current DHT inference.
        next_hop: Option<Did>,
    },
    /// Send a report message using the original request payload.
    SendReportMessage {
        /// Request payload to report against.
        payload: &'payload MessagePayload,
        /// Report message to send.
        msg: Box<Message>,
    },
    /// Reset a relayed payload to a new destination/next-hop.
    ResetDestination {
        /// Payload to relay after resetting destination.
        payload: &'payload MessagePayload,
        /// New destination and next hop.
        next_hop: Did,
    },
    /// Send a message using normal next-hop inference.
    SendMessage {
        /// Message to send.
        msg: Box<Message>,
        /// Final destination.
        destination: Did,
    },
    /// Send a message directly to the destination as the next hop.
    SendDirectMessage {
        /// Message to send.
        msg: Box<Message>,
        /// Direct destination and next hop.
        destination: Did,
    },
    /// Register and send one successor-list query that must be answered with
    /// the same request id.
    ///
    /// The DHT claim is part of the effect, not the message constructor,
    /// because a request id should become admissible only when the transport
    /// actually attempts to send the query to the current successor.
    SendSuccessorQuery {
        /// Topology query whose identity authorizes one successor-sync report.
        ///
        /// The interpreter registers `query.request_id` before moving this
        /// value onto the transport, so an immediate authenticated response can
        /// claim the exact request without a registration race.
        query: QueryForTopoInfoSend,
        /// Current successor that owns the registered response token.
        ///
        /// This DID is both the direct transport destination and the reporter
        /// expected by the DHT claim. A successor change invalidates the claim
        /// before any later report can create connection effects.
        destination: Did,
    },
    /// Establish an idempotent DHT-driven transport connection.
    ConnectDhtPeer {
        /// Peer to connect.
        peer: Did,
    },
    /// Hold an application payload in the relay inbox of its offline destination.
    HoldForOfflineDestination {
        /// Payload whose destination this node is responsible for but cannot reach.
        payload: &'payload MessagePayload,
        /// Handler clock timestamp at which the hold is signed and stamped.
        now_ms: u128,
    },
    /// Request a storage repair round: the placement interval changed, so the stabilizer's next
    /// repair pass must hand off what lies beyond the new successor head. Only the intent is
    /// recorded here; the pass itself runs under the repair schedule, whose admission grace
    /// outlives the peer's own admission of this node.
    RequestStorageRepair,
    /// Admit a validated `FoundEntry` storage-lookup reply into the local cache, miss ledger, and
    /// read-repair queue at `now_ms`.
    AdmitStorageLookupReply(Box<AdmitStorageLookupReplyEffect>),
    /// Advance a pending reported-store quorum receipt with one placement owner's outcome.
    RecordStoreReport {
        /// Digest of the stamped operation.
        operation: OperationDigest,
        /// Placement owner reporting the outcome.
        placement: Did,
        /// Stored or refused outcome.
        outcome: StoreOutcome,
    },
    /// Classify and admit a correlated `FixFingerTable` lookup report, initiating a detached dial
    /// when the candidate awaits an owner.
    AdmitFingerCandidate {
        /// Reported successor DID.
        did: Did,
        /// Correlated finger-fix request token.
        request: FingerFixRequest,
        /// Whether `did` names a remote peer rather than this node.
        reports_remote: bool,
    },
    /// Claim a correlated `Connect` lookup token and initiate a detached dial when authorized.
    ClaimConnectCandidate {
        /// Reported successor DID.
        did: Did,
        /// Correlated connect-lookup request id.
        request_id: uuid::Uuid,
        /// Whether `did` names a remote peer rather than this node.
        reports_remote: bool,
    },
    /// Resolve an awaiting application-layer successor lookup handle.
    AnswerApplicationLookup {
        /// Transaction id of the answered lookup.
        tx_id: uuid::Uuid,
        /// Resolved successor DID.
        did: Did,
    },
}

impl<'payload> CoreEffect<'payload> {
    /// Create a payload-forwarding effect.
    pub(crate) fn forward_payload(
        payload: &'payload MessagePayload,
        next_hop: Option<Did>,
    ) -> Self {
        Self::ForwardPayload { payload, next_hop }
    }

    /// Create a report-message effect.
    pub(crate) fn send_report_message(payload: &'payload MessagePayload, msg: Message) -> Self {
        Self::SendReportMessage {
            payload,
            msg: Box::new(msg),
        }
    }

    /// Create a destination-reset effect.
    pub(crate) fn reset_destination(payload: &'payload MessagePayload, next_hop: Did) -> Self {
        Self::ResetDestination { payload, next_hop }
    }

    /// Create an effect that holds `payload` in its destination's relay inbox at `now_ms`.
    pub(crate) const fn hold_for_offline_destination_at(
        payload: &'payload MessagePayload,
        now_ms: u128,
    ) -> Self {
        Self::HoldForOfflineDestination { payload, now_ms }
    }

    /// Create a normally-routed send effect.
    pub(crate) fn send_message(msg: Message, destination: Did) -> Self {
        Self::SendMessage {
            msg: Box::new(msg),
            destination,
        }
    }

    /// Create a direct send effect.
    pub(crate) fn send_direct_message(msg: Message, destination: Did) -> Self {
        Self::SendDirectMessage {
            msg: Box::new(msg),
            destination,
        }
    }

    /// Create a successor-list query whose claim is registered at interpretation time.
    ///
    /// Construction is pure: it stores `query` and `destination` without
    /// mutating DHT state. [`CoreEffectInterpreter`] later registers the exact
    /// request before sending it and cancels that registration if transport
    /// delivery fails.
    pub(crate) const fn send_successor_query(
        query: QueryForTopoInfoSend,
        destination: Did,
    ) -> Self {
        Self::SendSuccessorQuery { query, destination }
    }

    /// Create a DHT connection effect.
    pub(crate) const fn connect_dht_peer(peer: Did) -> Self {
        Self::ConnectDhtPeer { peer }
    }

    /// Create a storage repair request effect.
    pub(crate) const fn request_storage_repair() -> Self {
        Self::RequestStorageRepair
    }

    /// Create a storage-lookup reply admission effect.
    pub(crate) fn admit_storage_lookup_reply(
        resource: Did,
        redundancy: u16,
        found_entry: Option<Entry>,
        misses: Vec<PlacementMiss>,
        introducer: Principal,
        now_ms: u128,
    ) -> Self {
        Self::AdmitStorageLookupReply(Box::new(AdmitStorageLookupReplyEffect {
            resource,
            redundancy,
            found_entry,
            misses,
            introducer,
            now_ms,
        }))
    }

    /// Create a store-report receipt update effect.
    pub(crate) const fn record_store_report(
        operation: OperationDigest,
        placement: Did,
        outcome: StoreOutcome,
    ) -> Self {
        Self::RecordStoreReport {
            operation,
            placement,
            outcome,
        }
    }

    /// Create a correlated finger-candidate admission effect.
    pub(crate) const fn admit_finger_candidate(
        did: Did,
        request: FingerFixRequest,
        reports_remote: bool,
    ) -> Self {
        Self::AdmitFingerCandidate {
            did,
            request,
            reports_remote,
        }
    }

    /// Create a correlated connect-lookup candidate claim effect.
    pub(crate) const fn claim_connect_candidate(
        did: Did,
        request_id: uuid::Uuid,
        reports_remote: bool,
    ) -> Self {
        Self::ClaimConnectCandidate {
            did,
            request_id,
            reports_remote,
        }
    }

    /// Create an application-lookup resolution effect.
    pub(crate) const fn answer_application_lookup(tx_id: uuid::Uuid, did: Did) -> Self {
        Self::AnswerApplicationLookup { tx_id, did }
    }
}

fn find_successor_effect<'payload>(
    next: Did,
    did: Did,
    handler: FindSuccessorReportHandler,
) -> Option<CoreEffect<'payload>> {
    (next != did).then(|| {
        CoreEffect::send_direct_message(
            Message::FindSuccessorSend(FindSuccessorSend {
                did,
                strict: false,
                then: FindSuccessorThen::Report(handler),
            }),
            next,
        )
    })
}

/// Lower one DHT leaf action directly into a transport effect.
#[allow(dead_code)]
pub(crate) fn lower_dht_action<'payload>(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
) -> Result<Option<CoreEffect<'payload>>> {
    lower_dht_action_with_id(act, is_connected, crate::utils::new_uuid)
}

/// Lower one DHT leaf action directly into a transport effect, drawing any fresh request
/// identifiers from `next_uuid`.
pub(crate) fn lower_dht_action_with_id<'payload>(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
    mut next_uuid: impl FnMut() -> uuid::Uuid,
) -> Result<Option<CoreEffect<'payload>>> {
    match act {
        PeerRingAction::None => Ok(None),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did)) => {
            Ok(find_successor_effect(
                *next,
                *did,
                FindSuccessorReportHandler::Connect {
                    request_id: next_uuid(),
                },
            ))
        }
        PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::FindSuccessorForFix { did, request },
        ) => Ok(find_successor_effect(
            *next,
            *did,
            FindSuccessorReportHandler::FixFingerTable { request: *request },
        )),
        PeerRingAction::RemoteAction(successor, PeerRingRemoteAction::QueryForSuccessorList) => {
            Ok(Some(if is_connected(*successor) {
                CoreEffect::send_successor_query(
                    QueryForTopoInfoSend::new_for_sync_with_id(*successor, next_uuid()),
                    *successor,
                )
            } else {
                CoreEffect::connect_dht_peer(*successor)
            }))
        }
        PeerRingAction::StorageRepairDue => Ok(Some(CoreEffect::request_storage_repair())),
        PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)) => {
            let (target, predecessor) = (*target, *predecessor);
            Ok(if target == predecessor {
                None
            } else if is_connected(target) {
                Some(CoreEffect::send_message(
                    Message::NotifyPredecessorSend(NotifyPredecessorSend { did: predecessor }),
                    target,
                ))
            } else {
                Some(CoreEffect::connect_dht_peer(target))
            })
        }
        act => Err(Error::unexpected_peer_ring_action(act.clone())),
    }
}

/// Interpreter from `CoreEffect` into the current transport implementation.
pub(crate) struct CoreEffectInterpreter<'handler> {
    transport: &'handler Arc<SwarmTransport>,
}

impl<'handler> CoreEffectInterpreter<'handler> {
    /// Create an interpreter over the current swarm transport.
    pub(crate) fn new(transport: &'handler Arc<SwarmTransport>) -> Self {
        Self { transport }
    }

    fn connection_is_satisfied(&self, peer: Did) -> bool {
        peer == self.transport.dht.did || self.transport.get_connection(peer).is_some()
    }

    /// Interpret one `CoreEffect`.
    ///
    /// Every send goes through the transport's
    /// protocol egress (`SwarmTransport::protocol_egress`): an effect runs inside the
    /// inbound event or maintenance step that emitted it, so it returns once its payload is
    /// queued and never waits on a remote peer (the inbound-locality law of
    /// `swarm::transport::egress`).
    pub(crate) async fn run<'payload>(&self, effect: CoreEffect<'payload>) -> Result<()> {
        let egress = self.transport.protocol_egress();
        match effect {
            CoreEffect::ForwardPayload { payload, next_hop } => {
                egress.forward_payload(payload, next_hop).await
            }
            CoreEffect::SendReportMessage { payload, msg } => {
                egress.send_report_message(payload, *msg).await
            }
            CoreEffect::ResetDestination { payload, next_hop } => {
                egress.reset_destination(payload, next_hop).await
            }
            CoreEffect::HoldForOfflineDestination { payload, now_ms } => {
                let message_kind =
                    crate::message::MessageKind::from_wire(&payload.transaction.data)?;
                let result =
                    hold_for_offline_destination(self.transport.clone(), payload, now_ms).await;
                self.transport.observe_message(MessageObservation {
                    activity: MessageActivity::Stored,
                    category: message_kind.class(),
                    message_class: message_kind.as_str(),
                    outcome: if result.is_ok() {
                        ObservationOutcome::Succeeded
                    } else {
                        ObservationOutcome::Failed
                    },
                });
                result
            }
            CoreEffect::RequestStorageRepair => {
                self.transport.request_storage_repair();
                Ok(())
            }
            CoreEffect::AdmitStorageLookupReply(reply) => {
                let AdmitStorageLookupReplyEffect {
                    resource,
                    redundancy,
                    found_entry,
                    misses,
                    introducer,
                    now_ms,
                } = *reply;
                admit_storage_lookup_reply(
                    self.transport.clone(),
                    resource,
                    redundancy,
                    found_entry,
                    misses,
                    introducer,
                    now_ms,
                )
                .await
            }
            CoreEffect::RecordStoreReport {
                operation,
                placement,
                outcome,
            } => {
                self.transport
                    .store_reports
                    .report(operation, placement, outcome);
                Ok(())
            }
            CoreEffect::AdmitFingerCandidate {
                did,
                request,
                reports_remote,
            } => {
                let disposition = self.transport.record_finger_candidate(did, request)?;
                if disposition.proof_awaits_owner() && reports_remote {
                    if self.transport.is_candidate_dial_backed_off(did) {
                        self.transport.dht.cancel_finger_lookup(request)?;
                        return Ok(());
                    }
                    if let Err(error) = self.connect_dht_peer(did).await {
                        self.transport.dht.cancel_finger_lookup(request)?;
                        return Err(error);
                    }
                    let admitted = self.transport.record_finger_candidate(did, request)?;
                    if admitted.proof_awaits_owner() {
                        self.transport.dht.cancel_finger_lookup(request)?;
                    }
                }
                Ok(())
            }
            CoreEffect::ClaimConnectCandidate {
                did,
                request_id,
                reports_remote,
            } => {
                if self.transport.dht.claim_connect_lookup(request_id)? && reports_remote {
                    self.connect_dht_peer(did).await?;
                }
                Ok(())
            }
            CoreEffect::AnswerApplicationLookup { tx_id, did } => {
                self.transport.application().lookups.answer(tx_id, did);
                Ok(())
            }
            CoreEffect::SendMessage { msg, destination } => {
                egress.send_message(*msg, destination).await?;
                Ok(())
            }
            CoreEffect::SendDirectMessage { msg, destination } => {
                let request_id = connect_lookup_request_id(&msg);
                self.with_connect_lookup_and_then(destination, request_id, || async move {
                    egress.send_direct_message(*msg, destination).await?;
                    Ok(())
                })
                .await
            }
            CoreEffect::SendSuccessorQuery { query, destination } => {
                // Register the request id before the message is visible on the
                // network. A same-turn report can then be claimed deterministically.
                if !self
                    .transport
                    .dht
                    .begin_successor_sync(destination, query.request_id)?
                {
                    return Ok(());
                }
                if let Err(error) = egress
                    .send_direct_message(Message::QueryForTopoInfoSend(query), destination)
                    .await
                {
                    // The request id never left this node successfully, so no
                    // later report may spend it.
                    self.transport
                        .dht
                        .cancel_successor_sync(destination, query.request_id)?;
                    return Err(error);
                }
                Ok(())
            }
            CoreEffect::ConnectDhtPeer { peer } => self.connect_dht_peer(peer).await,
        }
    }

    async fn connect_dht_peer(&self, peer: Did) -> Result<()> {
        if self.connection_is_satisfied(peer) || self.transport.is_candidate_dial_backed_off(peer) {
            return Ok(());
        }

        let callback = SwarmTransportCallback::new(Arc::clone(self.transport));
        match self.transport.connect_detached(peer, callback).await {
            Ok(()) | Err(Error::AlreadyConnected) => Ok(()),
            Err(
                error @ (Error::PendingConnectionCapacityExceeded { .. }
                | Error::ConnectionCapacityExceeded { .. }),
            ) => {
                tracing::debug!(
                    peer = %peer,
                    error = %error,
                    "connection capacity is full; skipping DHT candidate"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Interpret effects in order and fail on the first execution error.
    pub(crate) async fn run_all<'payload>(
        &self,
        effects: impl IntoIterator<Item = CoreEffect<'payload>>,
    ) -> Result<()> {
        for (effect, has_next) in core_actor_steps(effects) {
            self.run(effect).await?;
            if has_next {
                yield_core_actor_step().await;
            }
        }
        Ok(())
    }

    /// Registers an admission connect-lookup token before executing `send`, cancelling it if
    /// `send` fails before leaving the node.
    async fn with_connect_lookup_and_then<F, Fut>(
        &self,
        destination: Did,
        request_id: Option<uuid::Uuid>,
        send: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        request_id
            .map(|id| self.transport.dht.begin_connect_lookup(destination, id))
            .transpose()?;
        match send().await {
            Ok(()) => Ok(()),
            Err(error) => {
                request_id
                    .map(|id| self.transport.dht.cancel_connect_lookup(destination, id))
                    .transpose()?;
                Err(error)
            }
        }
    }
}

fn connect_lookup_request_id(msg: &Message) -> Option<uuid::Uuid> {
    match msg {
        Message::FindSuccessorSend(FindSuccessorSend {
            then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect { request_id }),
            ..
        }) => Some(*request_id),
        _ => None,
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod test_inbound_locality;
#[cfg(test)]
mod tests;
