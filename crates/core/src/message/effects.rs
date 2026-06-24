//! Explicit side effects emitted by Core message handlers.
//!
//! This module is the adapter-first boundary for moving handlers away from
//! directly calling transport/DHT APIs. Handlers should describe the effect they
//! need, and the runner preserves the current execution behavior while the
//! migration proceeds message family by message family.

use std::sync::Arc;

use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Error;
use crate::error::Result;
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

/// A payload-relay effect requested by a Core protocol handler.
#[derive(Clone, Debug)]
pub(crate) enum PayloadRelayEffect {
    /// Forward an existing payload through the relay path.
    ForwardPayload {
        /// Payload to forward.
        payload: Box<MessagePayload>,
        /// Optional explicit next hop. `None` preserves current DHT inference.
        next_hop: Option<Did>,
    },
    /// Send a report message using the original request payload.
    SendReportMessage {
        /// Request payload to report against.
        payload: Box<MessagePayload>,
        /// Report message to send.
        msg: Box<Message>,
    },
    /// Reset a relayed payload to a new destination/next-hop.
    ResetDestination {
        /// Payload to relay after resetting destination.
        payload: Box<MessagePayload>,
        /// New destination and next hop.
        next_hop: Did,
    },
}

impl PayloadRelayEffect {
    /// Create a payload-forwarding effect.
    pub(crate) fn forward_payload(payload: &MessagePayload, next_hop: Option<Did>) -> Self {
        Self::ForwardPayload {
            payload: Box::new(payload.clone()),
            next_hop,
        }
    }

    /// Create a report-message effect.
    pub(crate) fn send_report_message(payload: &MessagePayload, msg: Message) -> Self {
        Self::SendReportMessage {
            payload: Box::new(payload.clone()),
            msg: Box::new(msg),
        }
    }

    /// Create a destination-reset effect.
    pub(crate) fn reset_destination(payload: &MessagePayload, next_hop: Did) -> Self {
        Self::ResetDestination {
            payload: Box::new(payload.clone()),
            next_hop,
        }
    }
}

/// A freshly-created message send effect.
#[derive(Clone, Debug)]
pub(crate) enum MessageSendEffect {
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
}

impl MessageSendEffect {
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
}

/// A connection-management effect requested by Core protocol logic.
#[derive(Clone, Debug)]
pub(crate) enum ConnectionEffect {
    /// Establish an idempotent DHT-driven transport connection.
    ConnectDhtPeer {
        /// Peer to connect.
        peer: Did,
    },
}

impl ConnectionEffect {
    /// Create a DHT connection effect.
    pub(crate) fn connect_dht_peer(peer: Did) -> Self {
        Self::ConnectDhtPeer { peer }
    }
}

/// The coproduct of Core effect functors.
#[derive(Clone, Debug)]
pub(crate) enum CoreEffect {
    /// Payload relay functor.
    Payload(PayloadRelayEffect),
    /// New message send functor.
    Message(MessageSendEffect),
    /// Connection management functor.
    Connection(ConnectionEffect),
}

impl From<PayloadRelayEffect> for CoreEffect {
    fn from(effect: PayloadRelayEffect) -> Self {
        Self::Payload(effect)
    }
}

impl From<MessageSendEffect> for CoreEffect {
    fn from(effect: MessageSendEffect) -> Self {
        Self::Message(effect)
    }
}

impl From<ConnectionEffect> for CoreEffect {
    fn from(effect: ConnectionEffect) -> Self {
        Self::Connection(effect)
    }
}

impl CoreEffect {
    /// Create a payload-forwarding effect.
    pub(crate) fn forward_payload(payload: &MessagePayload, next_hop: Option<Did>) -> Self {
        PayloadRelayEffect::forward_payload(payload, next_hop).into()
    }

    /// Create a normally-routed send effect.
    pub(crate) fn send_message(msg: Message, destination: Did) -> Self {
        MessageSendEffect::send_message(msg, destination).into()
    }

    /// Create a direct send effect.
    pub(crate) fn send_direct_message(msg: Message, destination: Did) -> Self {
        MessageSendEffect::send_direct_message(msg, destination).into()
    }

    /// Create a report-message effect.
    pub(crate) fn send_report_message(payload: &MessagePayload, msg: Message) -> Self {
        PayloadRelayEffect::send_report_message(payload, msg).into()
    }

    /// Create a destination-reset effect.
    pub(crate) fn reset_destination(payload: &MessagePayload, next_hop: Did) -> Self {
        PayloadRelayEffect::reset_destination(payload, next_hop).into()
    }

