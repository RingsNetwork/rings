#![warn(missing_docs)]

//! This mod is the main entrance of swarm.

mod builder;
/// Callback interface for swarm
pub mod callback;
pub(crate) mod transport;

use std::sync::Arc;
use std::sync::RwLock;

pub use builder::SwarmBuilder;

use self::callback::InnerSwarmCallback;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::Stabilizer;
use crate::error::Error;
use crate::error::Result;
use crate::inspect::ConnectionInspect;
use crate::inspect::SwarmInspect;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::SwarmTransport;

/// The transport and dht management.
pub struct Swarm {
    /// Reference of DHT.
    pub(crate) dht: Arc<PeerRing>,
    /// Swarm tansport.
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

    /// Create [Stabilizer] for swarm.
    pub fn stabilizer(&self) -> Stabilizer {
        Stabilizer::new(self.transport.clone())
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

    /// List peers and their connection status.
    pub fn peers(&self) -> Vec<ConnectionInspect> {
        self.transport
            .get_connections()
            .iter()
            .map(|(did, c)| ConnectionInspect {
                did: did.to_string(),
                state: format!("{:?}", c.webrtc_connection_state()),
            })
            .collect()
    }

    /// Check the status of swarm
    pub async fn inspect(&self) -> SwarmInspect {
        SwarmInspect::inspect(self).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl PayloadSender for Swarm {
    fn session_sk(&self) -> &SessionSk {
        Swarm::session_sk(self)
    }

    fn dht(&self) -> Arc<PeerRing> {
        Swarm::dht(self)
    }

    fn is_connected(&self, did: Did) -> bool {
        let Some(conn) = self.get_connection(did) else {
            return false;
        };
        conn.webrtc_connection_state() == WebrtcConnectionState::Connected
    }

    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        let conn = self
            .get_and_check_connection(did)
            .await
            .ok_or(Error::SwarmMissDidInTable(did))?;

        tracing::debug!(
            "Try send tx {}, to node {:?}",
            payload.transaction.tx_id,
            payload.relay.next_hop,
        );

        let data = payload.to_bincode()?;
        if data.len() > TRANSPORT_MAX_SIZE {
            tracing::error!("Message is too large: {:?}", payload);
            return Err(Error::MessageTooLarge(data.len()));
        }

        let result = if data.len() > TRANSPORT_MTU {
            let chunks = ChunkList::<TRANSPORT_MTU>::from(&data);
            tracing::info!("data split to chunk, len {}", chunks.to_vec().len());
            for chunk in chunks {
                let data =
                    MessagePayload::new_send(Message::Chunk(chunk), &self.session_sk, did, did)?
                        .to_bincode()?;
                conn.send_message(TransportMessage::Custom(data.to_vec()))
                    .await?;
            }
            Ok(())
        } else {
            conn.send_message(TransportMessage::Custom(data.to_vec()))
                .await
        };

        tracing::debug!(
            "Sent {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        if result.is_ok() {
            self.record_sent(payload.relay.next_hop).await
        } else {
            self.record_sent_failed(payload.relay.next_hop).await
        }

        result.map_err(|e| e.into())
    }
}

#[cfg(not(feature = "wasm"))]
impl Swarm {
    /// Creaet new connection and its answer. This function will wrap the offer inside a payload
    /// with verification.
    pub async fn create_offer(&self, peer: Did) -> Result<MessagePayload> {
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

    /// Answer the offer of remote connection. This function will verify the answer payload and
    /// will wrap the answer inside a payload with verification.
    pub async fn answer_offer(&self, offer_payload: MessagePayload) -> Result<MessagePayload> {
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

    /// Accept the answer of remote connection. This function will verify the answer payload and
    /// will return its did with the connection.
    pub async fn accept_answer(&self, answer_payload: MessagePayload) -> Result<()> {
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
