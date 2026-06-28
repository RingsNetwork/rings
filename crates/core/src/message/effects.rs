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
pub(crate) enum PayloadRelayFunctor<'payload> {
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
}

impl<'payload> PayloadRelayFunctor<'payload> {
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
pub(crate) enum CoreEffect<'payload> {
    /// Payload relay functor.
    Payload(PayloadRelayFunctor<'payload>),
    /// New message send functor.
    Message(MessageSendFunctor),
    /// Connection management functor.
    Connection(ConnectionFunctor),
}

impl<'payload> From<PayloadRelayFunctor<'payload>> for CoreEffect<'payload> {
    fn from(effect: PayloadRelayFunctor<'payload>) -> Self {
        Self::Payload(effect)
    }
}

impl<'payload> From<MessageSendFunctor> for CoreEffect<'payload> {
    fn from(effect: MessageSendFunctor) -> Self {
        Self::Message(effect)
    }
}

impl<'payload> From<ConnectionFunctor> for CoreEffect<'payload> {
    fn from(effect: ConnectionFunctor) -> Self {
        Self::Connection(effect)
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
    /// Ask `next` to find `did` and report with the finger-fix handler.
    FindSuccessorForFix {
        /// Next hop to query.
        next: Did,
        /// DID being searched.
        did: Did,
        /// Finger slot fixed by this lookup.
        index: usize,
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
            PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForFix { did, index },
            ) => Ok(Self::FindSuccessorForFix {
                next: *next,
                did: *did,
                index: *index,
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
            act => Err(Error::unexpected_peer_ring_action(act.clone())),
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
            DhtActionFunctor::FindSuccessorForFix { next, did, index } => {
                Self::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForFix {
                    did,
                    index,
                })
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
    pub(crate) fn lower<'payload>(
        self,
        is_connected: impl Fn(Did) -> bool,
    ) -> Option<CoreEffect<'payload>> {
        match self {
            Self::None => None,
            Self::FindSuccessorForConnect { next, did } => {
                if next == did {
                    None
                } else {
                    Some(
                        MessageSendFunctor::send_direct_message(
                            Message::FindSuccessorSend(FindSuccessorSend {
                                did,
                                strict: false,
                                then: FindSuccessorThen::Report(
                                    FindSuccessorReportHandler::Connect,
                                ),
                            }),
                            next,
                        )
                        .into(),
                    )
                }
            }
            Self::FindSuccessorForFix { next, did, index } => {
                if next == did {
                    None
                } else {
                    Some(
                        MessageSendFunctor::send_direct_message(
                            Message::FindSuccessorSend(FindSuccessorSend {
                                did,
                                strict: false,
                                then: FindSuccessorThen::Report(
                                    FindSuccessorReportHandler::FixFingerTable { index },
                                ),
                            }),
                            next,
                        )
                        .into(),
                    )
                }
            }
            Self::QueryForSuccessorList { successor } => {
                if is_connected(successor) {
                    Some(
                        MessageSendFunctor::send_direct_message(
                            Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_sync(
                                successor,
                            )),
                            successor,
                        )
                        .into(),
                    )
                } else {
                    Some(ConnectionFunctor::connect_dht_peer(successor).into())
                }
            }
            Self::TryConnect { peer } => Some(ConnectionFunctor::connect_dht_peer(peer).into()),
            Self::Notify {
                target,
                predecessor,
            } => {
                if target == predecessor {
                    None
                } else if is_connected(target) {
                    // `RemoteAction(target, Notify(pred))` means "send pred to target"
                    // and maps to CorrectStabilize.notify' in the TLA+ spec mirror.
                    Some(
                        MessageSendFunctor::send_message(
                            Message::NotifyPredecessorSend(NotifyPredecessorSend {
                                did: predecessor,
                            }),
                            target,
                        )
                        .into(),
                    )
                } else {
                    Some(ConnectionFunctor::connect_dht_peer(target).into())
                }
            }
        }
    }
}

