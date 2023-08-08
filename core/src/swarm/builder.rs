#![warn(missing_docs)]
//! This module provider [SwarmBuilder] and it's interface for
//! [Swarm]

use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use crate::channels::Channel;
use crate::dht::PeerRing;
use crate::message::CallbackFn;
use crate::message::MessageHandler;
use crate::message::ValidatorFn;
use crate::session::DelegateeSk;
use crate::storage::MemStorage;
use crate::storage::PersistenceStorage;
use crate::swarm::MeasureImpl;
use crate::swarm::Swarm;
use crate::types::channel::Channel as ChannelTrait;
use crate::types::ice_transport::IceServer;

/// Creates a SwarmBuilder to configure a Swarm.
pub struct SwarmBuilder {
    ice_servers: Vec<IceServer>,
    external_address: Option<String>,
    dht_succ_max: u8,
    dht_storage: PersistenceStorage,
    delegatee_sk: DelegateeSk,
    session_ttl: Option<usize>,
    measure: Option<MeasureImpl>,
    message_callback: Option<CallbackFn>,
    message_validator: Option<ValidatorFn>,
}

impl SwarmBuilder {
    /// Creates new instance of [SwarmBuilder]
    pub fn new(
        ice_servers: &str,
        dht_storage: PersistenceStorage,
        delegatee_sk: DelegateeSk,
    ) -> Self {
        let ice_servers = ice_servers
            .split(';')
            .collect::<Vec<&str>>()
            .into_iter()
            .map(|s| {
                IceServer::from_str(s)
                    .unwrap_or_else(|_| panic!("Failed on parse ice server {:?}", s))
            })
            .collect::<Vec<IceServer>>();
        SwarmBuilder {
            ice_servers,
            external_address: None,
            dht_succ_max: 3,
            dht_storage,
            delegatee_sk,
            session_ttl: None,
            measure: None,
            message_callback: None,
            message_validator: None,
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

    /// Bind message callback function for Swarm.
    pub fn message_callback(mut self, callback: CallbackFn) -> Self {
        self.message_callback = Some(callback);
        self
    }

    /// Bind message vilidator function implementation for Swarm.
    pub fn message_validator(mut self, validator: ValidatorFn) -> Self {
        self.message_validator = Some(validator);
        self
    }

    /// Try build for `Swarm`.
    pub fn build(self) -> Swarm {
        let dht_did = self.delegatee_sk.authorizer_did();

        let dht = Arc::new(PeerRing::new_with_storage(
            dht_did,
            self.dht_succ_max,
            self.dht_storage,
        ));

        let message_handler =
            MessageHandler::new(dht.clone(), self.message_callback, self.message_validator);

        Swarm {
            pending_transports: Mutex::new(vec![]),
            transports: MemStorage::new(),
            transport_event_channel: Channel::new(),
            ice_servers: self.ice_servers,
            external_address: self.external_address,
            dht,
            measure: self.measure,
            delegatee_sk: self.delegatee_sk,
            message_handler,
        }
    }
}
