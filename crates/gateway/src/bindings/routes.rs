//! Pure compilation of declarative gateway routes into platform route intents.

use std::collections::BTreeSet;

use ipnet::IpNet;

use crate::GatewayPlan;

pub(crate) fn capture_routes(plan: &GatewayPlan) -> Vec<IpNet> {
    plan.included_routes
        .iter()
        .copied()
        .map(|route| route.trunc())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;
    use crate::GatewayConfig;
    use crate::Mtu;

    fn network(address: Ipv4Addr, prefix: u8) -> IpNet {
        IpNet::new(address.into(), prefix).expect("test network")
    }

    fn plan() -> GatewayPlan {
        GatewayPlan {
            addresses: vec![network(Ipv4Addr::new(100, 64, 0, 1), 32)],
            included_routes: vec![network(Ipv4Addr::new(198, 18, 0, 0), 15)],
            mtu: Mtu::try_from(1_280).expect("test MTU"),
        }
    }

    #[test]
    fn capture_routes_equal_the_normalized_explicit_set() {
        let mut plan = plan();
        plan.included_routes = vec![
            network(Ipv4Addr::new(203, 0, 113, 7), 24),
            network(Ipv4Addr::new(198, 51, 100, 9), 32),
            network(Ipv4Addr::new(203, 0, 113, 0), 24),
        ];

        assert_eq!(capture_routes(&plan), vec![
            network(Ipv4Addr::new(198, 51, 100, 9), 32),
            network(Ipv4Addr::new(203, 0, 113, 0), 24),
        ]);
    }

    #[test]
    fn empty_capture_set_adds_no_platform_route() {
        let mut plan = plan();
        plan.included_routes.clear();
        assert!(capture_routes(&plan).is_empty());
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
}
