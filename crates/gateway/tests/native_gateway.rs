//! Privileged proof for the direct native tunnel control.

#![cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]

mod support;

use rings_gateway::bindings::EstablishedTunnel;
use rings_gateway::bindings::NativeTunnelControl;
use rings_gateway::bindings::NativeTunnelOptions;
use rings_gateway::bindings::TunnelControl;
use support::assert_exact_capture_ledger;
use support::capture_packet;
use support::gateway_plan;
use support::TestResult;

#[tokio::test]
#[ignore = "requires TUN/route privileges"]
async fn privileged_native_tunnel_establishes_and_cleans_up() -> TestResult {
    let directory = tempfile::tempdir()?;
    let ledger = directory.path().join("routes.json");
    let plan = gateway_plan()?;
    let options = NativeTunnelOptions::new(ledger.clone());
    #[cfg(target_os = "windows")]
    let options = match std::env::var_os("RINGS_GATEWAY_WINTUN_DLL") {
        Some(path) => options.with_wintun_dll(path.into()),
        None => options,
    };
    let mut control = NativeTunnelControl::new(options.clone())?;
    let EstablishedTunnel {
        mut device,
        lease,
        interface_name,
    } = control.establish(&plan).await?;

    assert_exact_capture_ledger(&ledger)?;
    let captured_length = capture_packet(&mut device, &plan).await?;
    control
        .teardown(lease)
        .await
        .map_err(rings_gateway::bindings::TeardownFailure::into_error)?;
    drop(device);

    assert!(!interface_name.is_empty());
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
