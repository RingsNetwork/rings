use std::cell::RefCell;

use super::*;

#[test]
fn route_record_round_trip_preserves_cleanup_identity() {
    let route = Route::new("203.0.113.7".parse().expect("test route"), 32)
        .with_gateway("192.0.2.1".parse().expect("test gateway"));
    assert_eq!(RouteRecord::from_route(&route).into_route(), route);
}

#[test]
fn route_install_persists_cleanup_intent_before_os_mutation() {
    let route = Route::new("203.0.113.7".parse().expect("test route"), 32)
        .with_gateway("192.0.2.1".parse().expect("test gateway"));
    let operations = RefCell::new(Vec::new());
    let mut installed = Vec::new();

    journal_then_add_route(
        route.clone(),
        &mut installed,
        |routes| {
            assert_eq!(routes, std::slice::from_ref(&route));
            operations.borrow_mut().push("journal");
            Ok(())
        },
        |candidate| {
            assert_eq!(candidate, &route);
            operations.borrow_mut().push("add");
            Ok(())
        },
    )
    .expect("write-ahead route transaction");

    assert_eq!(installed, vec![route]);
    assert_eq!(operations.into_inner(), vec!["journal", "add"]);
}

#[test]
fn bypass_inherits_the_most_specific_baseline_route() {
    let default = Route::new("0.0.0.0".parse().expect("default"), 0)
        .with_gateway("192.0.2.1".parse().expect("gateway"));
    let specific = Route::new("203.0.113.0".parse().expect("specific"), 24)
        .with_gateway("198.51.100.1".parse().expect("gateway"));
    let inherited = inherit_baseline_route(
        &[default, specific],
        "203.0.113.7/32".parse().expect("host route"),
    )
    .expect("baseline route");
    assert_eq!(
        inherited.gateway(),
        Some("198.51.100.1".parse().expect("gateway"))
    );
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn bypass_inherits_the_lowest_metric_route_at_the_same_prefix() {
    let expensive = Route::new("0.0.0.0".parse().expect("default"), 0)
        .with_gateway("192.0.2.1".parse().expect("expensive gateway"))
        .with_metric(50);
    let preferred = Route::new("0.0.0.0".parse().expect("default"), 0)
        .with_gateway("198.51.100.1".parse().expect("preferred gateway"))
        .with_metric(5);
    let inherited = inherit_baseline_route(
        &[expensive, preferred],
        "203.0.113.7/32".parse().expect("host route"),
    )
    .expect("baseline route");

    assert_eq!(
        inherited.gateway(),
        Some("198.51.100.1".parse().expect("preferred gateway"))
    );
    assert_eq!(inherited.metric(), Some(5));
}

#[test]
fn exact_capture_route_cannot_also_be_an_underlay_bypass() {
    let target = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
    let mut plan = GatewayPlan {
        routing_mode: crate::RoutingMode::Split,
        addresses: vec!["100.64.0.1/30".parse().expect("address")],
        included_routes: vec!["203.0.113.7/32".parse().expect("capture")],
        excluded_routes: Vec::new(),
        mtu: crate::Mtu::try_from(1_280).expect("MTU"),
        dns_policy: crate::DnsPolicy::Bypass,
        dns_servers: vec!["1.1.1.1".parse().expect("DNS")],
    };
    assert!(validate_underlay_capture_conflicts(&plan, &[target]).is_err());

    plan.included_routes = vec!["203.0.113.0/24".parse().expect("capture")];
    assert!(validate_underlay_capture_conflicts(&plan, &[target]).is_ok());

    plan.dns_policy = crate::DnsPolicy::Block;
    plan.dns_servers = vec![target];
    assert!(validate_underlay_capture_conflicts(&plan, &[target]).is_err());
}

#[test]
fn durable_ledger_round_trip_preserves_cleanup_routes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("routes.json");
    let routes = vec![Route::new("203.0.113.7".parse().expect("route"), 32)
        .with_gateway("192.0.2.1".parse().expect("gateway"))];

    write_lease(&path, &routes).expect("write ledger");
    assert_eq!(read_lease(&path).expect("read ledger"), Some(routes));
    remove_ledger(&path).expect("remove ledger");
    assert_eq!(read_lease(&path).expect("read removed ledger"), None);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn missing_interface_and_missing_route_errors_satisfy_cleanup() {
    for raw_error in [nix::libc::ENXIO, nix::libc::ENODEV, nix::libc::ESRCH] {
        assert!(route_is_absent(&std::io::Error::from_raw_os_error(
            raw_error
        )));
    }
    assert!(!route_is_absent(&std::io::Error::from(
        std::io::ErrorKind::PermissionDenied
    )));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_ipv6_capture_uses_a_stable_ula_without_scoping_the_global_route() {
    let first = macos_capture_anchor(Ipv4Addr::new(100, 64, 0, 1));
    let second = macos_capture_anchor(Ipv4Addr::new(100, 64, 0, 2));
    assert!(first.is_unique_local());
    assert_ne!(first, second);

    let route = capture_route("::/1".parse().expect("IPv6 capture half"), 42);
    assert!(!route.if_scope());
}

#[cfg(target_os = "windows")]
#[test]
fn interrupted_windows_commit_recovers_backup_ledger() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("routes.json");
    let routes = vec![Route::new("203.0.113.7".parse().expect("route"), 32)
        .with_gateway("192.0.2.1".parse().expect("gateway"))];
    write_lease(&path, &routes).expect("write ledger");
    std::fs::rename(&path, ledger_backup_path(&path)).expect("simulate interrupted replace");

    assert_eq!(read_lease(&path).expect("read backup"), Some(routes));
    remove_ledger(&path).expect("remove backup");
}
