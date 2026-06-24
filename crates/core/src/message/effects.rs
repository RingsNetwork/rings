//! Explicit effect functors emitted by Core message handlers.
//!
//! This module is the adapter-first boundary for moving handlers away from
//! directly calling transport/DHT APIs. Handlers describe values in small base
//! functors, `CoreEffectF` is their coproduct, and `CoreEffectInterpreter`
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
pub(crate) enum PayloadRelayF {
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

impl PayloadRelayF {
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
pub(crate) enum MessageSendF {
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

impl MessageSendF {
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
pub(crate) enum ConnectionF {
    /// Establish an idempotent DHT-driven transport connection.
    ConnectDhtPeer {
        /// Peer to connect.
        peer: Did,
    },
}

impl ConnectionF {
    /// Create a DHT connection effect.
    pub(crate) fn connect_dht_peer(peer: Did) -> Self {
        Self::ConnectDhtPeer { peer }
    }
}

/// The coproduct of Core effect functors.
#[derive(Clone, Debug)]
pub(crate) enum CoreEffectF {
    /// Payload relay functor.
    Payload(PayloadRelayF),
    /// New message send functor.
    Message(MessageSendF),
    /// Connection management functor.
    Connection(ConnectionF),
}

impl From<PayloadRelayF> for CoreEffectF {
    fn from(effect: PayloadRelayF) -> Self {
        Self::Payload(effect)
    }
}

impl From<MessageSendF> for CoreEffectF {
    fn from(effect: MessageSendF) -> Self {
        Self::Message(effect)
    }
}

impl From<ConnectionF> for CoreEffectF {
    fn from(effect: ConnectionF) -> Self {
        Self::Connection(effect)
    }
}

impl CoreEffectF {
    /// Create a payload-forwarding effect.
    pub(crate) fn forward_payload(payload: &MessagePayload, next_hop: Option<Did>) -> Self {
        PayloadRelayF::forward_payload(payload, next_hop).into()
    }

    /// Create a normally-routed send effect.
    pub(crate) fn send_message(msg: Message, destination: Did) -> Self {
        MessageSendF::send_message(msg, destination).into()
    }

    /// Create a direct send effect.
    pub(crate) fn send_direct_message(msg: Message, destination: Did) -> Self {
        MessageSendF::send_direct_message(msg, destination).into()
    }

    /// Create a report-message effect.
    pub(crate) fn send_report_message(payload: &MessagePayload, msg: Message) -> Self {
        PayloadRelayF::send_report_message(payload, msg).into()
    }

    /// Create a destination-reset effect.
    pub(crate) fn reset_destination(payload: &MessagePayload, next_hop: Did) -> Self {
        PayloadRelayF::reset_destination(payload, next_hop).into()
    }

