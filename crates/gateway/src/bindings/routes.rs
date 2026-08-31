//! Pure compilation of declarative gateway routes into platform route intents.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;

use ipnet::IpNet;

use crate::DnsPolicy;
use crate::GatewayPlan;
use crate::RoutingMode;

const IPV6_LOWER_HALF: IpNet = IpNet::new_assert(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 1);
const IPV6_UPPER_HALF: IpNet =
    IpNet::new_assert(IpAddr::V6(Ipv6Addr::new(0x8000, 0, 0, 0, 0, 0, 0, 0)), 1);

pub(super) fn capture_routes(plan: &GatewayPlan) -> Vec<IpNet> {
    if plan.routing_mode == RoutingMode::Disabled {
        return Vec::new();
    }
    let mut routes = BTreeSet::new();
    for route in &plan.included_routes {
        if plan.routing_mode == RoutingMode::Default && is_ipv4_default(*route) {
            routes.insert(IpNet::new(Ipv4Addr::UNSPECIFIED.into(), 1).unwrap_or(*route));
            routes.insert(IpNet::new(Ipv4Addr::new(128, 0, 0, 0).into(), 1).unwrap_or(*route));
        } else {
            routes.insert(*route);
        }
    }
    // IPv6 forwarding is deferred, but bypassing the gateway over the host's IPv6 default route
    // would violate the capture policy. Routing both halves to the packet device makes the total
    // IPv6 space fail closed; the IPv4-only classifier drops every captured IPv6 packet.
    routes.extend([IPV6_LOWER_HALF, IPV6_UPPER_HALF]);
    if plan.dns_policy == DnsPolicy::Block {
        routes.extend(plan.dns_servers.iter().filter_map(host_route));
    }
    routes.into_iter().collect()
}

pub(super) fn bypass_routes(plan: &GatewayPlan, underlay: &[IpAddr]) -> Vec<IpNet> {
    if plan.routing_mode == RoutingMode::Disabled {
        return Vec::new();
    }
    let mut routes = plan
        .excluded_routes
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if plan.dns_policy == DnsPolicy::Bypass {
        routes.extend(plan.dns_servers.iter().filter_map(host_route));
    }
    routes.extend(underlay.iter().filter_map(host_route));
    routes.into_iter().collect()
}

fn is_ipv4_default(network: IpNet) -> bool {
    matches!(network, IpNet::V4(network) if network.prefix_len() == 0)
}

fn host_route(address: &IpAddr) -> Option<IpNet> {
    match address {
        IpAddr::V4(address) => IpNet::new((*address).into(), 32).ok(),
        IpAddr::V6(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::GatewayConfig;
    use crate::Mtu;

    fn network(address: Ipv4Addr, prefix: u8) -> IpNet {
        IpNet::new(address.into(), prefix).expect("test network")
    }

    fn plan() -> GatewayPlan {
        GatewayPlan {
            routing_mode: RoutingMode::Default,
            addresses: vec![network(Ipv4Addr::new(100, 64, 0, 1), 30)],
            included_routes: vec![network(Ipv4Addr::UNSPECIFIED, 0)],
            excluded_routes: vec![network(Ipv4Addr::LOCALHOST, 8)],
            mtu: Mtu::try_from(1_280).expect("test MTU"),
            dns_policy: DnsPolicy::Bypass,
            dns_servers: vec!["1.1.1.1".parse().expect("test DNS")],
        }
    }

    #[test]
    fn default_capture_uses_openvpn_style_def1_routes() {
        assert_eq!(capture_routes(&plan()), vec![
            network(Ipv4Addr::UNSPECIFIED, 1),
            network(Ipv4Addr::new(128, 0, 0, 0), 1),
            IPV6_LOWER_HALF,
            IPV6_UPPER_HALF,
        ]);
    }

    #[test]
    fn every_enabled_capture_mode_blocks_the_complete_ipv6_space() {
        for routing_mode in [RoutingMode::Split, RoutingMode::Default] {
            let mut plan = plan();
            plan.routing_mode = routing_mode;
            let routes = capture_routes(&plan);
            assert!(routes.contains(&IPV6_LOWER_HALF));
            assert!(routes.contains(&IPV6_UPPER_HALF));
        }
    }

    #[test]
    fn dns_and_underlay_bypass_are_more_specific_than_capture() {
        let routes = bypass_routes(&plan(), &["203.0.113.7".parse().expect("test peer")]);
        assert!(routes.contains(&network(Ipv4Addr::LOCALHOST, 8)));
        assert!(routes.contains(&network(Ipv4Addr::new(1, 1, 1, 1), 32)));
        assert!(routes.contains(&network(Ipv4Addr::new(203, 0, 113, 7), 32)));
    }

    #[test]
    fn blocked_dns_gets_an_explicit_capture_host_route() {
        let mut plan = plan();
        plan.routing_mode = RoutingMode::Split;
        plan.included_routes = vec![network(Ipv4Addr::new(203, 0, 113, 0), 24)];
        plan.dns_policy = DnsPolicy::Block;

        assert_eq!(capture_routes(&plan), vec![
            network(Ipv4Addr::new(1, 1, 1, 1), 32),
            network(Ipv4Addr::new(203, 0, 113, 0), 24),
            IPV6_LOWER_HALF,
            IPV6_UPPER_HALF,
        ]);
        assert!(!bypass_routes(&plan, &[]).contains(&network(Ipv4Addr::new(1, 1, 1, 1), 32)));
    }

    #[test]
    fn route_plan_remains_valid_gateway_configuration() {
        let config = GatewayConfig {
            plan: plan(),
            max_flows: 1,
            flow_idle_timeout: Duration::from_secs(1),
            tcp_buffer_bytes: 1,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn disabled_routing_owns_neither_capture_nor_bypass_routes() {
        let mut plan = plan();
        plan.routing_mode = RoutingMode::Disabled;

        assert!(capture_routes(&plan).is_empty());
        assert!(bypass_routes(&plan, &["203.0.113.7".parse().expect("test underlay")]).is_empty());
    }
}
