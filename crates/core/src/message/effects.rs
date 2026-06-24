//! Explicit effect functors emitted by Core message handlers.
//!
//! This module is the adapter-first boundary for moving handlers away from
//! directly calling transport/DHT APIs. Handlers describe values in small base
//! functors, `CoreEffect` is their coproduct, and `CoreEffectInterpreter`
//! lowers that coproduct into the current transport implementation.

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

/// Payload relay base functor.
#[derive(Clone, Debug)]
pub(crate) enum PayloadRelayFunctor {
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

impl PayloadRelayFunctor {
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

/// Fresh message send base functor.
#[derive(Clone, Debug)]
pub(crate) enum MessageSendFunctor {
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

impl MessageSendFunctor {
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

/// Connection-management base functor.
#[derive(Clone, Debug)]
pub(crate) enum ConnectionFunctor {
    /// Establish an idempotent DHT-driven transport connection.
    ConnectDhtPeer {
        /// Peer to connect.
        peer: Did,
    },
}

impl ConnectionFunctor {
    /// Create a DHT connection effect.
    pub(crate) fn connect_dht_peer(peer: Did) -> Self {
        Self::ConnectDhtPeer { peer }
    }
}

/// The coproduct of Core effect functors.
#[derive(Clone, Debug)]
pub(crate) enum CoreEffect {
    /// Payload relay functor.
    Payload(PayloadRelayFunctor),
    /// New message send functor.
    Message(MessageSendFunctor),
    /// Connection management functor.
    Connection(ConnectionFunctor),
}

impl From<PayloadRelayFunctor> for CoreEffect {
    fn from(effect: PayloadRelayFunctor) -> Self {
        Self::Payload(effect)
    }
}

impl From<MessageSendFunctor> for CoreEffect {
    fn from(effect: MessageSendFunctor) -> Self {
        Self::Message(effect)
    }
}

impl From<ConnectionFunctor> for CoreEffect {
    fn from(effect: ConnectionFunctor) -> Self {
        Self::Connection(effect)
    }
}

impl CoreEffect {
    /// Create a payload-forwarding effect.
    pub(crate) fn forward_payload(payload: &MessagePayload, next_hop: Option<Did>) -> Self {
        PayloadRelayFunctor::forward_payload(payload, next_hop).into()
    }

    /// Create a normally-routed send effect.
    pub(crate) fn send_message(msg: Message, destination: Did) -> Self {
        MessageSendFunctor::send_message(msg, destination).into()
    }

    /// Create a direct send effect.
    pub(crate) fn send_direct_message(msg: Message, destination: Did) -> Self {
        MessageSendFunctor::send_direct_message(msg, destination).into()
    }

    /// Create a report-message effect.
    pub(crate) fn send_report_message(payload: &MessagePayload, msg: Message) -> Self {
        PayloadRelayFunctor::send_report_message(payload, msg).into()
    }

    /// Create a destination-reset effect.
    pub(crate) fn reset_destination(payload: &MessagePayload, next_hop: Did) -> Self {
        PayloadRelayFunctor::reset_destination(payload, next_hop).into()
    }

