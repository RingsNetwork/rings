//! Typed failures exposed by gateway domain and effect boundaries.

use std::io;
use std::net::SocketAddr;

use thiserror::Error;

use crate::flow::FlowEvent;
use crate::flow::FlowId;
use crate::flow::FlowState;
use crate::server::GatewayEvent;
use crate::server::GatewayState;

/// Invalid gateway configuration.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// The configured maximum flow count is zero.
    #[error("gateway max_flows must be greater than zero")]
    ZeroMaxFlows,
    /// The configured flow idle timeout is zero.
    #[error("gateway flow_idle_timeout must be greater than zero")]
    ZeroFlowIdleTimeout,
    /// The configured per-flow TCP buffer size is zero.
    #[error("gateway tcp_buffer_bytes must be greater than zero")]
    ZeroTcpBuffer,
    /// The configured interface MTU is outside the IPv4 packet range.
    #[error("gateway MTU {0} is outside the supported IPv4 range 576..=65535")]
    InvalidMtu(u32),
    /// An enabled routing mode has no included routes.
    #[error("gateway routing mode {0:?} requires at least one included route")]
    MissingIncludedRoute(crate::RoutingMode),
    /// One network is configured as both included and excluded.
    #[error("gateway route {0} is both included and excluded")]
    ConflictingRoute(ipnet::IpNet),
    /// An IPv6 route was supplied to the IPv4/TCP milestone.
    #[error("gateway IPv4/TCP milestone does not accept IPv6 route {0}")]
    Ipv6RouteUnsupported(ipnet::IpNet),
    /// The packet stack has no IPv4 interface address to bind.
    #[error("gateway plan requires at least one IPv4 interface address")]
    MissingInterfaceAddress,
    /// A virtual-interface address is not valid unicast IPv4.
    #[error("gateway interface address {0} is not unicast IPv4")]
    InvalidInterfaceAddress(ipnet::IpNet),
    /// A DNS policy was requested without explicit resolver destinations.
    #[error("gateway DNS policy {0:?} requires at least one explicit IPv4 resolver")]
    MissingDnsServer(crate::DnsPolicy),
    /// An IPv6 DNS resolver was supplied to the IPv4-only milestone.
    #[error("gateway IPv4/TCP milestone does not accept IPv6 DNS resolver {0}")]
    Ipv6DnsUnsupported(std::net::IpAddr),
}

/// A rejected flow-state transition.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("flow event {event:?} is invalid while flow is {state:?}")]
pub struct FlowTransitionError {
    /// State that rejected the transition.
    pub state: FlowState,
    /// Rejected event.
    pub event: FlowEvent,
}

/// A rejected flow-table operation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum FlowTableError {
    /// A packet attempted to capture a flow that is already tracked.
    #[error("gateway flow {0:?} is already tracked")]
    Duplicate(FlowId),
    /// An event referred to a flow that is not tracked.
    #[error("gateway flow {0:?} is not tracked")]
    NotFound(FlowId),
    /// The configured concurrent flow bound has been reached.
    #[error("gateway flow capacity {limit} is exhausted")]
    CapacityExhausted {
        /// Configured maximum concurrent flow count.
        limit: usize,
    },
    /// The tracked flow rejected a lifecycle event.
    #[error(transparent)]
    Transition(#[from] FlowTransitionError),
}

/// A rejected gateway-state transition.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("gateway event {event:?} is invalid while gateway is {state:?}")]
pub struct GatewayTransitionError {
    /// State that rejected the transition.
    pub state: GatewayState,
    /// Rejected event.
    pub event: GatewayEvent,
}

/// Packet-device IO failure.
#[derive(Debug, Error)]
pub enum PacketIoError {
    /// Reading a captured packet failed.
    #[error("failed to read a packet from gateway device: {0}")]
    Read(#[source] io::Error),
    /// Writing an injected packet failed.
    #[error("failed to write a packet to gateway device: {0}")]
    Write(#[source] io::Error),
    /// A packet supplied by the device violates the packet contract.
    #[error(
        "gateway device returned invalid packet length {length} for buffer capacity {capacity}"
    )]
    InvalidLength {
        /// Length reported by the device.
        length: usize,
        /// Capacity of the supplied packet buffer.
        capacity: usize,
    },
    /// The packet device accepted only a prefix of one packet.
    #[error("gateway device wrote {written} of {expected} packet bytes")]
    PartialWrite {
        /// Complete packet length.
        expected: usize,
        /// Bytes accepted by the device.
        written: usize,
    },
    /// The packet device has been closed.
    #[error("gateway packet device is closed")]
    Closed,
}

