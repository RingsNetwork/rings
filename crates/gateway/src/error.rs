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
    /// The configured maximum flow count exceeds the implementation bound.
    #[error("gateway max_flows {configured} exceeds supported limit {limit}")]
    MaxFlowsExceeded {
        /// Configured maximum concurrent flow count.
        configured: usize,
        /// Highest supported concurrent flow count.
        limit: usize,
    },
    /// The configured flow idle timeout is zero.
    #[error("gateway flow_idle_timeout must be greater than zero")]
    ZeroFlowIdleTimeout,
    /// The configured per-flow TCP buffer size is zero.
    #[error("gateway tcp_buffer_bytes must be greater than zero")]
    ZeroTcpBuffer,
    /// The configured per-flow TCP buffer exceeds the implementation bound.
    #[error("gateway tcp_buffer_bytes {configured} exceeds supported limit {limit}")]
    TcpBufferExceeded {
        /// Configured bytes for each TCP receive or transmit buffer.
        configured: usize,
        /// Highest supported per-buffer byte count.
        limit: usize,
    },
    /// The declared flow count and buffers exceed the aggregate allocation budget.
    #[error(
        "gateway max_flows {max_flows} with tcp_buffer_bytes {tcp_buffer_bytes} exceeds the {limit_bytes}-byte flow-buffer budget"
    )]
    FlowBufferBudgetExceeded {
        /// Configured maximum concurrent flow count.
        max_flows: usize,
        /// Configured bytes for each TCP receive or transmit buffer.
        tcp_buffer_bytes: usize,
        /// Highest supported aggregate flow-buffer allocation.
        limit_bytes: usize,
    },
    /// The configured interface MTU is outside the IPv4 packet range.
    #[error("gateway MTU {0} is outside the supported IPv4 range 576..=65535")]
    InvalidMtu(u32),
    /// A host-wide default capture route was requested.
    #[error("gateway capture route {0} is a default route; select explicit destinations instead")]
    DefaultRouteUnsupported(ipnet::IpNet),
    /// An IPv6 route was supplied to the IPv4/TCP milestone.
    #[error("gateway IPv4/TCP milestone does not accept IPv6 route {0}")]
    Ipv6RouteUnsupported(ipnet::IpNet),
    /// The packet stack has no IPv4 interface address to bind.
    #[error("gateway plan requires at least one IPv4 interface address")]
    MissingInterfaceAddress,
    /// A virtual-interface address is not valid unicast IPv4.
    #[error("gateway interface address {0} is not unicast IPv4")]
    InvalidInterfaceAddress(ipnet::IpNet),
    /// A virtual-interface address could create an implicit connected capture prefix.
    #[error("gateway interface address {0} must use /32 to avoid an implicit connected route")]
    InterfacePrefixUnsupported(ipnet::IpNet),
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
    /// A caller attempted to construct a table that can never admit a flow.
    #[error("gateway flow-table capacity must be greater than zero")]
    ZeroCapacity,
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
    /// A caller requested a table larger than the implementation resource bound.
    #[error("gateway flow-table capacity {requested} exceeds supported limit {limit}")]
    CapacityLimitExceeded {
        /// Requested table capacity.
        requested: usize,
        /// Highest supported table capacity.
        limit: usize,
    },
    /// The bounded flow table could not reserve its declared capacity.
    #[error("gateway flow table could not reserve capacity {capacity}")]
    AllocationFailed {
        /// Requested table capacity.
        capacity: usize,
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

/// Failure while adapting captured packets to the shared userspace TCP stack.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TcpStackError {
    /// A packet referred to a flow that is not tracked by the TCP stack.
    #[error("gateway TCP flow {0:?} is not tracked by the TCP stack")]
    UnknownFlow(FlowId),
    /// The userspace interface could not install its private AnyIP reply route.
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

impl GatewayError {
    /// Construct a platform-boundary failure without erasing its stable operation label.
    pub(crate) fn platform(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Platform {
            operation,
            message: error.to_string(),
        }
    }
}
