#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

#[cfg(target_family = "wasm")]
compile_error!("rings-gateway is a native-only crate; WASM nodes are clients only");

/// Operating-system effects used by the native gateway.
pub mod bindings;
mod bridge;
mod config;
mod error;
mod flow;
mod flow_table;
mod packet;
mod runtime;
mod server;
mod status;
mod stream;
mod tcp;

pub use config::DnsPolicy;
pub use config::GatewayConfig;
pub use config::GatewayPlan;
pub use config::Mtu;
pub use config::RoutingMode;
pub use error::ConfigError;
pub use error::FlowTableError;
pub use error::FlowTransitionError;
pub use error::GatewayError;
pub use error::GatewayTransitionError;
pub use error::PacketIoError;
pub use error::PacketParseError;
pub use error::TcpStackError;
pub use flow::FlowEvent;
pub use flow::FlowId;
pub use flow::FlowState;
pub use flow_table::FlowTable;
pub use packet::classify_ipv4_packet;
pub use packet::PacketDisposition;
pub use packet::PacketIo;
pub use packet::TcpSegment;
pub use runtime::GatewayControlHandle;
pub use runtime::GatewayRuntime;
pub use server::GatewayEvent;
pub use server::GatewayServer;
pub use server::GatewayState;
pub use status::ExitAvailability;
pub use status::GatewayHealth;
pub use status::GatewayStatus;
pub use status::GatewayStatusHandle;
pub use stream::BoxGatewayDuplex;
pub use stream::GatewayDuplex;
pub use stream::OnionStreamConnector;
pub use tcp::TcpEndpointState;
pub use tcp::TcpStack;
