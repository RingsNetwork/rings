use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;

use crate::auth::TurnAdmissionPolicy;
use crate::error::Result;
use crate::error::TurnError;

/// Default TURN UDP listening port (RFC 5766).
pub const DEFAULT_TURN_PORT: u16 = 3478;

/// Default TURN authentication realm.
pub const DEFAULT_TURN_REALM: &str = "rings";

/// Default maximum retries when allocating a random relay port in a configured range.
pub const DEFAULT_PORT_RANGE_MAX_RETRIES: u16 = 32;

/// Relay address allocation strategy for TURN allocations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayAddressConfig {
    /// Bind ephemeral relay sockets on `bind_ip` and advertise the bound local address directly.
    None {
        /// Local IP address passed to `bind` when creating relay sockets.
        bind_ip: IpAddr,
    },
    /// Bind ephemeral relay sockets on `bind_ip` and advertise `relay_ip` in `XOR-RELAYED-ADDRESS`.
    Static {
        /// Public/external IP returned to TURN clients when an allocation is created.
        relay_ip: IpAddr,
        /// Local IP address passed to `bind` when creating relay sockets.
        bind_ip: IpAddr,
    },
    /// Bind relay sockets on `bind_ip` within `[min_port, max_port]` and advertise `relay_ip`.
    Range {
        /// Public/external IP returned to TURN clients when an allocation is created.
        relay_ip: IpAddr,
        /// Local IP address passed to `bind` when creating relay sockets.
        bind_ip: IpAddr,
        /// Inclusive lower relay UDP port bound.
        min_port: u16,
        /// Inclusive upper relay UDP port bound.
        max_port: u16,
        /// Maximum attempts to pick a free random port in `[min_port, max_port]`.
        max_retries: u16,
    },
}

impl Default for RelayAddressConfig {
    fn default() -> Self {
        Self::Static {
            relay_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }
}

impl RelayAddressConfig {
    /// Validate port-range invariants before starting the TURN server.
    pub fn validate(&self) -> Result<()> {
        if let Self::Range {
            min_port, max_port, ..
        } = *self
        {
            if min_port == 0 || max_port == 0 || min_port > max_port {
                return Err(TurnError::InvalidPortRange { min_port, max_port });
            }
        }
        Ok(())
    }
}

/// Configuration for [`crate::TurnServer`].
#[derive(Clone)]
pub struct TurnServerConfig {
    /// UDP socket address the TURN server listens on.
    pub listen_addr: SocketAddr,
    /// Realm advertised in TURN 401/438 challenges.
    pub realm: String,
    /// Relay socket allocation strategy.
    pub relay: RelayAddressConfig,
    /// ChannelBind lifetime (`Duration::ZERO` uses the RFC 5766 default of 10 minutes).
    pub channel_bind_timeout: Duration,
    /// Admission policy for verified delegator DIDs.
    pub admission: TurnAdmissionPolicy,
    /// Injectable clock used to validate [`Delegation`](rings_core::delegation::Delegation) windows.
    pub clock: SharedClock,
}

impl std::fmt::Debug for TurnServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServerConfig")
            .field("listen_addr", &self.listen_addr)
            .field("realm", &self.realm)
            .field("relay", &self.relay)
            .field("channel_bind_timeout", &self.channel_bind_timeout)
            .field("admission", &self.admission)
            .finish()
    }
}

impl Default for TurnServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_TURN_PORT),
            realm: DEFAULT_TURN_REALM.to_string(),
            relay: RelayAddressConfig::default(),
            channel_bind_timeout: Duration::ZERO,
            admission: TurnAdmissionPolicy::default(),
            clock: Arc::new(SystemClock),
        }
    }
}

impl TurnServerConfig {
    /// Create a loopback TURN server configuration bound to an ephemeral port (`127.0.0.1:0`),
    /// suitable for local testing and in-process integration tests.
    pub fn loopback_ephemeral() -> Self {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        Self {
            listen_addr: SocketAddr::new(localhost, 0),
            realm: DEFAULT_TURN_REALM.to_string(),
            relay: RelayAddressConfig::Static {
                relay_ip: localhost,
                bind_ip: localhost,
            },
            channel_bind_timeout: Duration::ZERO,
            admission: TurnAdmissionPolicy::AllowAnyDelegation,
            clock: Arc::new(SystemClock),
        }
    }
}
