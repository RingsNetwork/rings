#![warn(missing_docs)]
//! This module provider [SwarmBuilder] and it's interface for
//! [Swarm]

use std::sync::Arc;
use std::sync::RwLock;

use crate::dht::EntryStorage;
use crate::dht::PeerRing;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::measure::MeasureImpl;
use crate::session::SessionSk;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;

struct DefaultCallback;
impl SwarmCallback for DefaultCallback {}

/// Creates a SwarmBuilder to configure a Swarm.
pub struct SwarmBuilder {
    network_id: u32,
    ice_servers: String,
    external_address: Option<String>,
    dht_succ_max: u8,
    dht_finger_table_size: usize,
    dht_storage_redundancy: u16,
    dht_storage: EntryStorage,
    session_sk: SessionSk,
    session_ttl: Option<usize>,
    measure: Option<MeasureImpl>,
    callback: Option<SharedSwarmCallback>,
}

impl SwarmBuilder {
    /// Creates new instance of [SwarmBuilder]
    pub fn new(
        network_id: u32,
        ice_servers: &str,
        dht_storage: EntryStorage,
        session_sk: SessionSk,
    ) -> Self {
        SwarmBuilder {
            network_id,
            ice_servers: ice_servers.to_string(),
            external_address: None,
            dht_succ_max: 3,
            dht_finger_table_size: DEFAULT_FINGER_TABLE_SIZE,
            dht_storage_redundancy: 1,
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

    /// Sets up the number of slots in the DHT finger table.
    ///
    /// `Did` is 160-bit, so values above `DEFAULT_FINGER_TABLE_SIZE` are clamped
    /// by `FingerTable::new`. A size of zero disables finger maintenance.
    pub fn dht_finger_table_size(mut self, size: usize) -> Self {
        self.dht_finger_table_size = size;
        self
    }

    /// Sets up the redundancy used by storage repair and anti-entropy.
    pub fn dht_storage_redundancy(mut self, redundancy: u16) -> Self {
        self.dht_storage_redundancy = redundancy;
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

        let dht = Arc::new(PeerRing::new_with_storage_and_finger_table_size(
            dht_did,
            self.dht_succ_max,
            self.dht_storage,
            self.dht_finger_table_size,
        ));

        let callback = RwLock::new(
            self.callback
                .unwrap_or_else(|| Arc::new(DefaultCallback {})),
        );

        let transport = Arc::new(SwarmTransport::new(
            self.network_id,
            &self.ice_servers,
            self.external_address,
            self.session_sk,
            dht.clone(),
            self.measure,
            self.dht_storage_redundancy,
        ));

        Swarm {
            dht,
            transport,
            callback,
        }
    }
}