    /// Create a DHT connection effect.
    pub(crate) fn connect_dht_peer(peer: Did) -> Self {
        ConnectionF::connect_dht_peer(peer).into()
    }
}

/// DHT action base functor consumed by the message layer.
///
/// This is intentionally isomorphic to the leaf `PeerRingAction` cases handled
/// by `MessageHandler`: converting from a leaf action to `DhtActionF` and back
/// preserves the DHT meaning before any transport effect is chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DhtActionF {
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

impl TryFrom<&PeerRingAction> for DhtActionF {
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

impl From<DhtActionF> for PeerRingAction {
    fn from(intent: DhtActionF) -> Self {
        match intent {
            DhtActionF::None => Self::None,
            DhtActionF::FindSuccessorForConnect { next, did } => {
                Self::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did))
            }
            DhtActionF::QueryForSuccessorList { successor } => {
                Self::RemoteAction(successor, PeerRingRemoteAction::QueryForSuccessorList)
            }
            DhtActionF::TryConnect { peer } => {
                Self::RemoteAction(peer, PeerRingRemoteAction::TryConnect)
            }
            DhtActionF::Notify {
                target,
                predecessor,
            } => Self::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
        }
    }
}

impl DhtActionF {
    /// Lower this DHT functor into the Core effect coproduct.
    pub(crate) fn lower(self, is_connected: impl Fn(Did) -> bool) -> Vec<CoreEffectF> {
        match self {
            Self::None => Vec::new(),
            Self::FindSuccessorForConnect { next, did } => {
                if next == did {
                    Vec::new()
                } else {
                    vec![CoreEffectF::send_direct_message(
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
                    vec![CoreEffectF::send_direct_message(
                        Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_sync(
                            successor,
                        )),
                        successor,
                    )]
                } else {
                    vec![CoreEffectF::connect_dht_peer(successor)]
                }
            }
            Self::TryConnect { peer } => vec![CoreEffectF::connect_dht_peer(peer)],
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
                    vec![CoreEffectF::send_message(
                        Message::NotifyPredecessorSend(NotifyPredecessorSend { did: predecessor }),
                        target,
                    )]
                } else {
                    vec![CoreEffectF::connect_dht_peer(target)]
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
pub(crate) fn lower_dht_action_f(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
) -> Result<Vec<CoreEffectF>> {
    DhtActionF::try_from(act).map(|intent| intent.lower(is_connected))
}

/// Interpreter from `CoreEffectF` into the current transport implementation.
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

    /// Interpret one `CoreEffectF`, preserving the existing transport behavior.
    pub(crate) async fn run(&self, effect: CoreEffectF) -> Result<()> {
        match effect {
            CoreEffectF::Payload(effect) => match effect {
                PayloadRelayF::ForwardPayload { payload, next_hop } => {
                    self.transport
                        .forward_payload(payload.as_ref(), next_hop)
                        .await
                }
                PayloadRelayF::SendReportMessage { payload, msg } => {
                    self.transport
                        .send_report_message(payload.as_ref(), *msg)
                        .await
                }
                PayloadRelayF::ResetDestination { payload, next_hop } => {
                    self.transport
                        .reset_destination(payload.as_ref(), next_hop)
                        .await
                }
            },
            CoreEffectF::Message(effect) => match effect {
                MessageSendF::SendMessage { msg, destination } => {
                    self.transport.send_message(*msg, destination).await?;
                    Ok(())
                }
                MessageSendF::SendDirectMessage { msg, destination } => {
                    self.transport
                        .send_direct_message(*msg, destination)
                        .await?;
                    Ok(())
                }
            },
            CoreEffectF::Connection(ConnectionF::ConnectDhtPeer { peer }) => {
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
        effects: impl IntoIterator<Item = CoreEffectF>,
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

    fn single_effect(effects: Result<Vec<CoreEffectF>>) -> Result<CoreEffectF> {
        let effects = effects?;
        assert_eq!(effects.len(), 1);
        effects
            .into_iter()
            .next()
            .ok_or_else(|| Error::InvalidMessage("expected one effect".to_string()))
    }

    fn assert_dht_action_f_iso(intent: DhtActionF) -> Result<()> {
        let action: PeerRingAction = intent.clone().into();
        assert_eq!(DhtActionF::try_from(&action)?, intent);
        Ok(())
    }

    #[test]
    fn dht_action_f_isomorphic_to_peer_ring_leaf_actions() -> Result<()> {
        assert_dht_action_f_iso(DhtActionF::None)?;

        assert_dht_action_f_iso(DhtActionF::FindSuccessorForConnect {
            next: did(),
            did: did(),
        })?;

        assert_dht_action_f_iso(DhtActionF::QueryForSuccessorList { successor: did() })?;
        assert_dht_action_f_iso(DhtActionF::TryConnect { peer: did() })?;

        assert_dht_action_f_iso(DhtActionF::Notify {
            target: did(),
            predecessor: did(),
        })?;
        Ok(())
    }

    #[test]
    fn send_report_message_effect_owns_payload_and_message() -> Result<()> {
        let destination = did();
        let payload = payload(destination)?;
        let effect = CoreEffectF::send_report_message(
            &payload,
            Message::NotifyPredecessorReport(crate::message::NotifyPredecessorReport {
                did: destination,
            }),
        );

        match effect {
            CoreEffectF::Payload(PayloadRelayF::SendReportMessage {
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
        let effect = CoreEffectF::reset_destination(&payload, next_hop);

        match effect {
            CoreEffectF::Payload(PayloadRelayF::ResetDestination {
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

        let effect = single_effect(lower_dht_action_f(
            &PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForConnect(target),
            ),
            |_| true,
        ))?;

        match effect {
            CoreEffectF::Message(MessageSendF::SendDirectMessage { msg, destination }) => {
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

        assert!(lower_dht_action_f(
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

        let effect = single_effect(lower_dht_action_f(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| false,
        ))?;

        match effect {
            CoreEffectF::Connection(ConnectionF::ConnectDhtPeer { peer }) => {
                assert_eq!(peer, target)
            }
            effect => panic!("expected ConnectDhtPeer, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_query_successor_list_sends_when_connected() -> Result<()> {
        let target = did();

        let effect = single_effect(lower_dht_action_f(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
            |_| true,
        ))?;

        match effect {
            CoreEffectF::Message(MessageSendF::SendDirectMessage { msg, destination }) => {
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

        let effect = single_effect(lower_dht_action_f(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| true,
        ))?;

        match effect {
            CoreEffectF::Message(MessageSendF::SendMessage { msg, destination }) => match *msg {
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

        let effect = single_effect(lower_dht_action_f(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
            |_| false,
        ))?;

        match effect {
            CoreEffectF::Connection(ConnectionF::ConnectDhtPeer { peer }) => {
                assert_eq!(peer, target)
            }
            effect => panic!("expected ConnectDhtPeer, got {effect:?}"),
        }
        Ok(())
    }

    #[test]
    fn dht_notify_to_self_is_noop() -> Result<()> {
        let target = did();

        assert!(lower_dht_action_f(
            &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(target)),
            |_| true,
        )?
        .is_empty());
        Ok(())
    }
}