    /// Create a DHT connection effect.
    pub(crate) fn connect_dht_peer(peer: Did) -> Self {
        ConnectionEffect::connect_dht_peer(peer).into()
    }
}

/// Semantic DHT action intent consumed by the message layer.
///
/// This is intentionally isomorphic to the leaf `PeerRingAction` cases handled
/// by `MessageHandler`: converting from a leaf action to an intent and back
/// preserves the DHT meaning before any transport effect is chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DhtActionIntent {
    /// No follow-up side effect is required.
    None,
    /// Ask `next` to find `did` and report with the connect handler.
    FindSuccessorForConnect {
        /// Next hop to query.
        next: Did,
        /// DID being searched.
        did: Did,
    },
    /// Query a successor for its successor list.
    QueryForSuccessorList {
        /// Successor to query.
        successor: Did,
    },
    /// Try to establish a DHT transport connection.
    TryConnect {
        /// Peer to connect.
        peer: Did,
    },
    /// Notify a target about a predecessor.
    Notify {
        /// Notify target.
        target: Did,
        /// Predecessor announced to the target.
        predecessor: Did,
    },
}

impl TryFrom<&PeerRingAction> for DhtActionIntent {
    type Error = Error;

    fn try_from(act: &PeerRingAction) -> Result<Self> {
        match act {
            PeerRingAction::None => Ok(Self::None),
            PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForConnect(did),
            ) => Ok(Self::FindSuccessorForConnect {
                next: *next,
                did: *did,
            }),
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
                Ok(Self::QueryForSuccessorList { successor: *next })
            }
            PeerRingAction::RemoteAction(peer, PeerRingRemoteAction::TryConnect) => {
                Ok(Self::TryConnect { peer: *peer })
            }
            PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)) => {
                Ok(Self::Notify {
                    target: *target,
                    predecessor: *predecessor,
                })
            }
            act => Err(Error::PeerRingUnexpectedAction(act.clone())),
        }
    }
}

impl From<DhtActionIntent> for PeerRingAction {
    fn from(intent: DhtActionIntent) -> Self {
        match intent {
            DhtActionIntent::None => Self::None,
            DhtActionIntent::FindSuccessorForConnect { next, did } => {
                Self::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did))
            }
            DhtActionIntent::QueryForSuccessorList { successor } => {
                Self::RemoteAction(successor, PeerRingRemoteAction::QueryForSuccessorList)
            }
            DhtActionIntent::TryConnect { peer } => {
                Self::RemoteAction(peer, PeerRingRemoteAction::TryConnect)
            }
            DhtActionIntent::Notify {
                target,
                predecessor,
            } => Self::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
        }
    }
}

impl DhtActionIntent {
    /// Lower the DHT intent functor into primitive Core effects.
    pub(crate) fn into_effects(self, is_connected: impl Fn(Did) -> bool) -> Vec<CoreEffect> {
        match self {
            Self::None => Vec::new(),
            Self::FindSuccessorForConnect { next, did } => {
                if next == did {
                    Vec::new()
                } else {
                    vec![CoreEffect::send_direct_message(
                        Message::FindSuccessorSend(FindSuccessorSend {
                            did,
                            strict: false,
                            then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                        }),
                        next,
                    )]
                }
            }
            Self::QueryForSuccessorList { successor } => {
                if is_connected(successor) {
                    vec![CoreEffect::send_direct_message(
                        Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_sync(
                            successor,
                        )),
                        successor,
                    )]
                } else {
                    vec![CoreEffect::connect_dht_peer(successor)]
                }
            }
            Self::TryConnect { peer } => vec![CoreEffect::connect_dht_peer(peer)],
            Self::Notify {
                target,
                predecessor,
            } => {
                if target == predecessor {
                    tracing::warn!("Notify target is equal to predecessor, may implement wrong.");
                    Vec::new()
                } else if is_connected(target) {
                    // `RemoteAction(target, Notify(pred))` means "send pred to target"
                    // and maps to CorrectStabilize.notify' in the TLA+ spec mirror.
                    vec![CoreEffect::send_message(
                        Message::NotifyPredecessorSend(NotifyPredecessorSend { did: predecessor }),
                        target,
                    )]
                } else {
                    vec![CoreEffect::connect_dht_peer(target)]
                }
            }
        }
    }
}

/// Translate a single DHT action into the transport effects it requires.
///
/// `MultiActions` preserve their old concurrent best-effort behavior in
/// `MessageHandler::handle_dht_events`, so this function intentionally handles
/// only the leaf actions emitted by Core DHT operations.
pub(crate) fn dht_action_effects(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
) -> Result<Vec<CoreEffect>> {
    DhtActionIntent::try_from(act).map(|intent| intent.into_effects(is_connected))
}

