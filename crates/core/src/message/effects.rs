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

use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Error;
use crate::error::Result;
use crate::message::handlers::inbox::hold_for_offline_destination;
use crate::message::types::FindSuccessorSend;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::QueryForTopoInfoSend;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SharedSwarmCallback;
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
    },
    /// Request a storage repair round: the placement interval changed, so the stabilizer's next
    /// repair pass must hand off what lies beyond the new successor head. Only the intent is
    /// recorded here; the pass itself runs under the repair schedule, whose admission grace
    /// outlives the peer's own admission of this node.
    RequestStorageRepair,
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

    /// Create an effect that holds `payload` in its destination's relay inbox.
    pub(crate) fn hold_for_offline_destination(payload: &'payload MessagePayload) -> Self {
        Self::HoldForOfflineDestination { payload }
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
pub(crate) fn lower_dht_action<'payload>(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
) -> Result<Option<CoreEffect<'payload>>> {
    match act {
        PeerRingAction::None => Ok(None),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did)) => {
            Ok(find_successor_effect(
                *next,
                *did,
                FindSuccessorReportHandler::Connect,
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
                    QueryForTopoInfoSend::new_for_sync(*successor),
                    *successor,
                )
            } else {
                CoreEffect::connect_dht_peer(*successor)
            }))
        }
        PeerRingAction::RemoteAction(peer, PeerRingRemoteAction::TryConnect) => {
            Ok(Some(CoreEffect::connect_dht_peer(*peer)))
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
    swarm_callback: &'handler SharedSwarmCallback,
}

impl<'handler> CoreEffectInterpreter<'handler> {
    /// Create an interpreter over the current swarm transport.
    pub(crate) fn new(
        transport: &'handler Arc<SwarmTransport>,
        swarm_callback: &'handler SharedSwarmCallback,
    ) -> Self {
        Self {
            transport,
            swarm_callback,
        }
    }

    fn connection_is_satisfied(&self, peer: Did) -> bool {
        peer == self.transport.dht.did || self.transport.get_connection(peer).is_some()
    }

