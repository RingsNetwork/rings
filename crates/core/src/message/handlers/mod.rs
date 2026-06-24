#![warn(missing_docs)]
//! This module implemented message handler of rings network.

use std::sync::Arc;

use async_recursion::async_recursion;
use async_trait::async_trait;

use super::MessagePayload;
use crate::dht::CorrectChord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::FindSuccessorSend;
use crate::message::types::Message;
use crate::message::types::QueryForTopoInfoSend;
use crate::message::FindSuccessorReportHandler;
use crate::message::FindSuccessorThen;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::SwarmTransport;

/// Operator and Handler for Connection
pub mod connection;
/// Operator and Handler for CustomMessage
pub mod custom;
/// Operator and handler for DHT stabilization
pub mod stabilization;
/// Operator and Handler for Storage
pub mod storage;
/// Operator and Handler for Subring
pub mod subring;

/// MessageHandler will manage resources.
#[derive(Clone)]
pub struct MessageHandler {
    transport: Arc<SwarmTransport>,
    dht: Arc<PeerRing>,
    swarm_callback: SharedSwarmCallback,
}

/// Generic trait for handle message ,inspired by Actor-Model.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait HandleMsg<T> {
    /// Message handler.
    async fn handle(&self, ctx: &MessagePayload, msg: &T) -> Result<()>;
}

impl MessageHandler {
    /// Create a new MessageHandler instance.
    pub fn new(transport: Arc<SwarmTransport>, swarm_callback: SharedSwarmCallback) -> Self {
        let dht = transport.dht.clone();
        Self {
            transport,
            dht,
            swarm_callback,
        }
    }

    fn inner_callback(&self) -> InnerSwarmCallback {
        InnerSwarmCallback::new(self.transport.clone(), self.swarm_callback.clone())
    }

    /// Idempotently establish a DHT-driven transport connection.
    ///
    /// Self and already-connected peers are no-ops. `AlreadyConnected` is treated
    /// as success so concurrent DHT actions racing through `MultiActions` do not
    /// fail the whole handler.
    pub(crate) async fn connect_dht_peer(&self, peer: Did) -> Result<()> {
        if peer == self.dht.did || self.transport.get_connection(peer).is_some() {
            return Ok(());
        }

        match self.transport.connect(peer, self.inner_callback()).await {
            Ok(()) | Err(Error::AlreadyConnected) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn join_dht(&self, peer: Did) -> Result<()> {
        // Default HMCC/Zave join path: maps to the JoinThenSync operation in
        // the CorrectChord spec (see tests/default/dht_convergence.rs).
        let conn = self
            .transport
            .get_connection(peer)
            .ok_or(Error::SwarmMissDidInTable(peer))?;
        let dht_ev = self.dht.join_then_sync(conn).await?;
        // The local join has completed. Follow-up convergence messages are
        // best-effort: a peer can churn before these sends complete, and that
        // must not suppress the application-level Connected event.
        if let Err(e) = self.handle_dht_events(&dht_ev).await {
            tracing::warn!("Failed to handle dht events while joining {peer}: {e:?}");
        }
        Ok(())
    }

    pub(crate) async fn leave_dht(&self, peer: Did) -> Result<()> {
        if self
            .transport
            .get_and_check_connection(peer)
            .await
            .is_none()
        {
            self.dht.remove(peer)?;
        };
        Ok(())
    }

    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub(crate) async fn handle_dht_events(&self, act: &PeerRingAction) -> Result<()> {
        match act {
            PeerRingAction::None => Ok(()),
            // Ask next hop to find successor for did,
            // if there is only two nodes A, B, it may cause loop, for example
            // A's successor is B, B ask A to find successor for B
            // A may send message to it's successor, which is B
            PeerRingAction::RemoteAction(
                next,
                PeerRingRemoteAction::FindSuccessorForConnect(did),
            ) => {
                if next != did {
                    self.transport
                        .send_direct_message(
                            Message::FindSuccessorSend(FindSuccessorSend {
                                did: *did,
                                strict: false,
                                then: FindSuccessorThen::Report(
                                    FindSuccessorReportHandler::Connect,
                                ),
                            }),
                            *next,
                        )
                        .await?;
                    Ok(())
                } else {
                    Ok(())
                }
            }
            // A new successor is set, request the new successor for it's successor list
            PeerRingAction::RemoteAction(next, PeerRingRemoteAction::QueryForSuccessorList) => {
                if !self.transport.is_connected(*next) {
                    self.connect_dht_peer(*next).await?;
                    return Ok(());
                }
                self.transport
                    .send_direct_message(
                        Message::QueryForTopoInfoSend(QueryForTopoInfoSend::new_for_sync(*next)),
                        *next,
                    )
                    .await?;
                Ok(())
            }
            PeerRingAction::RemoteAction(did, PeerRingRemoteAction::TryConnect) => {
                self.connect_dht_peer(*did).await?;
                Ok(())
            }
            PeerRingAction::RemoteAction(did, PeerRingRemoteAction::Notify(predecessor)) => {
                if did == predecessor {
                    tracing::warn!("Notify target is equal to predecessor, may implement wrong.");
                    return Ok(());
                }
                if !self.transport.is_connected(*did) {
                    self.connect_dht_peer(*did).await?;
                    return Ok(());
                }
                // `RemoteAction(target, Notify(pred))` means "send pred to target"
                // and maps to CorrectStabilize.notify' in the TLA+ spec mirror.
                let msg =
                    Message::NotifyPredecessorSend(NotifyPredecessorSend { did: *predecessor });
                self.transport.send_message(msg, *did).await?;
                Ok(())
            }
            PeerRingAction::MultiActions(acts) => {
                let jobs = acts
                    .iter()
                    .map(|act| async move { self.handle_dht_events(act).await });

                for res in futures::future::join_all(jobs).await {
                    if res.is_err() {
                        tracing::error!("Failed on handle multi actions: {:#?}", res)
                    }
                }

                Ok(())
            }
            _ => unreachable!(),
        }
    }
}
