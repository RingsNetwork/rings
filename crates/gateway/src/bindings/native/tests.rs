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
fn route_ownership_conflicts_only_on_the_exact_destination_prefix() {
    let existing = Route::new("203.0.113.0".parse().expect("existing route"), 24);
    let same = Route::new("203.0.113.0".parse().expect("same route"), 24);
    let more_specific = Route::new("203.0.113.7".parse().expect("host route"), 32);
    let adjacent = Route::new("203.0.114.0".parse().expect("adjacent route"), 24);

    assert!(same_route_destination(&existing, &same));
    assert!(!same_route_destination(&existing, &more_specific));
    assert!(!same_route_destination(&existing, &adjacent));
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
