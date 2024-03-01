#![warn(missing_docs)]
//! Tranposrt management

mod builder;
/// Callback interface for swarm
pub mod callback;
/// Implementations of connection management traits for swarm
pub mod impls;

use std::sync::Arc;
use std::sync::RwLock;

use async_recursion::async_recursion;
pub use builder::SwarmBuilder;
use rings_derive::JudgeConnection;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::inspect::SwarmInspect;
use crate::measure::BehaviourJudgement;
use crate::message::types::NotifyPredecessorSend;
use crate::message::ChordStorageInterface;
use crate::message::Message;
use crate::message::MessageHandler;
use crate::message::MessageHandlerEvent;
use crate::message::PayloadSender;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SharedSwarmCallback;
use crate::transport::SwarmTransport;

/// Type of Measure, see [crate::measure::Measure].
#[cfg(not(feature = "wasm"))]
pub type MeasureImpl = Box<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [crate::measure::Measure].
#[cfg(feature = "wasm")]
pub type MeasureImpl = Box<dyn BehaviourJudgement>;

/// The transport and dht management.
#[derive(JudgeConnection)]
pub struct Swarm {
    /// Reference of DHT.
    pub(crate) dht: Arc<PeerRing>,
    /// Implementationof measurement.
    pub(crate) measure: Option<MeasureImpl>,
    message_handler: MessageHandler,
    pub(crate) transport: Arc<SwarmTransport>,
    callback: RwLock<SharedSwarmCallback>,
}

impl Swarm {
    /// Get did of self.
    pub fn did(&self) -> Did {
        self.dht.did
    }

    /// Get DHT(Distributed Hash Table) of self.
    pub fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    fn callback(&self) -> Result<InnerSwarmCallback> {
        let shared = self
            .callback
            .read()
            .map_err(|_| Error::CallbackSyncLockError)?
            .clone();

        Ok(InnerSwarmCallback::new(
            self.did(),
            self.transport.clone(),
            shared,
        ))
    }

    /// Set callback for swarm.
    pub fn set_callback(&self, callback: SharedSwarmCallback) -> Result<()> {
        let mut inner = self
            .callback
            .write()
            .map_err(|_| Error::CallbackSyncLockError)?;

        *inner = callback;

        Ok(())
    }

    /// Event handler of Swarm.
    pub async fn handle_message_handler_event(
        &self,
        event: &MessageHandlerEvent,
    ) -> Result<Vec<MessageHandlerEvent>> {
        tracing::debug!("Handle message handler event: {:?}", event);
        match event {
            MessageHandlerEvent::Connect(did) => {
                let did = *did;
                if did != self.did() {
                    self.connect(did).await?;
                }
                Ok(vec![])
            }

            // Notify did with self.id
            MessageHandlerEvent::Notify(did) => {
                let msg =
                    Message::NotifyPredecessorSend(NotifyPredecessorSend { did: self.dht.did });
                Ok(vec![MessageHandlerEvent::SendMessage(msg, *did)])
            }

            MessageHandlerEvent::ConnectVia(did, next) => {
                let did = *did;
                if did != self.did() {
                    self.connect_via(did, *next).await?;
                }
                Ok(vec![])
            }

            MessageHandlerEvent::AnswerOffer(relay, msg) => {
                let answer = self
                    .transport
                    .answer_remote_connection(relay.relay.origin_sender(), self.callback()?, msg)
                    .await?;

                Ok(vec![MessageHandlerEvent::SendReportMessage(
                    relay.clone(),
                    Message::ConnectNodeReport(answer),
                )])
            }

            MessageHandlerEvent::AcceptAnswer(origin_sender, msg) => {
                self.transport
                    .accept_remote_connection(origin_sender.to_owned(), msg)
                    .await?;
                Ok(vec![])
            }

            MessageHandlerEvent::ForwardPayload(payload, next_hop) => {
                self.transport.forward_payload(payload, *next_hop).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::SendDirectMessage(msg, dest) => {
                self.transport
                    .send_direct_message(msg.clone(), *dest)
                    .await?;
                Ok(vec![])
            }

            MessageHandlerEvent::SendMessage(msg, dest) => {
                self.transport.send_message(msg.clone(), *dest).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::SendReportMessage(payload, msg) => {
                self.transport
                    .send_report_message(payload, msg.clone())
                    .await?;
                Ok(vec![])
            }

            MessageHandlerEvent::ResetDestination(payload, next_hop) => {
                self.transport.reset_destination(payload, *next_hop).await?;
                Ok(vec![])
            }

            MessageHandlerEvent::StorageStore(vnode) => {
                <Self as ChordStorageInterface<1>>::storage_store(self, vnode.clone()).await?;
                Ok(vec![])
            }
        }
    }

    /// Batch handle events
    #[cfg_attr(feature = "wasm", async_recursion(?Send))]
    #[cfg_attr(not(feature = "wasm"), async_recursion)]
    pub async fn handle_message_handler_events(
        &self,
        events: &Vec<MessageHandlerEvent>,
    ) -> Result<()> {
        match events.as_slice() {
            [] => Ok(()),
            [x, xs @ ..] => {
                let evs = self.handle_message_handler_event(x).await?;
                self.handle_message_handler_events(&evs).await?;
                self.handle_message_handler_events(&xs.to_vec()).await
            }
        }
    }

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        JudgeConnection::disconnect(self, did).await
    }

    /// Connect a given Did. If the did is already connected, return directly,
    /// else try prepare offer and establish connection by dht.
    /// This function may returns a pending connection or connected connection.
    pub async fn connect(&self, did: Did) -> Result<()> {
        if did == self.did() {
            return Err(Error::ShouldNotConnectSelf);
        }
        JudgeConnection::connect(self, did).await
    }

    /// Similar to connect, but this function will try connect a Did by given hop.
    pub async fn connect_via(&self, did: Did, next_hop: Did) -> Result<()> {
        if did == self.did() {
            return Err(Error::ShouldNotConnectSelf);
        }
        JudgeConnection::connect_via(self, did, next_hop).await
    }

    /// Check the status of swarm
    pub async fn inspect(&self) -> SwarmInspect {
        SwarmInspect::inspect(self).await
    }
}
