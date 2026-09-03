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
use crate::message::SyncEntriesWithSuccessor;
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
/// [`CORE_ACTOR_BROWSER_YIELD_INTERVAL`] steps, bounding event-loop starvation
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
    /// Forward an existing payload through the relay path.
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
    /// Establish an idempotent DHT-driven transport connection.
    ConnectDhtPeer {
        /// Peer to connect.
        peer: Did,
    },
    /// Send copy-only storage entries and register the matching ack capability.
    SendStorageSync {
        /// Storage-sync message to route by its storage destination.
        msg: SyncEntriesWithSuccessor,
    },
    /// Hold an application payload in the relay inbox of its offline destination.
    HoldForOfflineDestination {
        /// Payload whose destination this node is responsible for but cannot reach.
        payload: &'payload MessagePayload,
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

    /// Create a DHT connection effect.
    pub(crate) const fn connect_dht_peer(peer: Did) -> Self {
        Self::ConnectDhtPeer { peer }
    }

    /// Create a storage-sync effect.
    pub(crate) fn send_storage_sync(msg: SyncEntriesWithSuccessor) -> Self {
        Self::SendStorageSync { msg }
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
            PeerRingRemoteAction::FindSuccessorForFix { did, index },
        ) => Ok(find_successor_effect(
            *next,
            *did,
            FindSuccessorReportHandler::FixFingerTable { index: *index },
        )),
        PeerRingAction::RemoteAction(successor, PeerRingRemoteAction::QueryForSuccessorList) => {
            Ok(Some(if is_connected(*successor) {
                CoreEffect::send_direct_message(
                    Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_sync(*successor)),
                    *successor,
                )
            } else {
                CoreEffect::connect_dht_peer(*successor)
            }))
        }
        PeerRingAction::RemoteAction(peer, PeerRingRemoteAction::TryConnect) => {
            Ok(Some(CoreEffect::connect_dht_peer(*peer)))
        }
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
            CoreEffect::SendStorageSync { msg } => {
                self.transport
                    .send_storage_sync_or_defer(msg, "core_effect")
                    .await?;
                Ok(())
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
    use crate::dht::StorageSyncDestination;
    use crate::dht::StorageSyncPurpose;
    use crate::ecc::SecretKey;
    use crate::message::types::QueryFor;
    use crate::message::MessageSigner;
    use crate::session::SessionSk;
    use crate::tests::TEST_NETWORK_ID;

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
    fn test_storage_sync_effect_owns_sync_message() -> Result<()> {
        let destination = did();
        let msg = SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::OwnershipHandoff,
            destination: StorageSyncDestination::PhysicalOwner(destination),
            data: vec![],
        };

        let effect: CoreEffect<'_> = CoreEffect::send_storage_sync(msg.clone());

        match effect {
            CoreEffect::SendStorageSync { msg: effect_msg } => {
                assert_eq!(effect_msg.destination, msg.destination);
                assert!(effect_msg.data.is_empty());
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendStorageSync, got {effect:?}"
                )))
            }
        }
        Ok(())
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

    #[test]
    fn test_dht_find_successor_for_fix_sends_direct_indexed_report() -> Result<()> {
        let next = did();
        let target = did();
        let index = 11;

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForFix {
                did: target,
                index,
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
                            index: reported_index,
                        }) => assert_eq!(reported_index, index),
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

    #[test]
    fn test_dht_query_successor_list_sends_when_connected() -> Result<()> {
        let target = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| true,
        ))?;

        match effect {
            CoreEffect::SendDirectMessage { msg, destination } => match *msg {
                Message::QueryForTopoInfoSend(msg) => {
                    assert_eq!(destination, target);
                    assert_eq!(msg.did, target);
                    match msg.then {
                        QueryFor::SyncSuccessor => {}
                        then => {
                            return Err(Error::InvalidMessage(format!(
                                "expected SyncSuccessor query, got {then:?}"
                            )))
                        }
                    }
                }
                msg => {
                    return Err(Error::InvalidMessage(format!(
                        "expected QueryForTopoInfoSend, got {msg:?}"
                    )))
                }
            },
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendDirectMessage QueryForTopoInfoSend, got {effect:?}"
                )))
            }
        }
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
