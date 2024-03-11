#![warn(missing_docs)]
//! Tranposrt management

mod builder;
/// Callback interface for swarm
pub mod callback;

use std::sync::Arc;
use std::sync::RwLock;

use async_trait::async_trait;
pub use builder::SwarmBuilder;

use self::callback::InnerSwarmCallback;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::inspect::SwarmInspect;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::swarm::callback::SharedSwarmCallback;
use crate::transport::SwarmTransport;

/// The transport and dht management.
pub struct Swarm {
    /// Reference of DHT.
    pub(crate) dht: Arc<PeerRing>,
    /// Swarm tansport.
    pub transport: Arc<SwarmTransport>,
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

    fn callback(&self) -> Result<SharedSwarmCallback> {
        Ok(self
            .callback
            .read()
            .map_err(|_| Error::CallbackSyncLockError)?
            .clone())
    }

    fn inner_callback(&self) -> Result<InnerSwarmCallback> {
        Ok(InnerSwarmCallback::new(
            self.transport.clone(),
            self.callback()?,
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

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        self.transport.disconnect(peer).await
    }

    /// Connect a given Did. If the did is already connected, return directly,
    /// else try prepare offer and establish connection by dht.
    /// This function may returns a pending connection or connected connection.
    pub async fn connect(&self, peer: Did) -> Result<()> {
        if peer == self.did() {
            return Err(Error::ShouldNotConnectSelf);
        }
        self.transport.connect(peer, self.inner_callback()?).await
    }

    /// Send [Message] to peer.
    pub async fn send_message(&self, msg: Message, destination: Did) -> Result<uuid::Uuid> {
        self.transport.send_message(msg, destination).await
    }

    /// Check the status of swarm
    pub async fn inspect(&self) -> SwarmInspect {
        SwarmInspect::inspect(self).await
    }
}

/// ConnectionHandshake defined how to connect two connections between two swarms.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ConnectionHandshake {
    /// Creaet new connection and its answer. This function will wrap the offer inside a payload
    /// with verification.
    async fn create_offer(&self, peer: Did) -> Result<MessagePayload>;

    /// Answer the offer of remote connection. This function will verify the answer payload and
    /// will wrap the answer inside a payload with verification.
    async fn answer_offer(&self, offer_payload: MessagePayload) -> Result<MessagePayload>;

    /// Accept the answer of remote connection. This function will verify the answer payload and
    /// will return its did with the connection.
    async fn accept_answer(&self, answer_payload: MessagePayload) -> Result<()>;
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ConnectionHandshake for Swarm {
    async fn create_offer(&self, peer: Did) -> Result<MessagePayload> {
        let offer_msg = self
            .transport
            .prepare_connection_offer(peer, self.inner_callback()?)
            .await?;

        // This payload has fake next_hop.
        // The invoker should fix it before sending.
        let payload = MessagePayload::new_send(
            Message::ConnectNodeSend(offer_msg),
            self.transport.session_sk(),
            self.did(),
            peer,
        )?;

        Ok(payload)
    }

    async fn answer_offer(&self, offer_payload: MessagePayload) -> Result<MessagePayload> {
        if !offer_payload.verify() {
            return Err(Error::VerifySignatureFailed);
        }

        let Message::ConnectNodeSend(msg) = offer_payload.transaction.data()? else {
            return Err(Error::InvalidMessage(
                "Should be ConnectNodeSend".to_string(),
            ));
        };

        let peer = offer_payload.transaction.signer();
        let answer_msg = self
            .transport
            .answer_remote_connection(peer, self.inner_callback()?, &msg)
            .await?;

        // This payload has fake next_hop.
        // The invoker should fix it before sending.
        let answer_payload = MessagePayload::new_send(
            Message::ConnectNodeReport(answer_msg),
            self.transport.session_sk(),
            self.did(),
            self.did(),
        )?;

        Ok(answer_payload)
    }

    async fn accept_answer(&self, answer_payload: MessagePayload) -> Result<()> {
        if !answer_payload.verify() {
            return Err(Error::VerifySignatureFailed);
        }

        let Message::ConnectNodeReport(ref msg) = answer_payload.transaction.data()? else {
            return Err(Error::InvalidMessage(
                "Should be ConnectNodeReport".to_string(),
            ));
        };

        let peer = answer_payload.transaction.signer();
        self.transport.accept_remote_connection(peer, msg).await
    }
}
