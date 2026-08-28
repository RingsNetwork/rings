//! Validated gateway configuration and declarative tunnel plan.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::Duration;

use ipnet::IpNet;
use serde::Deserialize;
use serde::Serialize;

use crate::ConfigError;

const MIN_IPV4_MTU: u32 = 576;
const MAX_IPV4_PACKET: u32 = 65_535;

/// Host-routing behavior owned by the gateway.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingMode {
    /// The gateway owns no capture route.
    Disabled,
    /// Only explicitly included routes are captured.
    Split,
    /// The IPv4 default route is captured, subject to exclusions.
    Default,
}

/// DNS behavior for the IPv4/TCP milestone.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DnsPolicy {
    /// Route the declared host resolvers outside capture and report that DNS bypasses Onion.
    Bypass,
    /// Route the declared host resolvers into capture and reject ordinary DNS there.
    Block,
}

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
pub struct GatewayPlan {
    /// Routing mode.
    pub routing_mode: RoutingMode,
    /// IPv4 interface addresses assigned to the virtual interface.
    pub addresses: Vec<IpNet>,
    /// Destination networks captured by the gateway.
    pub included_routes: Vec<IpNet>,
    /// More-specific destinations that must bypass capture.
    pub excluded_routes: Vec<IpNet>,
    /// Virtual-interface MTU.
    pub mtu: Mtu,
    /// Explicit DNS behavior.
    pub dns_policy: DnsPolicy,
    /// Explicit IPv4 host resolvers to bypass or capture according to `dns_policy`.
    ///
    /// Rings deliberately does not discover or mutate system DNS configuration. Operators must
    /// list every resolver that applications can use so the route policy is deterministic.
    pub dns_servers: Vec<IpAddr>,
}

impl GatewayPlan {
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
        self.validate_dns()?;
        if self.routing_mode != RoutingMode::Disabled && self.included_routes.is_empty() {
            return Err(ConfigError::MissingIncludedRoute(self.routing_mode));
        }
        let mut captured = self
            .included_routes
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut bypassed = self
            .excluded_routes
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let dns_routes = self.dns_servers.iter().filter_map(ipv4_host_route);
        match self.dns_policy {
            DnsPolicy::Block => captured.extend(dns_routes),
            DnsPolicy::Bypass => bypassed.extend(dns_routes),
        }
        if let Some(conflict) = captured.intersection(&bypassed).next() {
            return Err(ConfigError::ConflictingRoute(*conflict));
        }
        Ok(())
    }

    fn validate_ipv4_only(&self) -> Result<(), ConfigError> {
        self.addresses
            .iter()
            .chain(&self.included_routes)
            .chain(&self.excluded_routes)
            .find(|network| matches!(network, IpNet::V6(_)))
            .copied()
            .map_or(Ok(()), |network| {
                Err(ConfigError::Ipv6RouteUnsupported(network))
            })
    }

    fn validate_dns(&self) -> Result<(), ConfigError> {
        if self.dns_servers.is_empty() {
            return Err(ConfigError::MissingDnsServer(self.dns_policy));
        }
        if let Some(address) = self
            .dns_servers
            .iter()
            .find(|address| !matches!(address, IpAddr::V4(_)))
            .copied()
        {
            return Err(ConfigError::Ipv6DnsUnsupported(address));
        }
        Ok(())
    }
}

fn ipv4_host_route(address: &IpAddr) -> Option<IpNet> {
    let IpAddr::V4(address) = address else {
        return None;
    };
    IpNet::new((*address).into(), 32).ok()
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
        if self.flow_idle_timeout.is_zero() {
            return Err(ConfigError::ZeroFlowIdleTimeout);
        }
        if self.tcp_buffer_bytes == 0 {
            return Err(ConfigError::ZeroTcpBuffer);
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
            routing_mode: RoutingMode::Default,
            addresses: vec![route(Ipv4Addr::new(100, 64, 0, 1), 30)],
            included_routes: vec![route(Ipv4Addr::UNSPECIFIED, 0)],
            excluded_routes: vec![route(Ipv4Addr::LOCALHOST, 8)],
            mtu: Mtu::try_from(1_280).expect("test MTU must be valid"),
            dns_policy: DnsPolicy::Block,
            dns_servers: vec!["1.1.1.1".parse().expect("test DNS")],
        }
    }

    #[test]
    fn default_route_plan_requires_an_included_route() {
        let mut candidate = plan();
        candidate.included_routes.clear();
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::MissingIncludedRoute(RoutingMode::Default))
        );
    }

    #[test]
    fn one_route_cannot_be_both_captured_and_excluded() {
        let mut candidate = plan();
        candidate.excluded_routes = candidate.included_routes.clone();
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::ConflictingRoute(route(
                Ipv4Addr::UNSPECIFIED,
                0
            )))
        );
    }

    #[test]
    fn dns_policy_routes_cannot_conflict_with_explicit_routes() {
        let dns_host = route(Ipv4Addr::new(1, 1, 1, 1), 32);
        let mut blocked = plan();
        blocked.excluded_routes.push(dns_host);
        assert_eq!(
            blocked.validate(),
            Err(ConfigError::ConflictingRoute(dns_host))
        );

        let mut bypassed = plan();
        bypassed.dns_policy = DnsPolicy::Bypass;
        bypassed.included_routes = vec![dns_host];
        assert_eq!(
            bypassed.validate(),
            Err(ConfigError::ConflictingRoute(dns_host))
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
    fn every_dns_policy_requires_explicit_ipv4_resolvers() {
        let mut candidate = plan();
        candidate.dns_servers.clear();
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::MissingDnsServer(DnsPolicy::Block))
        );

        let mut candidate = plan();
        candidate.dns_policy = DnsPolicy::Bypass;
        candidate.dns_servers.clear();
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::MissingDnsServer(DnsPolicy::Bypass))
        );
        candidate
            .dns_servers
            .push("2001:4860:4860::8888".parse().expect("test DNS"));
        assert!(matches!(
            candidate.validate(),
            Err(ConfigError::Ipv6DnsUnsupported(_))
        ));
    }

    #[test]
    fn runtime_limits_default_when_omitted_from_serialized_config() {
        let json = r#"{
            "plan": {
                "routing_mode": "default",
                "addresses": ["100.64.0.1/30"],
                "included_routes": ["0.0.0.0/0"],
                "excluded_routes": ["127.0.0.0/8"],
                "mtu": 1280,
                "dns_policy": "block",
                "dns_servers": ["1.1.1.1"]
            }
        }"#;
        let config: GatewayConfig = serde_json::from_str(json).expect("valid gateway JSON");
        assert_eq!(config.max_flows, default_max_flows());
        assert_eq!(config.flow_idle_timeout, default_flow_idle_timeout());
        assert_eq!(config.tcp_buffer_bytes, default_tcp_buffer_bytes());
    }
}