/// Executes Core effects against the current transport implementation.
#[derive(Clone)]
pub(crate) struct CoreEffectRunner {
    transport: Arc<SwarmTransport>,
    swarm_callback: SharedSwarmCallback,
}

impl CoreEffectRunner {
    /// Create a runner over the current swarm transport.
    pub(crate) fn new(transport: Arc<SwarmTransport>, swarm_callback: SharedSwarmCallback) -> Self {
        Self {
            transport,
            swarm_callback,
        }
    }

    /// Execute one effect, preserving the existing behavior for that effect.
    pub(crate) async fn run(&self, effect: CoreEffect) -> Result<()> {
        match effect {
            CoreEffect::Payload(effect) => match effect {
                PayloadRelayEffect::ForwardPayload { payload, next_hop } => {
                    self.transport
                        .forward_payload(payload.as_ref(), next_hop)
                        .await
                }
                PayloadRelayEffect::SendReportMessage { payload, msg } => {
                    self.transport
                        .send_report_message(payload.as_ref(), *msg)
                        .await
                }
                PayloadRelayEffect::ResetDestination { payload, next_hop } => {
                    self.transport
                        .reset_destination(payload.as_ref(), next_hop)
                        .await
                }
            },
            CoreEffect::Message(effect) => match effect {
                MessageSendEffect::SendMessage { msg, destination } => {
                    self.transport.send_message(*msg, destination).await?;
                    Ok(())
                }
                MessageSendEffect::SendDirectMessage { msg, destination } => {
                    self.transport
                        .send_direct_message(*msg, destination)
                        .await?;
                    Ok(())
                }
            },
            CoreEffect::Connection(ConnectionEffect::ConnectDhtPeer { peer }) => {
                if peer == self.transport.dht.did || self.transport.get_connection(peer).is_some() {
                    return Ok(());
                }

                let callback =
                    InnerSwarmCallback::new(self.transport.clone(), self.swarm_callback.clone());
                match self.transport.connect(peer, callback).await {
                    Ok(()) | Err(Error::AlreadyConnected) => Ok(()),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Execute effects in order and fail on the first execution error.
    pub(crate) async fn run_all(
        &self,
        effects: impl IntoIterator<Item = CoreEffect>,
    ) -> Result<()> {
        for effect in effects {
            self.run(effect).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;
    use crate::message::types::QueryFor;
    use crate::session::SessionSk;

    fn did() -> Did {
        SecretKey::random().address().into()
    }

    fn payload(destination: Did) -> Result<MessagePayload> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        MessagePayload::new_send(
            Message::custom(b"hello")?,
            &session_sk,
            destination,
            destination,
        )
    }

    fn single_effect(effects: Result<Vec<CoreEffect>>) -> Result<CoreEffect> {
        let effects = effects?;
        assert_eq!(effects.len(), 1);
        effects
            .into_iter()
            .next()
            .ok_or_else(|| Error::InvalidMessage("expected one effect".to_string()))
    }

    fn assert_dht_intent_roundtrip(intent: DhtActionIntent) -> Result<()> {
        let action: PeerRingAction = intent.clone().into();
        assert_eq!(DhtActionIntent::try_from(&action)?, intent);
        Ok(())
    }

    #[test]
    fn dht_intents_roundtrip_with_leaf_actions() -> Result<()> {
        assert_dht_intent_roundtrip(DhtActionIntent::None)?;

        assert_dht_intent_roundtrip(DhtActionIntent::FindSuccessorForConnect {
            next: did(),
            did: did(),
        })?;

        assert_dht_intent_roundtrip(DhtActionIntent::QueryForSuccessorList { successor: did() })?;
        assert_dht_intent_roundtrip(DhtActionIntent::TryConnect { peer: did() })?;

        assert_dht_intent_roundtrip(DhtActionIntent::Notify {
            target: did(),
            predecessor: did(),
        })?;
        Ok(())
    }

    #[test]
    fn send_report_message_effect_owns_payload_and_message() -> Result<()> {
        let destination = did();
        let payload = payload(destination)?;
        let effect = CoreEffect::send_report_message(
            &payload,
            Message::NotifyPredecessorReport(crate::message::NotifyPredecessorReport {
                did: destination,
            }),
        );

        match effect {
            CoreEffect::Payload(PayloadRelayEffect::SendReportMessage {
                payload: effect_payload,
                msg,
            }) => {
                assert_eq!(effect_payload.as_ref(), &payload);
                match *msg {
                    Message::NotifyPredecessorReport(report) => assert_eq!(report.did, destination),
                    msg => panic!("expected NotifyPredecessorReport, got {msg:?}"),
                }
            }
            effect => panic!("expected SendReportMessage, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn reset_destination_effect_owns_payload_and_next_hop() -> Result<()> {
        let destination = did();
        let next_hop = did();
        let payload = payload(destination)?;
        let effect = CoreEffect::reset_destination(&payload, next_hop);

        match effect {
            CoreEffect::Payload(PayloadRelayEffect::ResetDestination {
                payload: effect_payload,
                next_hop: effect_next_hop,
            }) => {
                assert_eq!(effect_payload.as_ref(), &payload);
                assert_eq!(effect_next_hop, next_hop);
            }
            effect => panic!("expected ResetDestination, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_find_successor_for_connect_sends_direct_report() -> Result<()> {
        let next = did();
        let target = did();

        let effect = single_effect(dht_action_effects(
            &PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForConnect(target),
            ),
            |_| true,
        ))?;

        match effect {
            CoreEffect::Message(MessageSendEffect::SendDirectMessage { msg, destination }) => {
                match *msg {
                    Message::FindSuccessorSend(msg) => {
                        assert_eq!(destination, next);
                        assert_eq!(msg.did, target);
                        assert!(!msg.strict);
                        match msg.then {
                            FindSuccessorThen::Report(FindSuccessorReportHandler::Connect) => {}
                            handler => panic!("expected connect report handler, got {handler:?}"),
                        }
                    }
                    msg => panic!("expected FindSuccessorSend, got {msg:?}"),
                }
            }
            effect => panic!("expected SendDirectMessage FindSuccessorSend, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_find_successor_for_connect_to_self_is_noop() -> Result<()> {
        let target = did();

        assert!(dht_action_effects(
            &PeerRingAction::RemoteAction(
                target,
                PeerRingRemoteAction::FindSuccessorForConnect(target),
            ),
            |_| true,
        )?
        .is_empty());
        Ok(())
    }

    #[test]
    fn dht_query_successor_list_connects_before_query() -> Result<()> {
        let target = did();

        let effect = single_effect(dht_action_effects(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| false,
        ))?;

        match effect {
            CoreEffect::Connection(ConnectionEffect::ConnectDhtPeer { peer }) => {
                assert_eq!(peer, target)
            }
            effect => panic!("expected ConnectDhtPeer, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_query_successor_list_sends_when_connected() -> Result<()> {
        let target = did();

        let effect = single_effect(dht_action_effects(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| true,
        ))?;

        match effect {
            CoreEffect::Message(MessageSendEffect::SendDirectMessage { msg, destination }) => {
                match *msg {
                    Message::QueryForTopoInfoSend(msg) => {
                        assert_eq!(destination, target);
                        assert_eq!(msg.did, target);
                        match msg.then {
                            QueryFor::SyncSuccessor => {}
                            then => panic!("expected SyncSuccessor query, got {then:?}"),
                        }
                    }
                    msg => panic!("expected QueryForTopoInfoSend, got {msg:?}"),
                }
            }
            effect => panic!("expected SendDirectMessage QueryForTopoInfoSend, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_notify_sends_predecessor_to_target() -> Result<()> {
        let target = did();
        let predecessor = did();

        let effect = single_effect(dht_action_effects(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| true,
        ))?;

        match effect {
            CoreEffect::Message(MessageSendEffect::SendMessage { msg, destination }) => {
                match *msg {
                    Message::NotifyPredecessorSend(msg) => {
                        assert_eq!(destination, target);
                        assert_eq!(msg.did, predecessor);
                    }
                    msg => panic!("expected NotifyPredecessorSend, got {msg:?}"),
                }
            }
            effect => panic!("expected SendMessage NotifyPredecessorSend, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_notify_connects_target_before_sending() -> Result<()> {
        let target = did();
        let predecessor = did();

        let effect = single_effect(dht_action_effects(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| false,
        ))?;

        match effect {
            CoreEffect::Connection(ConnectionEffect::ConnectDhtPeer { peer }) => {
                assert_eq!(peer, target)
            }
            effect => panic!("expected ConnectDhtPeer, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_notify_to_self_is_noop() -> Result<()> {
        let target = did();

        assert!(dht_action_effects(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(target)),
            |_| true,
        )?
        .is_empty());
        Ok(())
    }
}
