use std::error::Error;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::SocketAddrV4;
use std::time::Duration;
use std::time::Instant;

use rings_gateway::bindings::NativePacketIo;
use rings_gateway::GatewayPlan;
use rings_gateway::Mtu;
use rings_gateway::PacketIo;

pub const UNSELECTED_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
pub const CAPTURE_TARGET: Ipv4Addr = Ipv4Addr::new(1, 0, 0, 1);
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

pub fn gateway_plan() -> TestResult<GatewayPlan> {
    Ok(GatewayPlan {
        addresses: vec!["100.64.0.1/32".parse()?],
        included_routes: vec!["1.0.0.0/24".parse()?],
        mtu: Mtu::try_from(1_280)?,
    })
}

pub async fn probe_http(target: Ipv4Addr) -> io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect_timeout(
            &SocketAddrV4::new(target, 80).into(),
            IO_TIMEOUT,
        )?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.write_all(
            format!("HEAD / HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n").as_bytes(),
        )?;
        let mut response = vec![0_u8; 256];
        let length = stream.read(response.as_mut_slice())?;
        if length == 0 {
            return Err(io::Error::other("HTTP probe returned an empty response"));
        }
        response.truncate(length);
        Ok(response)
    })
    .await
    .map_err(|error| io::Error::other(format!("HTTP probe task failed: {error}")))?
}

pub async fn capture_packet(device: &mut NativePacketIo, plan: &GatewayPlan) -> io::Result<usize> {
    let mut packet = vec![0_u8; usize::from(plan.mtu.get())];
    let capture = tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            let length = device
                .read_packet(packet.as_mut_slice())
                .await
                .map_err(|error| io::Error::other(error.to_string()))?;
            let Some(ipv4) = packet.get(..length) else {
                return Err(io::Error::other(
                    "packet device returned an invalid packet length",
                ));
            };
            let Some(destination) = ipv4_destination(ipv4) else {
                continue;
            };
            if destination == UNSELECTED_TARGET {
                return Err(io::Error::other(
                    "unselected target was captured by the tunnel",
                ));
            }
            if destination == CAPTURE_TARGET {
                return Ok(length);
            }
        }
    });
    let (captured, injection) = tokio::join!(capture, inject_captured_udp(CAPTURE_TARGET));
    let captured = captured.map_err(|_| io::Error::other("timed out waiting for packet"))??;
    injection?;
    Ok(captured)
}

pub fn assert_exact_capture_ledger(ledger: &std::path::Path) -> TestResult {
    let contents = std::fs::read_to_string(ledger)?;
    let json: serde_json::Value = serde_json::from_str(&contents)?;
    let routes = json
        .get("routes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| io::Error::other("route ledger has no route array"))?;
    let destinations = routes
        .iter()
        .map(|route| {
            let destination = route
                .get("destination")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| io::Error::other("ledger route has no destination"))?;
            let prefix = route
                .get("prefix")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| io::Error::other("ledger route has no prefix"))?;
            Ok((destination, prefix))
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(destinations, vec![("1.0.0.0", 24)]);
    Ok(())
}

async fn inject_captured_udp(target: Ipv4Addr) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            match socket.send_to(b"must-capture", (target, 9)) {
                Ok(_) => return Ok(()),
                Err(error)
                    if capture_route_may_be_converging(&error) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(io_context("captured UDP injection", error)),
            }
        }
    })
    .await
    .map_err(|error| io::Error::other(format!("captured UDP injection task failed: {error}")))?
}

fn capture_route_may_be_converging(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable
    )
}

fn io_context(operation: &'static str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{operation} failed: {error}"))
}

fn ipv4_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    if packet.first().map(|version| version >> 4) != Some(4) {
        return None;
    }
    let octets: [u8; 4] = packet.get(16..20)?.try_into().ok()?;
    Some(Ipv4Addr::from(octets))
}
