#![warn(missing_docs)]
//! This module provider [SwarmBuilder] and it's interface for
//! [Swarm]

use std::sync::Arc;
use std::sync::RwLock;

use rings_transport::webrtc_config::WebrtcUdpPortRange;

use crate::chunk::ReassemblyLimits;
use crate::dht::EntryStorage;
use crate::dht::PeerRing;
use crate::dht::VirtualNodeConfig;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use crate::measure::MeasureImpl;
use crate::session::SessionSk;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::SwarmTransportSettings;
use crate::swarm::transport::SwarmWebrtcConfig;
use crate::swarm::Swarm;

struct DefaultCallback;
impl SwarmCallback for DefaultCallback {}

/// Creates a SwarmBuilder to configure a Swarm.
pub struct SwarmBuilder {
    network_id: u32,
    ice_servers: String,
    external_address: Option<String>,
    webrtc_udp_port_range: Option<WebrtcUdpPortRange>,
    dht_succ_max: u8,
    dht_finger_table_size: usize,
    dht_storage_redundancy: u16,
    dht_virtual_nodes: u16,
    reassembly_limits: ReassemblyLimits,
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
            webrtc_udp_port_range: None,
            dht_succ_max: 3,
            dht_finger_table_size: DEFAULT_FINGER_TABLE_SIZE,
            dht_storage_redundancy: 1,
            dht_virtual_nodes: DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER,
            reassembly_limits: ReassemblyLimits::production(),
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

    /// Sets storage-only Chord virtual positions derived per physical peer.
    ///
    /// By default, Rings follows the Chord paper's O(log N) virtual-node
    /// guidance through [`crate::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER`].
    /// A value of zero disables virtual-node storage ownership. Values above
    /// [`crate::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER`] are normalized
    /// once during [`Self::build`]. The same bounded value is used for both
    /// storage ownership and advertised protocol mode.
    pub fn dht_virtual_nodes(mut self, positions_per_peer: u16) -> Self {
        self.dht_virtual_nodes = positions_per_peer;
        self
    }

    /// Sets inbound chunk reassembly limits.
    pub fn reassembly_limits(mut self, limits: ReassemblyLimits) -> Self {
        self.reassembly_limits = limits;
        self
    }

    /// Sets up the external address for swarm transport.
    /// This will be used to configure the transport to listen for WebRTC connections in "HOST" mode.
    pub fn external_address(mut self, external_address: String) -> Self {
        self.external_address = Some(external_address);
        self
    }

    /// Sets the native WebRTC UDP port range used during ICE gathering.
    ///
    /// Invariant: a present range has already proven `1 <= min <= max`.
    /// Browser transports ignore this native deployment setting.
    pub fn webrtc_udp_port_range(mut self, range: WebrtcUdpPortRange) -> Self {
        self.webrtc_udp_port_range = Some(range);
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
        let storage_virtual_node_config =
            VirtualNodeConfig::new(self.network_id, self.dht_virtual_nodes);

        let dht = Arc::new(
            PeerRing::new_with_storage_finger_table_size_and_virtual_nodes(
                dht_did,
                self.dht_succ_max,
                self.dht_storage,
                self.dht_finger_table_size,
                storage_virtual_node_config,
            ),
        );

        let callback = RwLock::new(
            self.callback
                .unwrap_or_else(|| Arc::new(DefaultCallback {})),
        );

        let transport = Arc::new(SwarmTransport::new(
            self.network_id,
            SwarmWebrtcConfig::new(
                self.ice_servers,
                self.external_address,
                self.webrtc_udp_port_range,
            ),
            self.session_sk,
            dht.clone(),
            self.measure,
            SwarmTransportSettings::new(
                self.dht_storage_redundancy,
                storage_virtual_node_config,
                self.reassembly_limits,
            ),
        ));

        Swarm {
            dht,
            transport,
            callback,
        }
    }
}
