//! Privileged process-level proof for the production Unix helper boundary.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::error::Error;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddrV4;
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
use rings_gateway::bindings::UnderlayPolicy;
use rings_gateway::DnsPolicy;
use rings_gateway::GatewayPlan;
use rings_gateway::Mtu;
use rings_gateway::PacketIo;
use rings_gateway::RoutingMode;

const BYPASS_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
const CAPTURE_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 254);
const EXTRA_BYPASS_TARGET: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
const HELPER_START_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::test]
#[ignore = "requires root TUN/route privileges and public TCP reachability"]
async fn privileged_helper_transfers_tun_and_cleans_normal_and_disconnected_leases() -> TestResult {
    let directory = tempfile::Builder::new()
        .prefix("rings-gateway-helper-")
        .tempdir_in("/var/run")?;
    let socket = directory.path().join("helper.sock");
    let ledger = directory.path().join("routes.json");
    let plan = gateway_plan()?;

    let mut helper = HelperProcess::spawn(&socket, &ledger)?;
    helper.wait_for_socket(&socket).await?;
    let mut control = UnixTunnelControl::new(UnixTunnelOptions::new(socket.clone()));
    control
        .replace_bypass_targets(&[IpAddr::V4(BYPASS_TARGET)])
        .await?;
    let EstablishedTunnel {
        mut device,
        lease,
        interface_name,
    } = control.establish(&plan).await?;
    assert!(ledger.exists());

    control
        .replace_bypass_targets(&[IpAddr::V4(BYPASS_TARGET), IpAddr::V4(EXTRA_BYPASS_TARGET)])
        .await?;
    control
        .replace_bypass_targets(&[IpAddr::V4(BYPASS_TARGET)])
        .await?;

    let response = probe_bypass_http(BYPASS_TARGET).await?;
    let captured_length = capture_packet(&mut device, &plan).await?;
    drop(device);
    control.teardown(lease).await?;
    helper.wait_for_success().await?;

    assert!(!interface_name.is_empty());
    assert!(response.starts_with(b"HTTP/1."));
    assert!(captured_length >= 20);
    assert!(!ledger.exists());
    assert!(!socket.exists());

    let mut disconnected_helper = HelperProcess::spawn(&socket, &ledger)?;
    disconnected_helper.wait_for_socket(&socket).await?;
    let mut disconnected = UnixTunnelControl::new(UnixTunnelOptions::new(socket.clone()));
    disconnected
        .replace_bypass_targets(&[IpAddr::V4(BYPASS_TARGET)])
        .await?;
    let EstablishedTunnel {
        device,
        lease: _lease,
        interface_name: _,
    } = disconnected.establish(&plan).await?;
    assert!(ledger.exists());
    drop(device);
    drop(disconnected);
    disconnected_helper.wait_for_success().await?;

    assert!(!ledger.exists());
    assert!(!socket.exists());
    Ok(())
}

fn gateway_plan() -> TestResult<GatewayPlan> {
    Ok(GatewayPlan {
        routing_mode: RoutingMode::Split,
        addresses: vec!["100.64.0.1/30".parse()?],
        included_routes: vec!["1.1.1.0/24".parse()?],
        excluded_routes: Vec::new(),
        mtu: Mtu::try_from(1_280)?,
        dns_policy: DnsPolicy::Block,
        dns_servers: vec![IpAddr::V4(CAPTURE_TARGET)],
    })
}

async fn probe_bypass_http(target: Ipv4Addr) -> io::Result<Vec<u8>> {
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
            return Err(io::Error::other(
                "underlay HTTP probe returned an empty response",
            ));
        }
        response.truncate(length);
        Ok(response)
    })
    .await
    .map_err(|error| io::Error::other(format!("underlay probe task failed: {error}")))?
}

async fn capture_packet(
    device: &mut rings_gateway::bindings::NativePacketIo,
    plan: &GatewayPlan,
) -> io::Result<usize> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.send_to(b"must-capture", (CAPTURE_TARGET, 9))?;
    let mut packet = vec![0_u8; usize::from(plan.mtu.get())];
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let length = device
                .read_packet(packet.as_mut_slice())
                .await
                .map_err(|error| io::Error::other(error.to_string()))?;
            let Some(ipv4) = packet.get(..length) else {
                return Err(io::Error::other(
                    "helper packet device returned an invalid length",
                ));
            };
            let Some(destination) = ipv4_destination(ipv4) else {
                continue;
            };
            if destination == BYPASS_TARGET {
                return Err(io::Error::other(
                    "underlay bypass target was captured by the helper tunnel",
                ));
            }
            if destination == CAPTURE_TARGET {
                return Ok(length);
            }
        }
    })
    .await
    .map_err(|_| io::Error::other("timed out waiting for helper-captured packet"))?
}

fn ipv4_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    if packet.first().map(|version| version >> 4) != Some(4) {
        return None;
    }
    let octets: [u8; 4] = packet.get(16..20)?.try_into().ok()?;
    Some(Ipv4Addr::from(octets))
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
