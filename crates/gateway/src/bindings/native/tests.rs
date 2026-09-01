use std::cell::RefCell;

use super::*;

#[tokio::test]
async fn failed_teardown_returns_the_same_linear_lease() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let options = NativeTunnelOptions::new(directory.path().join("routes.json"));
    let mut control = NativeTunnelControl::new(options).expect("native control");
    let failure = control
        .teardown(NativeTunnelLease { id: 41 })
        .await
        .expect_err("inactive control cannot clean up a lease");

    let (lease, error) = failure.into_parts();
    assert_eq!(lease.id, 41);
    assert!(error
        .to_string()
        .contains("no native tunnel lease is active"));
}

#[test]
fn route_ledger_lock_is_exclusive_for_the_control_lifetime() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let options = NativeTunnelOptions::new(directory.path().join("routes.json"));
    let first = NativeTunnelControl::new(options.clone()).expect("first ledger owner");

    let error = match NativeTunnelControl::new(options.clone()) {
        Ok(_) => panic!("second control unexpectedly acquired the same ledger lock"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        GatewayError::RouteLedgerLocked { path }
            if path == route_ledger_lock_path(&options.route_ledger_path)
    ));

    drop(first);
    NativeTunnelControl::new(options).expect("released ledger lock can be acquired again");
}

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
fn failed_route_add_removes_the_unowned_cleanup_intent() {
    let route = Route::new("203.0.113.7".parse().expect("test route"), 32);
    let journaled = RefCell::new(Vec::new());
    let mut installed = Vec::new();

    let error = journal_then_add_route(
        route.clone(),
        &mut installed,
        |routes| {
            journaled.borrow_mut().push(routes.to_vec());
            Ok(())
        },
        |_| Err(GatewayError::platform("add-route", "route already exists")),
    )
    .expect_err("failed add must not claim route ownership");

    assert!(error.to_string().contains("route already exists"));
    assert!(installed.is_empty());
    assert_eq!(journaled.into_inner(), vec![vec![route], Vec::new()]);
}

#[test]
fn route_shadowing_detection_matches_longest_prefix_routing() {
    let capture = "203.0.113.0/24".parse().expect("capture route");
    let exact = Route::new("203.0.113.0".parse().expect("exact route"), 24);
    let more_specific = Route::new("203.0.113.128".parse().expect("nested route"), 25);
    let host = Route::new("203.0.113.7".parse().expect("host route"), 32);
    let less_specific = Route::new("203.0.0.0".parse().expect("covering route"), 16);
    let adjacent = Route::new("203.0.114.0".parse().expect("adjacent route"), 24);

    assert!(route_shadows_capture(&exact, &capture));
    assert!(route_shadows_capture(&more_specific, &capture));
    assert!(route_shadows_capture(&host, &capture));
    assert!(!route_shadows_capture(&less_specific, &capture));
    assert!(!route_shadows_capture(&adjacent, &capture));
}

#[test]
fn capture_shadowing_returns_a_typed_host_state_error() {
    let capture = "203.0.113.0/24".parse().expect("capture route");
    let existing = Route::new("203.0.113.128".parse().expect("nested route"), 25);

    let error = ensure_capture_destinations_available(&[existing], &[capture])
        .expect_err("more-specific route must block capture establishment");

    assert!(matches!(
        error,
        GatewayError::CaptureRouteShadowed {
            capture: actual_capture,
            existing_destination,
            existing_prefix: 25,
        } if actual_capture == capture
            && existing_destination == "203.0.113.128".parse::<IpAddr>().expect("address")
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn stale_route_ownership_requires_one_exact_interface_match() {
    let destination = "203.0.113.0".parse().expect("route destination");
    let recorded = Route::new(destination, 24)
        .with_if_index(17)
        .with_if_name("utun17".to_string());
    let owned = Route::new(destination, 24)
        .with_if_index(17)
        .with_if_name("utun17".to_string());
    let foreign = Route::new(destination, 24)
        .with_if_index(21)
        .with_if_name("utun21".to_string());

    assert_eq!(
        stale_route_ownership(&recorded, &[]),
        StaleRouteOwnership::Absent
    );
    assert_eq!(
        stale_route_ownership(&recorded, std::slice::from_ref(&owned)),
        StaleRouteOwnership::Owned
    );
    assert_eq!(
        stale_route_ownership(&recorded, std::slice::from_ref(&foreign)),
        StaleRouteOwnership::Conflict {
            existing_interface_index: Some(21),
            existing_interface_name: Some("utun21".to_string()),
        }
    );
    assert!(matches!(
        stale_route_ownership(&recorded, &[owned, foreign]),
        StaleRouteOwnership::Conflict { .. }
    ));
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