/// Rejected packet at the IPv4/TCP parser boundary.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PacketParseError {
    /// The packet is shorter than its declared IPv4 header or total length.
    #[error("malformed IPv4 packet")]
    MalformedIpv4,
    /// A non-IPv4 packet reached the IPv4/TCP milestone.
    #[error("non-IPv4 packet reached the IPv4/TCP gateway")]
    NonIpv4,
    /// IPv4 fragmentation is not supported by this milestone.
    #[error("fragmented IPv4 packet is unsupported")]
    FragmentedIpv4,
    /// The TCP header is malformed.
    #[error("malformed TCP segment")]
    MalformedTcp,
    /// TCP port zero cannot identify an admitted flow.
    #[error("TCP source and destination ports must be nonzero")]
    ZeroTcpPort,
}

/// Failure while adapting captured packets to the shared userspace TCP stack.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TcpStackError {
    /// The first packet for a flow was not an initial SYN.
    #[error("gateway TCP flow {0:?} did not begin with an initial SYN")]
    MissingInitialSyn(FlowId),
    /// An IPv6 flow reached the IPv4-only TCP stack.
    #[error("gateway TCP flow {0:?} is not IPv4")]
    NonIpv4Flow(FlowId),
    /// The configured concurrent TCP endpoint limit has been reached.
    #[error("gateway TCP endpoint capacity {limit} is exhausted")]
    TcpCapacityExhausted {
        /// Configured maximum concurrent endpoint count.
        limit: usize,
    },
    /// A packet referred to a flow that is not tracked by the TCP stack.
    #[error("gateway TCP flow {0:?} is not tracked by the TCP stack")]
    UnknownFlow(FlowId),
    /// The TCP listener rejected the captured destination endpoint.
    #[error("gateway TCP listener rejected target {0}")]
    ListenRejected(SocketAddr),
    /// The interface route table could not install the AnyIP capture route.
    #[error("gateway TCP interface route table is full")]
    RouteTableFull,
    /// The userspace TCP interface could not install its validated address.
    #[error("gateway TCP interface address table is full")]
    InterfaceAddressTableFull,
    /// The flow cannot currently accept application data.
    #[error("gateway TCP flow {0:?} cannot currently send application data")]
    SendUnavailable(FlowId),
    /// The flow cannot currently provide application data.
    #[error("gateway TCP flow {0:?} cannot currently receive application data")]
    ReceiveUnavailable(FlowId),
    /// A previously peeked TCP receive prefix could not be consumed atomically.
    #[error(
        "gateway TCP flow {flow:?} receive commit mismatch: expected {expected} bytes, got {actual}"
    )]
    ReceiveCommitMismatch {
        /// Immutable flow identity.
        flow: FlowId,
        /// Peeked prefix length.
        expected: usize,
        /// Consumed prefix length.
        actual: usize,
    },
}

/// Top-level gateway runtime failure.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// Gateway configuration is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A flow transition violates the lifecycle model.
    #[error(transparent)]
    FlowTransition(#[from] FlowTransitionError),
    /// A gateway transition violates the lifecycle model.
    #[error(transparent)]
    GatewayTransition(#[from] GatewayTransitionError),
    /// A flow-table operation failed.
    #[error(transparent)]
    FlowTable(#[from] FlowTableError),
    /// Packet-device IO failed.
    #[error(transparent)]
    PacketIo(#[from] PacketIoError),
    /// Captured packet parsing failed closed.
    #[error(transparent)]
    PacketParse(#[from] PacketParseError),
    /// Shared userspace TCP processing failed.
    #[error(transparent)]
    TcpStack(#[from] TcpStackError),
    /// Packet admission was attempted outside the active gateway state.
    #[error("gateway does not admit packets while it is {0:?}")]
    PacketAdmissionClosed(GatewayState),
    /// Onion route construction or stream opening failed for an admitted target.
    #[error("failed to open Onion stream for {target}: {message}")]
    OnionUnavailable {
        /// Immutable flow target.
        target: SocketAddr,
        /// Node-layer diagnostic.
        message: String,
    },
    /// A reconstructed TCP-to-Onion duplex operation failed.
    #[error("gateway flow {flow:?} stream operation {operation} failed: {message}")]
    StreamIo {
        /// Immutable flow identity.
        flow: FlowId,
        /// Stable operation label.
        operation: &'static str,
        /// Runtime diagnostic.
        message: String,
    },
    /// Runtime processing failed and fail-closed shutdown also reported an error.
    #[error("gateway runtime failed: {runtime}; shutdown cleanup also failed: {cleanup}")]
    RuntimeCleanup {
        /// Failure that caused the runtime loop to stop.
        #[source]
        runtime: Box<GatewayError>,
        /// First failure observed while still attempting all shutdown steps.
        cleanup: Box<GatewayError>,
    },
    /// A platform binding failed at a named operation.
    #[error("gateway platform operation {operation} failed: {message}")]
    Platform {
        /// Stable operation label.
        operation: &'static str,
        /// Platform diagnostic.
        message: String,
    },
}
