//! Validated gateway configuration and declarative tunnel plan.

use std::net::Ipv4Addr;
use std::time::Duration;

use ipnet::IpNet;
use serde::Deserialize;
use serde::Serialize;

use crate::ConfigError;

const MIN_IPV4_MTU: u32 = 576;
const MAX_IPV4_PACKET: u32 = 65_535;
pub(crate) const MAX_GATEWAY_FLOWS: usize = 16_384;
const MAX_TCP_BUFFER_BYTES: usize = 1_024 * 1_024;
// smoltcp receive/transmit buffers plus the two directions of Tokio's duplex bridge.
const FLOW_BUFFER_ALLOCATION_FACTOR: usize = 4;
const MAX_TOTAL_FLOW_BUFFER_BYTES: usize = 1_024 * 1_024 * 1_024;

/// A validated interface MTU.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Mtu(u16);

impl Mtu {
    /// Return the validated MTU as a host integer.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl TryFrom<u32> for Mtu {
    type Error = ConfigError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if !(MIN_IPV4_MTU..=MAX_IPV4_PACKET).contains(&value) {
            return Err(ConfigError::InvalidMtu(value));
        }
        u16::try_from(value)
            .map(Self)
            .map_err(|_| ConfigError::InvalidMtu(value))
    }
}

impl From<Mtu> for u32 {
    fn from(value: Mtu) -> Self {
        u32::from(value.0)
    }
}

/// Declarative network state a platform binding must establish before admitting packets.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayPlan {
    /// IPv4 `/32` host addresses assigned to the virtual interface.
    pub addresses: Vec<IpNet>,
    /// Exact destination networks captured by the gateway.
    ///
    /// An empty set is valid when an external router owns packet selection. Platform bindings may
    /// normalize and deduplicate these routes, but must not add implicit capture routes.
    #[serde(default)]
    pub included_routes: Vec<IpNet>,
    /// Virtual-interface MTU.
    pub mtu: Mtu,
}

impl GatewayPlan {
    /// Return the primary IPv4 interface address and prefix.
    pub fn first_ipv4_address(&self) -> Result<(Ipv4Addr, u8), ConfigError> {
        self.addresses
            .iter()
            .find_map(|network| match network {
                IpNet::V4(network) => Some((network.addr(), network.prefix_len())),
                IpNet::V6(_) => None,
            })
            .ok_or(ConfigError::MissingInterfaceAddress)
    }

    /// Validate the platform-neutral routing plan.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_ipv4_only()?;
        if self.addresses.is_empty() {
            return Err(ConfigError::MissingInterfaceAddress);
        }
        if let Some(invalid) = self.addresses.iter().copied().find(|network| {
            let IpNet::V4(network) = network else {
                return true;
            };
            let address = network.addr();
            address.is_unspecified()
                || address.is_multicast()
                || address == std::net::Ipv4Addr::BROADCAST
        }) {
            return Err(ConfigError::InvalidInterfaceAddress(invalid));
        }
        if let Some(prefixed) = self
            .addresses
            .iter()
            .copied()
            .find(|network| network.prefix_len() != 32)
        {
            return Err(ConfigError::InterfacePrefixUnsupported(prefixed));
        }
        if let Some(default_route) = self
            .included_routes
            .iter()
            .copied()
            .find(|network| network.prefix_len() == 0)
        {
            return Err(ConfigError::DefaultRouteUnsupported(default_route));
        }
        Ok(())
    }

    fn validate_ipv4_only(&self) -> Result<(), ConfigError> {
        self.addresses
            .iter()
            .chain(&self.included_routes)
            .find(|network| matches!(network, IpNet::V6(_)))
            .copied()
            .map_or(Ok(()), |network| {
                Err(ConfigError::Ipv6RouteUnsupported(network))
            })
    }
}

/// Validated limits and network plan for one gateway runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GatewayConfig {
    /// Network state established before packet admission.
    pub plan: GatewayPlan,
    /// Maximum number of concurrently live TCP flows.
    #[serde(default = "default_max_flows")]
    pub max_flows: usize,
    /// Idle timeout applied independently to each admitted flow.
    #[serde(default = "default_flow_idle_timeout", with = "duration_seconds")]
    pub flow_idle_timeout: Duration,
    /// Receive and transmit buffer size allocated for each TCP flow.
    #[serde(default = "default_tcp_buffer_bytes")]
    pub tcp_buffer_bytes: usize,
}

const fn default_max_flows() -> usize {
    1_024
}

const fn default_flow_idle_timeout() -> Duration {
    Duration::from_secs(300)
}

const fn default_tcp_buffer_bytes() -> usize {
    64 * 1_024
}

mod duration_seconds {
    use std::time::Duration;

    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where D: Deserializer<'de> {
        u64::deserialize(deserializer).map(Duration::from_secs)
    }
}

