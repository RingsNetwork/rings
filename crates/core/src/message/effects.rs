//! Explicit effects emitted by Core message handlers.
//!
//! This module is the adapter-first boundary for moving handlers away from
//! directly calling transport/DHT APIs. Handlers describe values in
//! [`CoreEffect`], and [`CoreEffectInterpreter`] applies those values to the
//! current transport implementation.

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
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::SwarmTransport;

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

/// Lower one DHT leaf action directly into a transport effect.
pub(crate) fn lower_dht_action<'payload>(
    act: &PeerRingAction,
    is_connected: impl Fn(Did) -> bool,
) -> Result<Option<CoreEffect<'payload>>> {
    match act {
        PeerRingAction::None => Ok(None),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(did)) => {
            let (next, did) = (*next, *did);
            Ok(if next == did {
                None
            } else {
                Some(CoreEffect::send_direct_message(
                    Message::FindSuccessorSend(FindSuccessorSend {
                        did,
                        strict: false,
                        then: FindSuccessorThen::Report(FindSuccessorReportHandler::Connect),
                    }),
                    next,
                ))
            })
        }
        PeerRingAction::RemoteAction(
            next,
            PeerRingRemoteAction::FindSuccessorForFix { did, index },
        ) => {
            let (next, did, index) = (*next, *did, *index);
            Ok(if next == did {
                None
            } else {
                Some(CoreEffect::send_direct_message(
                    Message::FindSuccessorSend(FindSuccessorSend {
                        did,
                        strict: false,
                        then: FindSuccessorThen::Report(
                            FindSuccessorReportHandler::FixFingerTable { index },
                        ),
                    }),
                    next,
                ))
            })
        }
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
                    Err(Error::PendingConnectionCapacityExceeded { capacity }) => {
                        tracing::debug!(
                            "pending connection pool is full ({capacity}); skipping DHT candidate {peer}"
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
        for effect in effects {
            self.run(effect).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::StorageSyncDestination;
    use crate::dht::StorageSyncPurpose;
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

    #[test]
    fn send_report_message_effect_borrows_payload_and_owns_message() -> Result<()> {
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
    fn reset_destination_effect_borrows_payload_and_next_hop() -> Result<()> {
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
    fn storage_sync_effect_owns_sync_message() -> Result<()> {
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
    fn dht_query_successor_list_connects_before_query() -> Result<()> {
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
    fn dht_query_successor_list_sends_when_connected() -> Result<()> {
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
    fn dht_notify_sends_predecessor_to_target() -> Result<()> {
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
    fn dht_notify_connects_target_before_sending() -> Result<()> {
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
