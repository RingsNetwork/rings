use std::net::SocketAddr;

use rings_core::ecc::SecretKey;
use rings_dns_relay::ProxyProtocol;

use super::*;
use crate::error::Error;

#[test]
fn test_did_owner_labels_round_trip_and_parse_canonically() {
    for _ in 0..64 {
        let did = SecretKey::random().address().into();
        let owner = DidOwner(did);
        let label = owner.label();
        assert_eq!(label.len(), 42, "{label}");
        assert_eq!(DidOwner::from_label(&label), Some(owner));
        let bare = label.trim_start_matches("0x");
        assert_eq!(
            DidOwner::from_label(bare),
            None,
            "the 0x prefix is required"
        );
    }
    let short = format!("0x{}", "1".repeat(39));
    for label in ["", "0x", "www", short.as_str()] {
        assert_eq!(DidOwner::from_label(label), None, "{label:?}");
    }
}

#[test]
fn test_relay_service_names_each_port_apart() {
    assert_eq!(relay_service(25), "dns-relay/25");
    assert_ne!(relay_service(25), relay_service(250));
}

/// A `services` section serving `ports`, publishing `txt` at `name`.
fn services(ports: &[u16], txt: &[(&str, &str)]) -> ServicesConfig {
    ServicesConfig {
        ports: ports
            .iter()
            .map(|port| ServedPortConfig {
                port: *port,
                target: SocketAddr::from(([127, 0, 0, 1], 2525)),
                proxy_protocol: ProxyProtocol::Off,
            })
            .collect(),
        txt: txt
            .iter()
            .map(|(name, value)| ServedTxtConfig {
                name: (*name).to_owned(),
                values: vec![(*value).to_owned()],
            })
            .collect(),
        heartbeat_interval_secs: 30,
        ttl_secs: 90,
    }
}

#[test]
fn test_services_section_validates_into_an_offer() {
    let offer = services(&[25, 22], &[("_acme-challenge", "token")])
        .offer()
        .unwrap();
    let ports = offer.served_ports().map(|served| served.port);
    assert_eq!(ports.collect::<Vec<_>>(), vec![22, 25]);
    assert!(
        services(&[], &[]).offer().is_ok(),
        "an empty section withdraws"
    );
}

#[test]
fn test_services_section_refuses_what_a_relay_would_reject() {
    let cases = [
        services(&[25, 25], &[]),
        services(&[0], &[]),
        services(&[], &[("Bad Name", "v")]),
        ServicesConfig {
            heartbeat_interval_secs: 90,
            ..services(&[25], &[])
        },
    ];
    for config in cases {
        assert!(
            matches!(config.offer(), Err(Error::InvalidConfig(_))),
            "{config:?}"
        );
    }
}

#[test]
fn test_a_bad_schedule_names_the_settings_that_fix_it() {
    let config = ServicesConfig {
        heartbeat_interval_secs: 90,
        ..services(&[25], &[])
    };
    let Err(Error::InvalidConfig(message)) = config.offer() else {
        panic!("a heartbeat as long as the TTL is refused");
    };
    assert!(
        message.contains("services.heartbeat_interval_secs")
            && message.contains("services.ttl_secs"),
        "{message}"
    );
}