    /// Create a DHT connection effect.
    pub(crate) fn connect_dht_peer(peer: Did) -> Self {
        ConnectionFunctor::connect_dht_peer(peer).into()
    }
}

/// DHT action base functor consumed by the message layer.
///
/// This is intentionally isomorphic to the leaf `PeerRingAction` cases handled
/// by `MessageHandler`: converting from a leaf action to `DhtActionFunctor`
/// and back preserves the DHT meaning before any transport effect is chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DhtActionFunctor {
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

impl TryFrom<&PeerRingAction> for DhtActionFunctor {
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

impl From<DhtActionFunctor> for PeerRingAction {
    fn from(functor: DhtActionFunctor) -> Self {
        match functor {
            DhtActionFunctor::None => Self::None,
            DhtActionFunctor::FindSuccessorForConnect { next, did } => {
                Self::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did))
            }
            DhtActionFunctor::QueryForSuccessorList { successor } => {
                Self::RemoteAction(successor, PeerRingRemoteAction::QueryForSuccessorList)
            }
            DhtActionFunctor::TryConnect { peer } => {
                Self::RemoteAction(peer, PeerRingRemoteAction::TryConnect)
            }
            DhtActionFunctor::Notify {
                target,
                predecessor,
            } => Self::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
        }
    }
}

impl DhtActionFunctor {
    /// Lower this DHT functor into the Core effect coproduct.
    pub(crate) fn lower(self, is_connected: impl Fn(Did) -> bool) -> Vec<CoreEffect> {
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

/// Natural transformation from a single DHT leaf action to Core effects.
///
/// `MultiActions` preserve their old concurrent best-effort behavior in
/// `MessageHandler::handle_dht_events`, so this function intentionally handles
/// only the leaf actions emitted by Core DHT operations.
pub(crate) fn lower_dht_action(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
) -> Result<Vec<CoreEffect>> {
    DhtActionFunctor::try_from(act).map(|functor| functor.lower(is_connected))
}

/// Interpreter from `CoreEffect` into the current transport implementation.
#[derive(Clone)]
pub(crate) struct CoreEffectInterpreter {
    transport: Arc<SwarmTransport>,
    swarm_callback: SharedSwarmCallback,
}

impl CoreEffectInterpreter {
    /// Create an interpreter over the current swarm transport.
    pub(crate) fn new(transport: Arc<SwarmTransport>, swarm_callback: SharedSwarmCallback) -> Self {
        Self {
            transport,
            swarm_callback,
        }
    }

    /// Interpret one `CoreEffect`, preserving the existing transport behavior.
    pub(crate) async fn run(&self, effect: CoreEffect) -> Result<()> {
        match effect {
            CoreEffect::Payload(effect) => match effect {
                PayloadRelayFunctor::ForwardPayload { payload, next_hop } => {
                    self.transport
                        .forward_payload(payload.as_ref(), next_hop)
                        .await
                }
                PayloadRelayFunctor::SendReportMessage { payload, msg } => {
                    self.transport
                        .send_report_message(payload.as_ref(), *msg)
                        .await
                }
                PayloadRelayFunctor::ResetDestination { payload, next_hop } => {
                    self.transport
                        .reset_destination(payload.as_ref(), next_hop)
                        .await
                }
            },
            CoreEffect::Message(effect) => match effect {
                MessageSendFunctor::SendMessage { msg, destination } => {
                    self.transport.send_message(*msg, destination).await?;
                    Ok(())
                }
                MessageSendFunctor::SendDirectMessage { msg, destination } => {
                    self.transport
                        .send_direct_message(*msg, destination)
                        .await?;
                    Ok(())
                }
            },
            CoreEffect::Connection(ConnectionFunctor::ConnectDhtPeer { peer }) => {
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

    /// Interpret effects in order and fail on the first execution error.
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

    fn assert_dht_action_functor_isomorphism(functor: DhtActionFunctor) -> Result<()> {
        let action: PeerRingAction = functor.clone().into();
        assert_eq!(DhtActionFunctor::try_from(&action)?, functor);
        Ok(())
    }

    #[test]
    fn dht_action_functor_isomorphic_to_peer_ring_leaf_actions() -> Result<()> {
        assert_dht_action_functor_isomorphism(DhtActionFunctor::None)?;

        assert_dht_action_functor_isomorphism(DhtActionFunctor::FindSuccessorForConnect {
            next: did(),
            did: did(),
        })?;

        assert_dht_action_functor_isomorphism(DhtActionFunctor::QueryForSuccessorList {
            successor: did(),
        })?;
        assert_dht_action_functor_isomorphism(DhtActionFunctor::TryConnect { peer: did() })?;

        assert_dht_action_functor_isomorphism(DhtActionFunctor::Notify {
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
            CoreEffect::Payload(PayloadRelayFunctor::SendReportMessage {
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
            CoreEffect::Payload(PayloadRelayFunctor::ResetDestination {
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

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForConnect(target),
            ),
            |_| true,
        ))?;

        match effect {
            CoreEffect::Message(MessageSendFunctor::SendDirectMessage { msg, destination }) => {
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

        assert!(lower_dht_action(
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

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| false,
        ))?;

        match effect {
            CoreEffect::Connection(ConnectionFunctor::ConnectDhtPeer { peer }) => {
                assert_eq!(peer, target)
            }
            effect => panic!("expected ConnectDhtPeer, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_query_successor_list_sends_when_connected() -> Result<()> {
        let target = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| true,
        ))?;

        match effect {
            CoreEffect::Message(MessageSendFunctor::SendDirectMessage { msg, destination }) => {
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

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| true,
        ))?;

        match effect {
            CoreEffect::Message(MessageSendFunctor::SendMessage { msg, destination }) => match *msg
            {
                Message::NotifyPredecessorSend(msg) => {
                    assert_eq!(destination, target);
                    assert_eq!(msg.did, predecessor);
                }
                msg => panic!("expected NotifyPredecessorSend, got {msg:?}"),
            },
            effect => panic!("expected SendMessage NotifyPredecessorSend, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_notify_connects_target_before_sending() -> Result<()> {
        let target = did();
        let predecessor = did();

        let effect = single_effect(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| false,
        ))?;

        match effect {
            CoreEffect::Connection(ConnectionFunctor::ConnectDhtPeer { peer }) => {
                assert_eq!(peer, target)
            }
            effect => panic!("expected ConnectDhtPeer, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_notify_to_self_is_noop() -> Result<()> {
        let target = did();

        assert!(lower_dht_action(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(target)),
            |_| true,
        )?
        .is_empty());
        Ok(())
    }
}