    /// Interpret one `CoreEffect`, preserving the existing transport behavior.
    pub(crate) async fn run<'payload>(&self, effect: CoreEffect<'payload>) -> Result<()> {
        match effect {
            CoreEffect::ForwardPayload { payload, next_hop } => {
                self.transport.forward_payload(payload, next_hop).await
            }
            CoreEffect::SendReportMessage { payload, msg } => {
                self.transport.send_report_message(payload, *msg).await
            }
            CoreEffect::ResetDestination { payload, next_hop } => {
                self.transport.reset_destination(payload, next_hop).await
            }
            CoreEffect::HoldForOfflineDestination { payload } => {
                hold_for_offline_destination(self.transport.clone(), payload).await
            }
            CoreEffect::RequestStorageRepair => {
                self.transport.request_storage_repair();
                Ok(())
            }
            CoreEffect::SendMessage { msg, destination } => {
                self.transport.send_message(*msg, destination).await?;
                Ok(())
            }
            CoreEffect::SendDirectMessage { msg, destination } => {
                self.transport
                    .send_direct_message(*msg, destination)
                    .await?;
                Ok(())
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
                if let Err(error) = self
                    .transport
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
            CoreEffect::ConnectDhtPeer { peer } => {
                if self.connection_is_satisfied(peer) {
                    return Ok(());
                }

                let callback = InnerSwarmCallback::new(
                    Arc::clone(self.transport),
                    Arc::clone(self.swarm_callback),
                );
                match self.transport.connect(peer, callback).await {
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
}

#[cfg(test)]
mod tests {
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    use std::future::Future;
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    use std::sync::atomic::AtomicUsize;
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    use std::sync::atomic::Ordering;
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    use std::task::Context;
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    use std::task::Wake;
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    use std::task::Waker;

    use super::*;
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    use crate::dht::types::Chord;
    use crate::ecc::SecretKey;
    use crate::message::types::QueryFor;
    use crate::message::MessageSigner;
    use crate::session::SessionSk;
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    use crate::swarm::callback::SwarmCallback;
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    use crate::tests::default::prepare_node;
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    use crate::tests::default::wait_for_connection_state;
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    use crate::tests::manually_establish_connection;
    use crate::tests::TEST_NETWORK_ID;

    /// Callback fixture for tests that exercise only interpreter-owned transport effects.
    ///
    /// It intentionally implements no event behavior, ensuring assertions
    /// observe request registration, delivery, and cancellation rather than a
    /// callback-generated DHT transition.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    struct NoopCallback;

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    impl SwarmCallback for NoopCallback {}

    fn did() -> Did {
        SecretKey::random().address().into()
    }

    fn payload(destination: Did) -> Result<MessagePayload> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        MessagePayload::new_send(
            Message::custom(b"hello")?,
            MessageSigner::new(&session_sk, TEST_NETWORK_ID),
            destination,
            destination,
        )
    }

    fn single_effect<'payload>(
        effect: Result<Option<CoreEffect<'payload>>>,
    ) -> Result<CoreEffect<'payload>> {
        effect?.ok_or_else(|| Error::InvalidMessage("expected one effect".to_string()))
    }

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    struct WakeCounter(AtomicUsize);

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    #[test]
    fn test_core_actor_step_yields_for_exactly_one_poll() {
        let wake_counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(yield_core_actor_step());

        assert_eq!(Future::poll(future.as_mut(), &mut context), Poll::Pending);
        assert_eq!(wake_counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(Future::poll(future.as_mut(), &mut context), Poll::Ready(()));
        assert_eq!(wake_counter.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_core_actor_steps_marks_only_real_yield_boundaries() {
        assert_eq!(core_actor_steps([1, 2, 3]).collect::<Vec<_>>(), vec![
            (1, true),
            (2, true),
            (3, false),
        ]);
        assert_eq!(core_actor_steps(Vec::<u8>::new()).next(), None);
    }

    #[test]
    fn test_send_report_message_effect_borrows_payload_and_owns_message() -> Result<()> {
        let destination = did();
        let payload = payload(destination)?;
        let effect = CoreEffect::send_report_message(
            &payload,
            Message::NotifyPredecessorReport(crate::message::NotifyPredecessorReport {
                did: destination,
            }),
        );

        match effect {
            CoreEffect::SendReportMessage {
                payload: effect_payload,
                msg,
            } => {
                assert!(std::ptr::eq(effect_payload, &payload));
                match *msg {
                    Message::NotifyPredecessorReport(report) => assert_eq!(report.did, destination),
                    msg => {
                        return Err(Error::InvalidMessage(format!(
                            "expected NotifyPredecessorReport, got {msg:?}"
                        )))
                    }
                }
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendReportMessage, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_reset_destination_effect_borrows_payload_and_next_hop() -> Result<()> {
        let destination = did();
        let next_hop = did();
        let payload = payload(destination)?;
        let effect = CoreEffect::reset_destination(&payload, next_hop);

        match effect {
            CoreEffect::ResetDestination {
                payload: effect_payload,
                next_hop: effect_next_hop,
            } => {
                assert!(std::ptr::eq(effect_payload, &payload));
                assert_eq!(effect_next_hop, next_hop);
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected ResetDestination, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_storage_repair_due_lowers_to_a_repair_request() -> Result<()> {
        let effect = single_effect(lower_dht_action(&PeerRingAction::StorageRepairDue, |_| {
            false
        }))?;

        match effect {
            CoreEffect::RequestStorageRepair => Ok(()),
            effect => Err(Error::InvalidMessage(format!(
                "expected RequestStorageRepair, got {effect:?}"
            ))),
        }
    }

    #[test]
    fn test_dht_find_successor_for_connect_sends_direct_report() -> Result<()> {
        let next = did();
        let target = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForConnect(target),
            ),
            |_| true,
        ))?;

        match effect {
            CoreEffect::SendDirectMessage { msg, destination } => match *msg {
                Message::FindSuccessorSend(msg) => {
                    assert_eq!(destination, next);
                    assert_eq!(msg.did, target);
                    assert!(!msg.strict);
                    match msg.then {
                        FindSuccessorThen::Report(FindSuccessorReportHandler::Connect) => {}
                        handler => {
                            return Err(Error::InvalidMessage(format!(
                                "expected connect report handler, got {handler:?}"
                            )))
                        }
                    }
                }
                msg => {
                    return Err(Error::InvalidMessage(format!(
                        "expected FindSuccessorSend, got {msg:?}"
                    )))
                }
            },
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendDirectMessage FindSuccessorSend, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_dht_find_successor_for_connect_to_self_is_noop() -> Result<()> {
        let target = did();

        assert!(lower_dht_action(
            &PeerRingAction::RemoteAction(
                target,
                PeerRingRemoteAction::FindSuccessorForConnect(target),
            ),
            |_| true,
        )?
        .is_none());
        Ok(())
    }

    /// Proves that lowering a finger lookup preserves its complete range token.
    ///
    /// The test checks the remote hop, lookup position, strictness flag, slot,
    /// and UUID after lowering, preventing the effect layer from degrading a
    /// range-aware request back into an uncorrelated slot update.
    #[test]
    fn test_dht_find_successor_for_fix_echoes_range_request() -> Result<()> {
        let next = did();
        let target = did();
        let request = crate::dht::FingerFixRequest::new(11, uuid::Uuid::from_u128(7))
            .ok_or_else(|| Error::InvalidMessage("invalid test finger request".to_owned()))?;

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForFix {
                did: target,
                request,
            }),
            |_| true,
        ))?;

        match effect {
            CoreEffect::SendDirectMessage { msg, destination } => match *msg {
                Message::FindSuccessorSend(msg) => {
                    assert_eq!(destination, next);
                    assert_eq!(msg.did, target);
                    assert!(!msg.strict);
                    match msg.then {
                        FindSuccessorThen::Report(FindSuccessorReportHandler::FixFingerTable {
                            request: reported_request,
                        }) => assert_eq!(reported_request, request),
                        handler => {
                            return Err(Error::InvalidMessage(format!(
                                "expected fix-finger report handler, got {handler:?}"
                            )))
                        }
                    }
                }
                msg => {
                    return Err(Error::InvalidMessage(format!(
                        "expected FindSuccessorSend, got {msg:?}"
                    )))
                }
            },
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendDirectMessage FindSuccessorSend, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_dht_query_successor_list_connects_before_query() -> Result<()> {
        let target = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| false,
        ))?;

        match effect {
            CoreEffect::ConnectDhtPeer { peer } => {
                assert_eq!(peer, target)
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected ConnectDhtPeer, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    /// Verifies that lowering a successor-list query for an admitted peer emits
    /// one correlated send effect addressed to that exact peer.
    #[test]
    fn test_dht_query_successor_list_sends_when_connected() -> Result<()> {
        let target = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| true,
        ))?;

        match effect {
            CoreEffect::SendSuccessorQuery { query, destination } => {
                assert_eq!(destination, target);
                assert_eq!(query.did, target);
                match query.then {
                    QueryFor::SyncSuccessor => {}
                    then => {
                        return Err(Error::InvalidMessage(format!(
                            "expected SyncSuccessor query, got {then:?}"
                        )))
                    }
                }
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendSuccessorQuery, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    /// Proves that successor-sync authority is installed before delivery and
    /// removed when delivery fails.
    ///
    /// A successful send leaves the exact reporter/token pair claimable. A send
    /// to a missing peer returns an error and leaves the same pair unclaimable,
    /// witnessing both sides of the interpreter's transactional boundary.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    #[tokio::test]
    async fn test_successor_query_effect_registers_before_send_and_cancels_send_failure(
    ) -> Result<()> {
        let first = prepare_node(SecretKey::random()).await;
        let second = prepare_node(SecretKey::random()).await;
        manually_establish_connection(&first.swarm, &second.swarm).await;
        wait_for_connection_state(
            &first,
            second.did(),
            rings_transport::core::transport::WebrtcConnectionState::Connected,
        )
        .await?;
        first.dht().join(second.did())?;

        let callback: SharedSwarmCallback = Arc::new(NoopCallback);
        let interpreter = CoreEffectInterpreter::new(&first.swarm.transport, &callback);
        let sent = QueryForTopoInfoSend::new_for_sync(second.did());
        // Keep the id before the query is moved into the effect; the assertion
        // below proves the interpreter registered this exact request.
        let sent_request_id = sent.request_id;
        interpreter
            .run(CoreEffect::send_successor_query(sent, second.did()))
            .await?;
        assert!(first
            .dht()
            .claim_successor_sync_report(second.did(), sent_request_id)?
            .is_some());

        let missing = did();
        first.dht().join(missing)?;
        let failed = QueryForTopoInfoSend::new_for_sync(missing);
        // Failed sends must remove the otherwise claimable successor-sync slot.
        let failed_request_id = failed.request_id;
        assert!(interpreter
            .run(CoreEffect::send_successor_query(failed, missing))
            .await
            .is_err());
        assert!(first
            .dht()
            .claim_successor_sync_report(missing, failed_request_id)?
            .is_none());
        Ok(())
    }

    #[test]
    fn test_dht_notify_sends_predecessor_to_target() -> Result<()> {
        let target = did();
        let predecessor = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| true,
        ))?;

        match effect {
            CoreEffect::SendMessage { msg, destination } => match *msg {
                Message::NotifyPredecessorSend(msg) => {
                    assert_eq!(destination, target);
                    assert_eq!(msg.did, predecessor);
                }
                msg => {
                    return Err(Error::InvalidMessage(format!(
                        "expected NotifyPredecessorSend, got {msg:?}"
                    )))
                }
            },
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendMessage NotifyPredecessorSend, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_dht_notify_connects_target_before_sending() -> Result<()> {
        let target = did();
        let predecessor = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| false,
        ))?;

        match effect {
            CoreEffect::ConnectDhtPeer { peer } => {
                assert_eq!(peer, target)
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected ConnectDhtPeer, got {effect:?}"
                )))
            }
        }
        Ok(())
    }

    #[test]
    fn test_dht_notify_to_self_is_noop() -> Result<()> {
        let target = did();

        assert!(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(target)),
            |_| true,
        )?
        .is_none());
        Ok(())
    }
}
