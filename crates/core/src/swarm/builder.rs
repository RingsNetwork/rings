#![warn(missing_docs)]
//! This module provider [SwarmBuilder] and it's interface for
//! [Swarm]

use std::sync::Arc;
use std::sync::RwLock;

use crate::dht::PeerRing;
use crate::dht::VNodeStorage;
use crate::message::MessageHandler;
use crate::session::SessionSk;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::MeasureImpl;
use crate::swarm::Swarm;
use crate::transport::SwarmTransport;

struct DefaultCallback;
impl SwarmCallback for DefaultCallback {}

/// Creates a SwarmBuilder to configure a Swarm.
pub struct SwarmBuilder {
    ice_servers: String,
    external_address: Option<String>,
    dht_succ_max: u8,
    dht_storage: VNodeStorage,
    session_sk: SessionSk,
    session_ttl: Option<usize>,
    measure: Option<MeasureImpl>,
    callback: Option<SharedSwarmCallback>,
}

impl SwarmBuilder {
    /// Creates new instance of [SwarmBuilder]
    pub fn new(ice_servers: &str, dht_storage: VNodeStorage, session_sk: SessionSk) -> Self {
        SwarmBuilder {
            ice_servers: ice_servers.to_string(),
            external_address: None,
            dht_succ_max: 3,
            dht_storage,
            session_sk,
            session_ttl: None,
            measure: None,
            callback: None,
        }
    }

    /// Sets up the maximum length of successors in the DHT.
    pub fn dht_succ_max(mut self, succ_max: u8) -> Self {
        self.dht_succ_max = succ_max;
        self
    }

    /// Sets up the external address for swarm transport.
    /// This will be used to configure the transport to listen for WebRTC connections in "HOST" mode.
    pub fn external_address(mut self, external_address: String) -> Self {
        self.external_address = Some(external_address);
        self
    }

    /// Setup timeout for session.
    pub fn session_ttl(mut self, ttl: usize) -> Self {
        self.session_ttl = Some(ttl);
        self
    }

    /// Bind measurement function for Swarm.
    pub fn measure(mut self, implement: MeasureImpl) -> Self {
        self.measure = Some(implement);
        self
    }

    /// Bind callback for Swarm.
    pub fn callback(mut self, callback: SharedSwarmCallback) -> Self {
        self.callback = Some(callback);
        self
    }

    /// Try build for `Swarm`.
    pub fn build(self) -> Swarm {
        let dht_did = self.session_sk.account_did();

        let dht = Arc::new(PeerRing::new_with_storage(
            dht_did,
            self.dht_succ_max,
            self.dht_storage,
        ));

        let transport = Arc::new(SwarmTransport::new(
            &self.ice_servers,
            self.external_address,
            dht.clone(),
            self.session_sk,
        ));

        let callback = RwLock::new(
            self.callback
                .unwrap_or_else(|| Arc::new(DefaultCallback {})),
        );

        let message_handler = MessageHandler::new(transport.clone());

        Swarm {
            dht,
            measure: self.measure,
            message_handler,
            transport,
            callback,
        }
    }
}