/// Natural transformation from a single DHT leaf action to Core effects.
///
/// `MultiActions` are flattened by `MessageHandler::handle_dht_events`, which
/// can quality-order connection leaves while preserving best-effort execution.
/// This function intentionally handles only the leaf actions emitted by Core DHT
/// operations.
pub(crate) fn lower_dht_action<'payload>(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
) -> Result<Option<CoreEffect<'payload>>> {
    DhtActionFunctor::try_from(act).map(|functor| functor.lower(is_connected))
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
            CoreEffect::Payload(effect) => match effect {
                PayloadRelayFunctor::ForwardPayload { payload, next_hop } => {
                    self.transport.forward_payload(payload, next_hop).await
                }
                PayloadRelayFunctor::SendReportMessage { payload, msg } => {
                    self.transport.send_report_message(payload, *msg).await
                }
                PayloadRelayFunctor::ResetDestination { payload, next_hop } => {
                    self.transport.reset_destination(payload, next_hop).await
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
                if self.connection_is_satisfied(peer) {
                    return Ok(());
                }

                let callback = InnerSwarmCallback::new(
                    Arc::clone(self.transport),
                    Arc::clone(self.swarm_callback),
                );
                match self.transport.connect(peer, callback).await {
                    Ok(()) | Err(Error::AlreadyConnected) => Ok(()),
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

    fn single_effect<'payload>(
        effect: Result<Option<CoreEffect<'payload>>>,
    ) -> Result<CoreEffect<'payload>> {
        effect?.ok_or_else(|| Error::InvalidMessage("expected one effect".to_string()))
    }

    fn assert_dht_action_functor_section(functor: DhtActionFunctor) -> Result<()> {
        let action: PeerRingAction = functor.clone().into();
        assert_eq!(DhtActionFunctor::try_from(&action)?, functor);
        Ok(())
    }

    fn assert_dht_action_functor_retraction(action: PeerRingAction) -> Result<()> {
        let functor = DhtActionFunctor::try_from(&action)?;
        let roundtrip: PeerRingAction = functor.into();
        assert_eq!(roundtrip, action);
        Ok(())
    }

    #[test]
    fn dht_action_functor_isomorphic_to_peer_ring_leaf_actions() -> Result<()> {
        assert_dht_action_functor_section(DhtActionFunctor::None)?;

        assert_dht_action_functor_section(DhtActionFunctor::FindSuccessorForConnect {
            next: did(),
            did: did(),
        })?;
        assert_dht_action_functor_section(DhtActionFunctor::FindSuccessorForFix {
            next: did(),
            did: did(),
            index: 7,
        })?;

        assert_dht_action_functor_section(DhtActionFunctor::QueryForSuccessorList {
            successor: did(),
        })?;
        assert_dht_action_functor_section(DhtActionFunctor::TryConnect { peer: did() })?;

        assert_dht_action_functor_section(DhtActionFunctor::Notify {
            target: did(),
            predecessor: did(),
        })?;

        assert_dht_action_functor_retraction(PeerRingAction::None)?;
        assert_dht_action_functor_retraction(PeerRingAction::RemoteAction(
            did(),
            PeerRingRemoteAction::FindSuccessorForConnect(did()),
        ))?;
        assert_dht_action_functor_retraction(PeerRingAction::RemoteAction(
            did(),
            PeerRingRemoteAction::FindSuccessorForFix {
                did: did(),
                index: 7,
            },
        ))?;
        assert_dht_action_functor_retraction(PeerRingAction::RemoteAction(
            did(),
            PeerRingRemoteAction::QueryForSuccessorList,
        ))?;
        assert_dht_action_functor_retraction(PeerRingAction::RemoteAction(
            did(),
            PeerRingRemoteAction::TryConnect,
        ))?;
        assert_dht_action_functor_retraction(PeerRingAction::RemoteAction(
            did(),
            PeerRingRemoteAction::Notify(did()),
        ))?;
        Ok(())
    }

    #[test]
    fn send_report_message_effect_borrows_payload_and_owns_message() -> Result<()> {
        let destination = did();
        let payload = payload(destination)?;
        let effect: CoreEffect<'_> = PayloadRelayFunctor::send_report_message(
            &payload,
            Message::NotifyPredecessorReport(crate::message::NotifyPredecessorReport {
                did: destination,
            }),
        )
        .into();

        match effect {
            CoreEffect::Payload(PayloadRelayFunctor::SendReportMessage {
                payload: effect_payload,
                msg,
            }) => {
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
    fn reset_destination_effect_borrows_payload_and_next_hop() -> Result<()> {
        let destination = did();
        let next_hop = did();
        let payload = payload(destination)?;
        let effect: CoreEffect<'_> =
            PayloadRelayFunctor::reset_destination(&payload, next_hop).into();

        match effect {
            CoreEffect::Payload(PayloadRelayFunctor::ResetDestination {
                payload: effect_payload,
                next_hop: effect_next_hop,
            }) => {
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
                }
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendDirectMessage FindSuccessorSend, got {effect:?}"
                )))
            }
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
        .is_none());
        Ok(())
    }

    #[test]
    fn dht_find_successor_for_fix_sends_direct_indexed_report() -> Result<()> {
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
            CoreEffect::Message(MessageSendFunctor::SendDirectMessage { msg, destination }) => {
                match *msg {
                    Message::FindSuccessorSend(msg) => {
                        assert_eq!(destination, next);
                        assert_eq!(msg.did, target);
                        assert!(!msg.strict);
                        match msg.then {
                            FindSuccessorThen::Report(
                                FindSuccessorReportHandler::FixFingerTable {
                                    index: reported_index,
                                },
                            ) => assert_eq!(reported_index, index),
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
                }
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendDirectMessage FindSuccessorSend, got {effect:?}"
                )))
            }
        }
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
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected ConnectDhtPeer, got {effect:?}"
                )))
            }
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
                }
            }
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected SendDirectMessage QueryForTopoInfoSend, got {effect:?}"
                )))
            }
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
            effect => {
                return Err(Error::InvalidMessage(format!(
                    "expected ConnectDhtPeer, got {effect:?}"
                )))
            }
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
        .is_none());
        Ok(())
    }
}
