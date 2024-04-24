use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use rings_transport::connection_ref::ConnectionRef;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyConnection as ConnectionOwner;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyTransport as Transport;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcConnection as ConnectionOwner;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcTransport as Transport;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
use rings_transport::connections::WebrtcConnection as ConnectionOwner;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
use rings_transport::connections::WebrtcTransport as Transport;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::TransportMessage;
use rings_transport::core::transport::WebrtcConnectionState;

use crate::chunk::ChunkList;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::consts::TRANSPORT_MTU;
use crate::dht::Did;
use crate::dht::LiveDid;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasureImpl;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::swarm::callback::InnerSwarmCallback;

pub struct SwarmTransport {
    transport: Transport,
    session_sk: SessionSk,
    pub(crate) dht: Arc<PeerRing>,
    #[allow(dead_code)]
    measure: Option<MeasureImpl>,
}

#[derive(Clone)]
pub struct SwarmConnection {
    peer: Did,
    pub connection: ConnectionRef<ConnectionOwner>,
}

impl SwarmTransport {
    pub fn new(
        ice_servers: &str,
        external_address: Option<String>,
        session_sk: SessionSk,
        dht: Arc<PeerRing>,
        measure: Option<MeasureImpl>,
    ) -> Self {
        Self {
            transport: Transport::new(ice_servers, external_address),
            session_sk,
            dht,
            measure,
        }
    }

    /// Create new connection that will be handled by swarm.
    pub async fn new_connection(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        if peer == self.dht.did {
            return Ok(());
        }

        let cid = peer.to_string();
        self.transport
            .new_connection(&cid, Box::new(callback))
            .await
            .map_err(Error::Transport)
    }

    /// Get connection by did.
    pub fn get_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.transport
            .connection(&peer.to_string())
            .map(|conn| SwarmConnection {
                peer,
                connection: conn,
            })
            .ok()
    }

    /// Get all connections in transport.
    pub fn get_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.transport
            .connections()
            .into_iter()
            .filter_map(|(k, v)| {
                Did::from_str(&k).ok().map(|did| {
                    (did, SwarmConnection {
                        peer: did,
                        connection: v,
                    })
                })
            })
            .collect()
    }

    /// Get dids of all connections in transport.
    pub fn get_connection_ids(&self) -> Vec<Did> {
        self.transport
            .connection_ids()
            .into_iter()
            .filter_map(|k| Did::from_str(&k).ok())
            .collect()
    }

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        tracing::info!("removing {peer} from DHT");
        self.dht.remove(peer)?;
        self.transport
            .close_connection(&peer.to_string())
            .await
            .map_err(|e| e.into())
    }

    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    pub async fn connect(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        let offer_msg = self.prepare_connection_offer(peer, callback).await?;
        self.send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await?;
        Ok(())
    }

    /// Get connection by did and check if data channel is open.
    /// This method will return None if the connection is not found.
    /// This method will wait_for_data_channel_open.
    /// If it's not ready in 8 seconds this method will close it and return None.
    /// If it's ready in 8 seconds this method will return the connection.
    /// See more information about [rings_transport::core::transport::WebrtcConnectionState].
    /// See also method webrtc_wait_for_data_channel_open [rings_transport::core::transport::ConnectionInterface].
    pub async fn get_and_check_connection(&self, peer: Did) -> Option<SwarmConnection> {
        let Some(conn) = self.get_connection(peer) else {
            return None;
        };

        if let Err(e) = conn.connection.webrtc_wait_for_data_channel_open().await {
            tracing::warn!(
                "[get_and_check_connection] connection {peer} data channel not open, will be dropped, reason: {e:?}"
            );

            if let Err(e) = self.disconnect(peer).await {
                tracing::error!("Failed on close connection {peer}: {e:?}");
            }

            return None;
        };

        Some(conn)
    }

    /// Create new connection and its offer.
    pub async fn prepare_connection_offer(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
    ) -> Result<ConnectNodeSend> {
        if self.get_and_check_connection(peer).await.is_some() {
            return Err(Error::AlreadyConnected);
        };

        self.new_connection(peer, callback).await?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;

        let offer = conn.webrtc_create_offer().await.map_err(Error::Transport)?;
        let offer_str = serde_json::to_string(&offer).map_err(|_| Error::SerializeToString)?;
        let offer_msg = ConnectNodeSend { sdp: offer_str };

        Ok(offer_msg)
    }

    /// Answer the offer of remote connection.
    pub async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<ConnectNodeReport> {
        let offer = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;

        if let Some(swarm_conn) = self.get_connection(peer) {
            // Solve the scenario of creating offers simultaneously.
            //
            // When both sides create_offer at the same time and trigger answer_offer of the other side,
            // they will got existed New state connection when answer_offer, which will prevent
            // it to create new connection to answer the offer.
            //
            // The party with a larger Did (ranked lower on the ring) should abandon their own offer and instead answer_offer to the other party.
            // The party with a smaller Did should reject answering the other party and report an Error::AlreadyConnected error.
            if swarm_conn.connection.webrtc_connection_state() == WebrtcConnectionState::New {
                // drop local offer and continue answer remote offer
                if self.dht.did > peer {
                    // this connection will replaced by new connection created bellow
                    self.disconnect(peer).await?;
                } else {
                    // ignore remote offer, and refuse to answer remote offer
                    return Err(Error::AlreadyConnected);
                }
            } else if self.get_and_check_connection(peer).await.is_some() {
                return Err(Error::AlreadyConnected);
            };
        };

        self.new_connection(peer, callback).await?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;

        let answer = conn
            .webrtc_answer_offer(offer)
            .await
            .map_err(Error::Transport)?;
        let answer_str = serde_json::to_string(&answer).map_err(|_| Error::SerializeToString)?;
        let answer_msg = ConnectNodeReport { sdp: answer_str };

        Ok(answer_msg)
    }

    /// Accept the answer of remote connection.
    pub async fn accept_remote_connection(
        &self,
        peer: Did,
        answer_msg: &ConnectNodeReport,
    ) -> Result<()> {
        let answer = serde_json::from_str(&answer_msg.sdp).map_err(Error::Deserialize)?;

        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;
        conn.webrtc_accept_answer(answer)
            .await
            .map_err(Error::Transport)?;

        Ok(())
    }
}

impl SwarmConnection {
    pub async fn send_data(&self, data: Bytes) -> Result<()> {
        self.connection
            .send_message(TransportMessage::Custom(data.to_vec()))
            .await
            .map_err(|e| e.into())
    }

    pub fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.connection.webrtc_connection_state()
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl PayloadSender for SwarmTransport {
    fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
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
            "Try send {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        let data = payload.to_bincode()?;
        if data.len() > TRANSPORT_MAX_SIZE {
            tracing::error!("Message is too large: {:?}", payload);
            return Err(Error::MessageTooLarge(data.len()));
        }

        let result = if data.len() > TRANSPORT_MTU {
            let chunks = ChunkList::<TRANSPORT_MTU>::from(&data);
            for chunk in chunks {
                let data =
                    MessagePayload::new_send(Message::Chunk(chunk), &self.session_sk, did, did)?
                        .to_bincode()?;
                conn.send_data(data).await?;
            }
            Ok(())
        } else {
            conn.send_data(data).await
        };

        tracing::debug!(
            "Sent {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        result
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl LiveDid for SwarmConnection {
    async fn live(&self) -> bool {
        self.webrtc_connection_state() == WebrtcConnectionState::Connected
    }
}

impl From<SwarmConnection> for Did {
    fn from(conn: SwarmConnection) -> Self {
        conn.peer
    }
}
