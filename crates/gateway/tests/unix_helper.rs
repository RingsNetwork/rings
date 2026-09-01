//! Privileged process-level proof for the production Unix helper boundary.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod support;

use std::io;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use rings_gateway::bindings::unix::UnixTunnelControl;
use rings_gateway::bindings::unix::UnixTunnelOptions;
use rings_gateway::bindings::EstablishedTunnel;
use rings_gateway::bindings::TunnelControl;
use support::assert_exact_capture_ledger;
use support::capture_packet;
use support::gateway_plan;
use support::probe_http;
use support::TestResult;
use support::CAPTURE_TARGET;
use support::UNSELECTED_TARGET;

const HELPER_START_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const HELPER_STATE_PARENT: &str = "/var/run";
#[cfg(target_os = "macos")]
const HELPER_STATE_PARENT: &str = "/var/db";

#[tokio::test]
#[ignore = "requires root TUN/route privileges and public TCP reachability"]
async fn privileged_helper_retains_and_recovers_a_disconnected_lease() -> TestResult {
    let baseline = probe_http(CAPTURE_TARGET).await?;
    let directory = tempfile::Builder::new()
        .prefix("rings-gateway-helper-")
        .tempdir_in(HELPER_STATE_PARENT)?;
    let socket = directory.path().join("helper.sock");
    let ledger = directory.path().join("routes.json");
    let plan = gateway_plan()?;

    let mut helper = HelperProcess::spawn(&socket, &ledger)?;
    helper.wait_for_socket(&socket).await?;
    let mut control = UnixTunnelControl::new(UnixTunnelOptions::new(socket.clone()));
    let EstablishedTunnel {
        mut device,
        lease,
        interface_name,
    } = control.establish(&plan).await?;
    assert_exact_capture_ledger(&ledger)?;

    let unselected_response = probe_http(UNSELECTED_TARGET).await?;
    let first_capture = capture_packet(&mut device, &plan).await?;
    drop(device);
    drop(lease);
    drop(control);

    helper.assert_running()?;
    assert!(ledger.exists());
    assert!(probe_http(CAPTURE_TARGET).await.is_err());

    let mut resumed = UnixTunnelControl::new(UnixTunnelOptions::new(socket.clone()));
    let EstablishedTunnel {
        mut device,
        lease,
        interface_name: resumed_interface_name,
    } = resumed.establish(&plan).await?;
    let resumed_capture = capture_packet(&mut device, &plan).await?;
    resumed
        .teardown(lease)
        .await
        .map_err(rings_gateway::bindings::TeardownFailure::into_error)?;
    drop(device);
    helper.wait_for_success().await?;

    assert!(baseline.starts_with(b"HTTP/1."));
    assert!(!interface_name.is_empty());
    assert_eq!(resumed_interface_name, interface_name);
    assert!(unselected_response.starts_with(b"HTTP/1."));
    assert!(first_capture >= 20);
    assert!(resumed_capture >= 20);
    assert!(!ledger.exists());
    assert!(!socket.exists());
    Ok(())
}

struct HelperProcess {
    child: Child,
    reaped: bool,
}

impl HelperProcess {
    fn spawn(socket: &Path, ledger: &Path) -> io::Result<Self> {
        let child = Command::new(env!("CARGO_BIN_EXE_gateway-config-unix"))
            .arg("--socket")
            .arg(socket)
            .arg("--ledger")
            .arg(ledger)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Self {
            child,
            reaped: false,
        })
    }

    async fn wait_for_socket(&mut self, socket: &Path) -> io::Result<()> {
        let deadline = Instant::now() + HELPER_START_TIMEOUT;
        loop {
            if socket.exists() {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return Err(io::Error::other(format!(
                    "gateway-config-unix exited before binding its socket: {status}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "timed out waiting for gateway-config-unix socket",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn assert_running(&mut self) -> io::Result<()> {
        match self.child.try_wait()? {
            None => Ok(()),
            Some(status) => {
                self.reaped = true;
                Err(io::Error::other(format!(
                    "gateway-config-unix exited while retaining a disconnected lease: {status}"
                )))
            }
        }
    }

    async fn wait_for_success(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + HELPER_EXIT_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "gateway-config-unix exited unsuccessfully: {status}"
                    )))
                };
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "timed out waiting for gateway-config-unix to exit",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}
