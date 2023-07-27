#![warn(missing_docs)]
//! This module defined TransportManager trait

use async_trait::async_trait;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;

/// TransportManager trait use to manage transports in swarm.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TransportManager {
    /// Transport type.
    type Transport;

    /// Get all transports in swarm.
    fn get_transports(&self) -> Vec<(Did, Self::Transport)>;
    /// Get dids of all transports in swarm.
    fn get_dids(&self) -> Vec<Did>;
    /// Get transport by did.
    fn get_transport(&self, did: Did) -> Option<Self::Transport>;
    /// Remove transport by did.
    fn remove_transport(&self, did: Did) -> Option<(Did, Self::Transport)>;
    /// Get transport by did and check if it is connected.
    async fn get_and_check_transport(&self, did: Did) -> Option<Self::Transport>;
    /// Create new transport that will be handled by swarm.
    async fn new_transport(&self) -> Result<Self::Transport>;
    /// Register transport to swarm.
    async fn register(&self, did: Did, trans: Self::Transport) -> Result<()>;
}

/// TransportHandshake defined how to connect two transports between two swarms.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait TransportHandshake {
    /// Transport type.
    type Transport;
    /// To wrap sdp with verification and did. And also make it serializable.
    type Payload;

    /// Create new transport and its offer.
    async fn prepare_transport_offer(&self) -> Result<(Self::Transport, ConnectNodeSend)>;
    /// Answer the offer of remote transport.
    async fn answer_remote_transport(
        &self,
        did: Did,
        offer_msg: &ConnectNodeSend,
    ) -> Result<(Self::Transport, ConnectNodeReport)>;
    /// Creaet new transport and its answer. This function will wrap the offer inside a payload
    /// with verification.
    async fn create_offer(&self) -> Result<(Self::Transport, Self::Payload)>;
    /// Answer the offer of remote transport. This function will verify the answer payload and
    /// will wrap the answer inside a payload with verification.
    async fn answer_offer(
        &self,
        offer_payload: Self::Payload,
    ) -> Result<(Self::Transport, Self::Payload)>;
    /// Accept the answer of remote transport. This function will verify the answer payload and
    /// will return its did with the transport.
    async fn accept_answer(&self, answer_payload: Self::Payload) -> Result<(Did, Self::Transport)>;
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

/// A trait for managing connections and handling transports, extending the `TransportManager` trait.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ConnectionManager: TransportManager {
    /// Asynchronously disconnects the transport associated with the provided DID.
    async fn disconnect(&self, did: Did) -> Result<()>;

    /// Asynchronously establishes a new connection and returns the transport associated with the provided DID.
    async fn connect(&self, did: Did) -> Result<Self::Transport>;

    /// Asynchronously establishes a new connection via a specified next hop DID and returns the transport associated with the provided DID.
    async fn connect_via(&self, did: Did, next_hop: Did) -> Result<Self::Transport>;
}

/// A trait that combines the `Judegement` and `ConnectionManager` traits.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait JudgeConnection: Judegement + ConnectionManager {
    /// Asynchronously disconnects the transport associated with the provided DID after recording the disconnection.
    async fn disconnect(&self, did: Did) -> Result<()> {
        self.record_disconnected(did).await;
	tracing::debug!("[JudegeConnection] Disconnected {:?}", &did);
        ConnectionManager::disconnect(self, did).await
    }

    /// Asynchronously establishes a new connection and returns the transport associated with the provided DID if `should_connect` returns true; otherwise, returns an error.
    async fn connect(&self, did: Did) -> Result<Self::Transport> {
        if !self.should_connect(did).await {
            return Err(Error::NodeBehaviourBad(did));
        }
	tracing::debug!("[JudgeConnection] Try Connect {:?}", &did);
        self.record_connect(did).await;
        ConnectionManager::connect(self, did).await
    }

    /// Asynchronously establishes a new connection via a specified next hop DID and returns the transport associated with the provided DID if `should_connect` returns true; otherwise, returns an error.
    async fn connect_via(&self, did: Did, next_hop: Did) -> Result<Self::Transport> {
        if !self.should_connect(did).await {
            return Err(Error::NodeBehaviourBad(did));
        }
	tracing::debug!("[JudgeConnection] Try Connect {:?}", &did);
        self.record_connect(did).await;
        ConnectionManager::connect_via(self, did, next_hop).await
    }
}
