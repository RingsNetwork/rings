use async_trait::async_trait;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasureCounter;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::swarm::Swarm;

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

/// A trait for managing connections.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ConnectionManager {
    /// Asynchronously disconnects the connection associated with the provided DID.
    async fn disconnect(&self, did: Did) -> Result<()>;

    /// Asynchronously establishes a new connection and returns the connection associated with the provided DID.
    async fn connect(&self, did: Did) -> Result<()>;

    /// Asynchronously establishes a new connection via a specified next hop DID and returns the connection associated with the provided DID.
    async fn connect_via(&self, did: Did, next_hop: Did) -> Result<()>;
}

/// A trait for judging whether a connection should be established with a given DID (Decentralized Identifier).
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait Judegement {
    /// Asynchronously checks if a connection should be established with the provided DID.
    async fn should_connect(&self, did: Did) -> bool;

    /// Asynchronously records that a connection has been established with the provided DID.
    async fn record_connect(&self, did: Did);

    /// Asynchronously records that a connection has been disconnected with the provided DID.
    async fn record_disconnected(&self, did: Did);
}

/// A trait that combines the `Judegement` and `ConnectionManager` traits.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait JudgeConnection: Judegement + ConnectionManager {
    /// Asynchronously disconnects the connection associated with the provided DID after recording the disconnection.
    async fn disconnect(&self, did: Did) -> Result<()> {
        self.record_disconnected(did).await;
        tracing::debug!("[JudegeConnection] Disconnected {:?}", &did);
        ConnectionManager::disconnect(self, did).await
    }

    /// Asynchronously establishes a new connection and returns the connection associated with the provided DID if `should_connect` returns true; otherwise, returns an error.
    async fn connect(&self, did: Did) -> Result<()> {
        if !self.should_connect(did).await {
            return Err(Error::NodeBehaviourBad(did));
        }
        tracing::debug!("[JudgeConnection] Try Connect {:?}", &did);
        self.record_connect(did).await;
        ConnectionManager::connect(self, did).await
    }

    /// Asynchronously establishes a new connection via a specified next hop DID and returns the connection associated with the provided DID if `should_connect` returns true; otherwise, returns an error.
    async fn connect_via(&self, did: Did, next_hop: Did) -> Result<()> {
        if !self.should_connect(did).await {
            return Err(Error::NodeBehaviourBad(did));
        }
        tracing::debug!("[JudgeConnection] Try Connect {:?}", &did);
        self.record_connect(did).await;
        ConnectionManager::connect_via(self, did, next_hop).await
    }
}

impl Swarm {
    /// Record a succeeded message sent
    pub async fn record_sent(&self, did: Did) {
        if let Some(measure) = &self.measure {
            measure.incr(did, MeasureCounter::Sent).await;
        }
    }

    /// Record a failed message sent
    pub async fn record_sent_failed(&self, did: Did) {
        if let Some(measure) = &self.measure {
            measure.incr(did, MeasureCounter::FailedToSend).await;
        }
    }

    /// Check that a Did is behaviour good
    pub async fn behaviour_good(&self, did: Did) -> bool {
        if let Some(measure) = &self.measure {
            measure.good(did).await
        } else {
            true
        }
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ConnectionHandshake for Swarm {
    async fn create_offer(&self, peer: Did) -> Result<MessagePayload> {
        let offer_msg = self
            .transport
            .prepare_connection_offer(peer, self.callback()?)
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

        let peer = offer_payload.relay.origin_sender();
        let answer_msg = self
            .transport
            .answer_remote_connection(peer, self.callback()?, &msg)
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

        let peer = answer_payload.relay.origin_sender();
        self.transport.accept_remote_connection(peer, msg).await
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ConnectionManager for Swarm {
    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    async fn disconnect(&self, peer: Did) -> Result<()> {
        self.transport.disconnect(peer).await
    }

    /// Connect a given Did. If the did is already connected, return directly,
    /// else try prepare offer and establish connection by dht.
    /// This function may returns a pending connection or connected connection.
    async fn connect(&self, peer: Did) -> Result<()> {
        let offer_msg = self
            .transport
            .prepare_connection_offer(peer, self.callback()?)
            .await?;
        self.transport
            .send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await?;
        Ok(())
    }

    /// Similar to connect, but this function will try connect a Did by given hop.
    async fn connect_via(&self, peer: Did, next_hop: Did) -> Result<()> {
        let offer_msg = self
            .transport
            .prepare_connection_offer(peer, self.callback()?)
            .await?;

        self.transport
            .send_message_by_hop(Message::ConnectNodeSend(offer_msg), peer, next_hop)
            .await?;

        Ok(())
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl Judegement for Swarm {
    /// Record a succeeded connected
    async fn record_connect(&self, did: Did) {
        tracing::info!("Record connect {:?}", &did);
        if let Some(measure) = &self.measure {
            tracing::info!("[Judgement] Record connect");
            measure.incr(did, MeasureCounter::Connect).await;
        }
    }

    /// Record a disconnected
    async fn record_disconnected(&self, did: Did) {
        tracing::info!("Record disconnected {:?}", &did);
        if let Some(measure) = &self.measure {
            tracing::info!("[Judgement] Record disconnected");
            measure.incr(did, MeasureCounter::Disconnected).await;
        }
    }

    /// Asynchronously checks if a connection should be established with the provided DID.
    async fn should_connect(&self, did: Did) -> bool {
        self.behaviour_good(did).await
    }
}