impl GatewayConfig {
    /// Validate runtime limits and the underlying tunnel plan.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_flows == 0 {
            return Err(ConfigError::ZeroMaxFlows);
        }
        if self.max_flows > MAX_GATEWAY_FLOWS {
            return Err(ConfigError::MaxFlowsExceeded {
                configured: self.max_flows,
                limit: MAX_GATEWAY_FLOWS,
            });
        }
        if self.flow_idle_timeout.is_zero() {
            return Err(ConfigError::ZeroFlowIdleTimeout);
        }
        if self.tcp_buffer_bytes == 0 {
            return Err(ConfigError::ZeroTcpBuffer);
        }
        if self.tcp_buffer_bytes > MAX_TCP_BUFFER_BYTES {
            return Err(ConfigError::TcpBufferExceeded {
                configured: self.tcp_buffer_bytes,
                limit: MAX_TCP_BUFFER_BYTES,
            });
        }
        let flow_buffer_bytes = self
            .max_flows
            .checked_mul(self.tcp_buffer_bytes)
            .and_then(|bytes| bytes.checked_mul(FLOW_BUFFER_ALLOCATION_FACTOR))
            .ok_or(ConfigError::FlowBufferBudgetExceeded {
                max_flows: self.max_flows,
                tcp_buffer_bytes: self.tcp_buffer_bytes,
                limit_bytes: MAX_TOTAL_FLOW_BUFFER_BYTES,
            })?;
        if flow_buffer_bytes > MAX_TOTAL_FLOW_BUFFER_BYTES {
            return Err(ConfigError::FlowBufferBudgetExceeded {
                max_flows: self.max_flows,
                tcp_buffer_bytes: self.tcp_buffer_bytes,
                limit_bytes: MAX_TOTAL_FLOW_BUFFER_BYTES,
            });
        }
        self.plan.validate()
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn route(address: Ipv4Addr, prefix: u8) -> IpNet {
        IpNet::new(address.into(), prefix).expect("test route must be valid")
    }

    fn plan() -> GatewayPlan {
        GatewayPlan {
            addresses: vec![route(Ipv4Addr::new(100, 64, 0, 1), 32)],
            included_routes: vec![route(Ipv4Addr::new(198, 18, 0, 0), 15)],
            mtu: Mtu::try_from(1_280).expect("test MTU must be valid"),
        }
    }

    #[test]
    fn empty_capture_set_is_valid_for_external_traffic_selection() {
        let mut candidate = plan();
        candidate.included_routes.clear();
        assert_eq!(candidate.validate(), Ok(()));
    }

    #[test]
    fn host_wide_default_capture_is_rejected() {
        let mut candidate = plan();
        let default_route = route(Ipv4Addr::UNSPECIFIED, 0);
        candidate.included_routes = vec![default_route];
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::DefaultRouteUnsupported(default_route))
        );
    }

    #[test]
    fn ipv6_is_rejected_until_the_gateway_has_an_ipv6_policy() {
        let mut candidate = plan();
        let ipv6 = IpNet::new("2001:db8::".parse().expect("test address"), 32).expect("test route");
        candidate.included_routes.push(ipv6);
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::Ipv6RouteUnsupported(ipv6))
        );
    }

    #[test]
    fn interface_requires_one_unicast_ipv4_address() {
        let mut candidate = plan();
        candidate.addresses.clear();
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::MissingInterfaceAddress)
        );

        candidate.addresses = vec![route(Ipv4Addr::UNSPECIFIED, 32)];
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::InvalidInterfaceAddress(route(
                Ipv4Addr::UNSPECIFIED,
                32
            )))
        );
    }

    #[test]
    fn interface_address_cannot_create_an_implicit_connected_prefix() {
        let mut candidate = plan();
        let prefixed = route(Ipv4Addr::new(100, 64, 0, 1), 30);
        candidate.addresses = vec![prefixed];
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::InterfacePrefixUnsupported(prefixed))
        );
    }

    #[test]
    fn primary_ipv4_address_is_plan_vocabulary() {
        let candidate = plan();
        assert_eq!(
            candidate.first_ipv4_address(),
            Ok((Ipv4Addr::new(100, 64, 0, 1), 32))
        );
    }

    #[test]
    fn runtime_limits_default_when_omitted_from_serialized_config() {
        let json = r#"{
            "plan": {
                "addresses": ["100.64.0.1/32"],
                "included_routes": ["198.18.0.0/15"],
                "mtu": 1280
            }
        }"#;
        let config: GatewayConfig = serde_json::from_str(json).expect("valid gateway JSON");
        assert_eq!(config.max_flows, default_max_flows());
        assert_eq!(config.flow_idle_timeout, default_flow_idle_timeout());
        assert_eq!(config.tcp_buffer_bytes, default_tcp_buffer_bytes());
    }

    #[test]
    fn removed_route_authority_fields_are_not_silently_accepted() {
        let legacy = r#"{
            "routing_mode": "default",
            "addresses": ["100.64.0.1/32"],
            "included_routes": ["198.18.0.0/15"],
            "excluded_routes": [],
            "mtu": 1280,
            "dns_policy": "bypass",
            "dns_servers": ["1.1.1.1"]
        }"#;

        let error = serde_json::from_str::<GatewayPlan>(legacy)
            .expect_err("removed full-capture fields must fail closed")
            .to_string();
        assert!(error.contains("unknown field"));
    }

    #[test]
    fn runtime_limits_reject_pathological_allocations() {
        let mut candidate = GatewayConfig {
            plan: plan(),
            max_flows: default_max_flows(),
            flow_idle_timeout: default_flow_idle_timeout(),
            tcp_buffer_bytes: default_tcp_buffer_bytes(),
        };
        candidate.max_flows = MAX_GATEWAY_FLOWS + 1;
        assert!(matches!(
            candidate.validate(),
            Err(ConfigError::MaxFlowsExceeded { .. })
        ));

        candidate.max_flows = default_max_flows();
        candidate.tcp_buffer_bytes = MAX_TCP_BUFFER_BYTES + 1;
        assert!(matches!(
            candidate.validate(),
            Err(ConfigError::TcpBufferExceeded { .. })
        ));

        candidate.max_flows = MAX_GATEWAY_FLOWS;
        candidate.tcp_buffer_bytes = default_tcp_buffer_bytes();
        assert!(matches!(
            candidate.validate(),
            Err(ConfigError::FlowBufferBudgetExceeded { .. })
        ));
    }
}
