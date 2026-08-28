use std::cell::RefCell;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddrV4;
use std::time::Duration;

use super::*;

#[tokio::test]
#[ignore = "requires TUN/route privileges and public TCP reachability"]
async fn privileged_native_tunnel_establishes_and_cleans_up(
) -> Result<(), Box<dyn std::error::Error>> {
    const BYPASS_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
    const CAPTURE_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 254);

    let directory = tempfile::tempdir()?;
    let ledger = directory.path().join("routes.json");
    let plan = GatewayPlan {
        routing_mode: crate::RoutingMode::Split,
        addresses: vec!["100.64.0.1/30".parse()?],
        included_routes: vec!["1.1.1.0/24".parse()?],
        excluded_routes: Vec::new(),
        mtu: crate::Mtu::try_from(1_280)?,
        dns_policy: crate::DnsPolicy::Block,
        dns_servers: vec![IpAddr::V4(CAPTURE_TARGET)],
    };
    let options = NativeTunnelOptions::new(ledger.clone());
    #[cfg(target_os = "windows")]
    let options = match std::env::var_os("RINGS_GATEWAY_WINTUN_DLL") {
        Some(path) => options.with_wintun_dll(path.into()),
        None => options,
    };
    let mut control = NativeTunnelControl::new(options.clone())?;
    control
        .replace_bypass_targets(&[IpAddr::V4(BYPASS_TARGET)])
        .await?;
    let EstablishedTunnel {
        mut device,
        lease,
        interface_name,
    } = control.establish(&plan).await?;
    let ledger_was_written = ledger.exists();

    let observation = async {
        let response = probe_bypass_http(BYPASS_TARGET).await?;
        let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.send_to(b"must-capture", (CAPTURE_TARGET, 9))?;
        let mut packet = vec![0_u8; usize::from(plan.mtu.get())];
        let captured = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let length = device
                    .read_packet(packet.as_mut_slice())
                    .await
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                let Some(ipv4) = packet.get(..length) else {
                    return Err(std::io::Error::other(
                        "native packet device returned an invalid length",
                    ));
                };
                let Some(destination) = ipv4_destination(ipv4) else {
                    continue;
                };
                if destination == BYPASS_TARGET {
                    return Err(std::io::Error::other(
                        "reachable underlay target was also captured by the tunnel",
                    ));
                }
                if destination == CAPTURE_TARGET {
                    return Ok(length);
                }
            }
        })
        .await
        .map_err(|_| std::io::Error::other("timed out waiting for captured packet"))??;
        Ok::<_, std::io::Error>((response, captured))
    }
    .await;
    drop(device);
    let teardown = control.teardown(lease).await;
    teardown?;
    let (bypass_response, captured_length) = observation?;

    assert!(!interface_name.is_empty());
    assert!(ledger_was_written);
    assert!(bypass_response.starts_with(b"HTTP/1."));
    assert!(captured_length >= 20);
    assert!(!ledger.exists());

    let EstablishedTunnel {
        device: stale_device,
        lease: _stale_lease,
        ..
    } = control.establish(&plan).await?;
    assert!(ledger.exists());
    drop(stale_device);
    drop(control);

    let mut recovery = NativeTunnelControl::new(options)?;
    recovery.reconcile_stale()?;
    assert!(!ledger.exists());
    Ok(())
}

async fn probe_bypass_http(target: Ipv4Addr) -> std::io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let timeout = Duration::from_secs(10);
        let mut stream =
            std::net::TcpStream::connect_timeout(&SocketAddrV4::new(target, 80).into(), timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(b"HEAD / HTTP/1.1\r\nHost: 1.1.1.1\r\nConnection: close\r\n\r\n")?;
        let mut response = vec![0_u8; 256];
        let length = stream.read(response.as_mut_slice())?;
        if length == 0 {
            return Err(std::io::Error::other(
                "underlay HTTP probe returned an empty response",
            ));
        }
        response.truncate(length);
        Ok(response)
    })
    .await
    .map_err(|error| std::io::Error::other(format!("underlay probe task failed: {error}")))?
}

fn ipv4_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    if packet.first().map(|version| version >> 4) != Some(4) {
        return None;
    }
    let octets: [u8; 4] = packet.get(16..20)?.try_into().ok()?;
    Some(Ipv4Addr::from(octets))
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
