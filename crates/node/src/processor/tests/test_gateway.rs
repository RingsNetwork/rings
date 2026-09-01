#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::Ipv4Addr;
#[cfg(target_os = "linux")]
use std::net::SocketAddrV4;
#[cfg(target_os = "linux")]
use std::net::TcpStream;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
use rings_gateway::bindings::NativePacketIo;
#[cfg(target_os = "linux")]
use rings_gateway::bindings::NativeTunnelControl;
#[cfg(target_os = "linux")]
use rings_gateway::bindings::NativeTunnelLease;
#[cfg(target_os = "linux")]
use rings_gateway::bindings::NativeTunnelOptions;
#[cfg(target_os = "linux")]
use rings_gateway::bindings::TunnelControl;
use rings_gateway::GatewayConfig;
use rings_gateway::GatewayPlan;
use rings_gateway::GatewayRuntime;
use rings_gateway::Mtu;
use rings_gateway::PacketIo;
use rings_gateway::PacketIoError;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use smoltcp::wire::Ipv4Repr;
use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpPacket;
use smoltcp::wire::TcpRepr;
use smoltcp::wire::TcpSeqNumber;
use tokio::sync::mpsc;
#[cfg(target_os = "linux")]
use tokio::sync::oneshot;

use super::common::*;
use super::*;
use crate::onion::proxy::OnionProxyConfig;
use crate::onion::tcp::NativeOnionCircuitHandle;
use crate::onion::tcp::NativeOnionTcpExitConfig;
use crate::onion::NativeOnionGatewayConnector;

const PUBLIC_HTTP_PORT: u16 = 80;
const PUBLIC_HTTP_IPV4: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
const CLIENT_PORT: u16 = 41_000;

struct ChannelPacketIo {
    ingress: mpsc::Receiver<Vec<u8>>,
    egress: mpsc::Sender<Vec<u8>>,
}

#[async_trait::async_trait]
impl PacketIo for ChannelPacketIo {
    async fn read_packet(
        &mut self,
        output: &mut [u8],
    ) -> std::result::Result<usize, PacketIoError> {
        let packet = self.ingress.recv().await.ok_or(PacketIoError::Closed)?;
        let capacity = output.len();
        let Some(destination) = output.get_mut(..packet.len()) else {
            return Err(PacketIoError::InvalidLength {
                length: packet.len(),
                capacity,
            });
        };
        destination.copy_from_slice(&packet);
        Ok(packet.len())
    }

    async fn write_packet(&mut self, packet: &[u8]) -> std::result::Result<(), PacketIoError> {
        self.egress.send(packet.to_vec()).await.map_err(|error| {
            PacketIoError::Write(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                error.to_string(),
            ))
        })
    }
}

struct TcpObservation {
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    fin: bool,
    rst: bool,
    payload: Vec<u8>,
}

impl TcpObservation {
    fn parse(packet: &[u8]) -> Self {
        let ipv4 = Ipv4Packet::new_checked(packet).expect("valid gateway IPv4 packet");
        let tcp = TcpPacket::new_checked(ipv4.payload()).expect("valid gateway TCP packet");
        Self {
            sequence: tcp.seq_number(),
            acknowledgment: tcp.ack().then(|| tcp.ack_number()),
            fin: tcp.fin(),
            rst: tcp.rst(),
            payload: tcp.payload().to_vec(),
        }
    }
}

fn gateway_config() -> GatewayConfig {
    gateway_config_for(&["1.1.1.1/32"])
}

fn gateway_config_for(included_routes: &[&str]) -> GatewayConfig {
    GatewayConfig {
        plan: GatewayPlan {
            addresses: vec!["100.64.0.1/32".parse().expect("gateway address")],
            included_routes: included_routes
                .iter()
                .map(|route| route.parse().expect("capture route"))
                .collect(),
            mtu: Mtu::try_from(1_280).expect("gateway MTU"),
        },
        max_flows: 8,
        flow_idle_timeout: Duration::from_secs(30),
        tcp_buffer_bytes: 64 * 1_024,
    }
}

fn client_packet(
    target: Ipv4Addr,
    control: TcpControl,
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    payload: &[u8],
) -> Vec<u8> {
    let source = Ipv4Addr::new(100, 64, 0, 2);
    let tcp = TcpRepr {
        src_port: CLIENT_PORT,
        dst_port: PUBLIC_HTTP_PORT,
        control,
        seq_number: sequence,
        ack_number: acknowledgment,
        window_len: 16_384,
        window_scale: None,
        max_seg_size: Some(1_200),
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload,
    };
    let ipv4 = Ipv4Repr {
        src_addr: source,
        dst_addr: target,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut packet = vec![0_u8; ipv4.buffer_len() + tcp.buffer_len()];
    ipv4.emit(
        &mut Ipv4Packet::new_unchecked(&mut packet),
        &ChecksumCapabilities::default(),
    );
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut packet[ipv4.buffer_len()..]),
        &source.into(),
        &target.into(),
        &ChecksumCapabilities::default(),
    );
    packet
}

async fn receive_tcp(
    egress: &mut mpsc::Receiver<Vec<u8>>,
    predicate: impl Fn(&TcpObservation) -> bool,
) -> TcpObservation {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let packet = egress.recv().await.expect("gateway egress remains open");
            let observation = TcpObservation::parse(&packet);
            if predicate(&observation) {
                return observation;
            }
        }
    })
    .await
    .expect("gateway TCP response deadline")
}

async fn connect_gateway_edge(first: &Processor, second: &Processor, label: &str) {
    let offer = first
        .swarm
        .create_offer(second.did())
        .await
        .expect("create gateway edge offer");
    let answer = second
        .swarm
        .answer_offer(offer)
        .await
        .expect("answer gateway edge offer");
    first
        .swarm
        .accept_answer(answer)
        .await
        .expect("accept gateway edge answer");
    tokio::time::timeout(Duration::from_secs(20), async {
        while !processor_has_connected_peer(first, second.did())
            || !processor_has_connected_peer(second, first.did())
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("gateway WebRTC edge {label} did not connect"));
}

struct TwoHopGatewayFixture {
    runtime: GatewayRuntime,
    _processors: Vec<Arc<Processor>>,
    _providers: Vec<Provider>,
}

async fn prepare_two_hop_public_gateway(config: GatewayConfig) -> Result<TwoHopGatewayFixture> {
    // Use a literal global address so host DNS proxies and fake-IP modes cannot turn this into a
    // synthetic 198.18.0.0/15 target that the exit policy must reject.
    let authority = format!("{PUBLIC_HTTP_IPV4}:{PUBLIC_HTTP_PORT}");
    let mut exit_policy = onion_policy(&[authority.as_str()], &[])?;
    exit_policy.max_circuits = 8;
    exit_policy.max_streams_per_circuit = 2;
    exit_policy.max_bytes_per_minute = 1_048_576;

    let client = Arc::new(prepare_processor().await);
    let relay = Arc::new(prepare_processor().await);
    let exit = Arc::new(prepare_processor().await);
    let client_provider = Provider::from_processor(Arc::clone(&client));
    let relay_provider = Provider::from_processor(Arc::clone(&relay));
    let exit_provider = Provider::from_processor(Arc::clone(&exit));
    let client_onion = NativeOnionCircuitHandle::install(
        &client_provider.extensions(),
        client.session_sk().clone(),
        false,
        None,
    )?;
    let _relay_onion = NativeOnionCircuitHandle::install(
        &relay_provider.extensions(),
        relay.session_sk().clone(),
        true,
        None,
    )?;
    let _exit_onion = NativeOnionCircuitHandle::install(
        &exit_provider.extensions(),
        exit.session_sk().clone(),
        false,
        Some(NativeOnionTcpExitConfig::tcp(exit_policy.clone())),
    )?;
    client_provider.set_backend()?;
    relay_provider.set_backend()?;
    exit_provider.set_backend()?;

    connect_gateway_edge(&client, &relay, "client-relay").await;
    connect_gateway_edge(&relay, &exit, "relay-exit").await;

    let now_ms = get_epoch_ms();
    client
        .storage_store(Processor::online_node_registry_entry(vec![
            online_relay_descriptor_for_processor(&relay, now_ms)?,
        ])?)
        .await?;
    client
        .storage_store(Processor::onion_exit_registry_entry(vec![
            onion_exit_descriptor_for_processor_with_service(
                &exit,
                OnionExitService::tcp(),
                now_ms,
                exit_policy,
            )?,
        ])?)
        .await?;

    let proxy = OnionProxyConfig::tcp_connect(2, false);
    let preview = client
        .build_onion_proxy_route(
            proxy.clone(),
            OnionProxyTarget::parse_authority(&authority)?,
        )
        .await?;
    assert_eq!(preview.route.hops().len(), 2);
    assert_eq!(preview.route.hops().first(), Some(&relay.did()));
    assert_eq!(preview.route.exit_did(), exit.did());

    let connector = Arc::new(NativeOnionGatewayConnector::new(
        Arc::clone(&client),
        client_onion,
        proxy,
    ));
    let runtime =
        GatewayRuntime::new(config, connector, 23).expect("construct gateway runtime fixture");
    Ok(TwoHopGatewayFixture {
        runtime,
        _processors: vec![client, relay, exit],
        _providers: vec![client_provider, relay_provider, exit_provider],
    })
}

#[cfg(target_os = "linux")]
struct LinuxNamespaceSetup {
    runtime: tokio::runtime::Runtime,
    control: NativeTunnelControl,
    lease: Option<NativeTunnelLease>,
    descriptor: OwnedFd,
    interface_name: String,
}

#[cfg(target_os = "linux")]
enum LinuxNamespaceCommand {
    Connect(oneshot::Sender<std::result::Result<Vec<u8>, String>>),
    Teardown,
}

#[cfg(target_os = "linux")]
struct LinuxNamespaceControl {
    commands: std::sync::mpsc::Sender<LinuxNamespaceCommand>,
    worker: std::thread::JoinHandle<std::result::Result<(), String>>,
}

#[cfg(target_os = "linux")]
impl LinuxNamespaceControl {
    async fn request_public_http(&self) -> std::result::Result<Vec<u8>, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(LinuxNamespaceCommand::Connect(response_tx))
            .map_err(|error| format!("send namespace HTTP command: {error}"))?;
        tokio::time::timeout(Duration::from_secs(45), response_rx)
            .await
            .map_err(|_| "namespace HTTP response deadline".to_string())?
            .map_err(|error| format!("namespace HTTP worker dropped its response: {error}"))?
    }

    async fn teardown(self) -> std::result::Result<(), String> {
        let send = self
            .commands
            .send(LinuxNamespaceCommand::Teardown)
            .map_err(|error| format!("send namespace teardown command: {error}"));
        let joined = tokio::task::spawn_blocking(move || self.worker.join())
            .await
            .map_err(|error| format!("join namespace worker task: {error}"))?
            .map_err(|_| "namespace worker panicked".to_string())?;
        match (send, joined) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(send), Err(worker)) => Err(format!("{send}; namespace cleanup failed: {worker}")),
        }
    }
}

#[cfg(target_os = "linux")]
fn start_linux_namespace_tunnel(
    plan: GatewayPlan,
) -> std::result::Result<(OwnedFd, String, LinuxNamespaceControl), String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let setup = setup_linux_namespace(plan);
        let mut setup = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = ready_tx.send(Err(error.clone()));
                return Err(error);
            }
        };
        ready_tx
            .send(Ok((setup.descriptor, setup.interface_name)))
            .map_err(|error| format!("publish namespace tunnel: {error}"))?;

        loop {
            match command_rx.recv() {
                Ok(LinuxNamespaceCommand::Connect(response)) => {
                    let _ = response.send(linux_namespace_http_request());
                }
                Ok(LinuxNamespaceCommand::Teardown) | Err(_) => {
                    let lease = setup
                        .lease
                        .take()
                        .ok_or_else(|| "namespace tunnel lease was already consumed".to_string())?;
                    return setup
                        .runtime
                        .block_on(setup.control.teardown(lease))
                        .map_err(rings_gateway::bindings::TeardownFailure::into_error)
                        .map_err(|error| format!("teardown namespace tunnel: {error}"));
                }
            }
        }
    });

    match ready_rx.recv() {
        Ok(Ok((descriptor, interface_name))) => {
            Ok((descriptor, interface_name, LinuxNamespaceControl {
                commands: command_tx,
                worker,
            }))
        }
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(error) => {
            let _ = worker.join();
            Err(format!("namespace tunnel did not become ready: {error}"))
        }
    }
}

#[cfg(target_os = "linux")]
fn setup_linux_namespace(plan: GatewayPlan) -> std::result::Result<LinuxNamespaceSetup, String> {
    // SAFETY: `unshare(CLONE_NEWNET)` affects only this dedicated OS thread. It runs before the
    // thread creates a Tokio runtime, route manager, tunnel, or socket, so every following network
    // effect is confined to the disposable namespace while the test's Rings peers remain outside.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(format!(
            "create isolated network namespace: {}",
            std::io::Error::last_os_error()
        ));
    }
    enable_linux_loopback()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build namespace runtime: {error}"))?;
    let ledger = std::env::temp_dir().join(format!(
        "rings-gateway-netns-{}-{}.json",
        std::process::id(),
        rand::random::<u64>()
    ));
    let options = NativeTunnelOptions::new(ledger).with_interface_name("rings6840".to_string());
    let mut control = NativeTunnelControl::new(options)
        .map_err(|error| format!("open route manager: {error}"))?;
    let established = runtime
        .block_on(control.establish(&plan))
        .map_err(|error| format!("establish namespace tunnel: {error}"))?;
    let descriptor = established
        .device
        .into_owned_fd()
        .map_err(|error| format!("export namespace tunnel descriptor: {error}"))?;
    Ok(LinuxNamespaceSetup {
        runtime,
        control,
        lease: Some(established.lease),
        descriptor,
        interface_name: established.interface_name,
    })
}

#[cfg(target_os = "linux")]
fn enable_linux_loopback() -> std::result::Result<(), String> {
    // SAFETY: `socket` has no borrowed inputs. A successful descriptor is moved immediately into
    // `OwnedFd`, which closes it exactly once on every return path.
    let raw_socket =
        unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw_socket < 0 {
        return Err(format!(
            "open loopback configuration socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the successful `socket` return transfers ownership of this descriptor, and no other
    // RAII owner has been constructed for it.
    let socket = unsafe { OwnedFd::from_raw_fd(raw_socket) };
    // SAFETY: zero is a valid initial state for Linux `ifreq`; the interface name and flags union
    // member are initialized before their corresponding ioctl calls.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (destination, byte) in request.ifr_name.iter_mut().zip(b"lo\0".iter().copied()) {
        *destination = libc::c_char::try_from(byte)
            .map_err(|_| "loopback interface name is not representable as c_char".to_string())?;
    }
    // SAFETY: `request` points to writable `ifreq` storage with a terminated interface name, and
    // `socket` remains live for the ioctl duration.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) } != 0 {
        return Err(format!(
            "read loopback interface flags: {}",
            std::io::Error::last_os_error()
        ));
    }
    let up = libc::c_short::try_from(libc::IFF_UP)
        .map_err(|_| "IFF_UP is not representable as c_short".to_string())?;
    // SAFETY: SIOCGIFFLAGS initialized the `ifru_flags` union member above.
    let flags = unsafe { request.ifr_ifru.ifru_flags };
    request.ifr_ifru.ifru_flags = flags | up;
    // SAFETY: `request.ifru_flags` and its interface name are initialized for SIOCSIFFLAGS, and
    // `socket` remains live for the ioctl duration.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &request) } != 0 {
        return Err(format!(
            "enable isolated loopback interface: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_namespace_http_request() -> std::result::Result<Vec<u8>, String> {
    let target = SocketAddrV4::new(PUBLIC_HTTP_IPV4, PUBLIC_HTTP_PORT).into();
    let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(30))
        .map_err(|error| format!("connect captured kernel TCP socket: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| format!("set captured TCP read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| format!("set captured TCP write timeout: {error}"))?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 1.1.1.1\r\nConnection: close\r\n\r\n")
        .map_err(|error| format!("write captured HTTP request: {error}"))?;

    let mut response = Vec::new();
    let mut chunk = [0_u8; 4_096];
    while response.len() < 64 * 1_024 {
        let length = stream
            .read(&mut chunk)
            .map_err(|error| format!("read captured HTTP response: {error}"))?;
        if length == 0 {
            break;
        }
        let bytes = chunk
            .get(..length)
            .ok_or_else(|| "captured TCP read returned an invalid length".to_string())?;
        response.extend_from_slice(bytes);
        if response.windows(2).any(|window| window == b"\r\n") {
            break;
        }
    }
    Ok(response)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires public network access and three native WebRTC processors"]
async fn captured_tcp_reaches_public_http_only_through_two_hop_onion_route() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let target = PUBLIC_HTTP_IPV4;
    let TwoHopGatewayFixture {
        mut runtime,
        _processors,
        _providers,
    } = prepare_two_hop_public_gateway(gateway_config()).await?;
    runtime
        .activate("memory-packet-io".to_string())
        .expect("activate gateway runtime");
    let status = runtime.status_handle();
    let (ingress_tx, ingress_rx) = mpsc::channel(16);
    let (egress_tx, mut egress_rx) = mpsc::channel(32);
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let mut device = ChannelPacketIo {
            ingress: ingress_rx,
            egress: egress_tx,
        };
        runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await
    });

    ingress_tx
        .send(client_packet(
            target,
            TcpControl::Syn,
            TcpSeqNumber(7),
            None,
            &[],
        ))
        .await
        .expect("send captured SYN");
    let syn_ack = receive_tcp(&mut egress_rx, |packet| {
        packet.acknowledgment.is_some() && packet.payload.is_empty() && !packet.fin
    })
    .await;
    let server_next = syn_ack.sequence + 1;
    ingress_tx
        .send(client_packet(
            target,
            TcpControl::None,
            TcpSeqNumber(8),
            Some(server_next),
            &[],
        ))
        .await
        .expect("send captured handshake ACK");

    let request = b"GET / HTTP/1.1\r\nHost: 1.1.1.1\r\nConnection: close\r\n\r\n";
    ingress_tx
        .send(client_packet(
            target,
            TcpControl::None,
            TcpSeqNumber(8),
            Some(server_next),
            request,
        ))
        .await
        .expect("send captured HTTP request");
    let response = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let packet = egress_rx.recv().await.expect("gateway egress remains open");
            let observation = TcpObservation::parse(&packet);
            if observation.rst || !observation.payload.is_empty() {
                return observation;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("gateway HTTP response deadline: {:?}", status.snapshot()));
    assert!(
        !response.rst,
        "gateway reset captured flow: {:?}",
        status.snapshot()
    );
    assert!(response.payload.starts_with(b"HTTP/1."));
    assert!(response.payload.windows(4).any(|window| window == b" 301"));

    stop.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("gateway stop deadline")
        .expect("gateway task")
        .expect("gateway runtime exits cleanly");
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires root, Linux TUN/network namespaces, and public network access"]
async fn real_linux_tun_tcp_reaches_public_http_through_two_hop_onion_route() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let config = gateway_config_for(&["1.1.1.1/32"]);
    let plan = config.plan.clone();
    let TwoHopGatewayFixture {
        mut runtime,
        _processors,
        _providers,
    } = prepare_two_hop_public_gateway(config).await?;

    let (descriptor, interface_name, namespace) =
        start_linux_namespace_tunnel(plan).expect("establish isolated Linux TUN");
    let mut device = match NativePacketIo::from_owned_fd(descriptor) {
        Ok(device) => device,
        Err(error) => {
            namespace
                .teardown()
                .await
                .expect("cleanup namespace after descriptor import failure");
            panic!("import isolated Linux TUN descriptor: {error}");
        }
    };
    if let Err(error) = runtime.activate(interface_name) {
        drop(device);
        namespace
            .teardown()
            .await
            .expect("cleanup namespace after runtime activation failure");
        panic!("activate real Linux gateway runtime: {error}");
    }
    let status = runtime.status_handle();
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let mut task = tokio::spawn(async move {
        let result = runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await;
        (result, device)
    });

    let response = namespace.request_public_http().await;
    stop.store(true, Ordering::Release);
    let runtime_result = match tokio::time::timeout(Duration::from_secs(10), &mut task).await {
        Ok(Ok((result, device))) => {
            drop(device);
            result.map_err(|error| format!("real Linux gateway runtime: {error}"))
        }
        Ok(Err(error)) => Err(format!("join real Linux gateway runtime: {error}")),
        Err(_) => {
            task.abort();
            let _ = task.await;
            Err("real Linux gateway runtime stop deadline".to_string())
        }
    };
    let teardown = namespace.teardown().await;

    runtime_result
        .unwrap_or_else(|error| panic!("{error}; final gateway status: {:?}", status.snapshot()));
    teardown.expect("tear down isolated Linux TUN namespace");
    let response = response
        .unwrap_or_else(|error| panic!("{error}; final gateway status: {:?}", status.snapshot()));
    assert!(response.starts_with(b"HTTP/1."), "response was not HTTP");
    assert!(
        response.windows(4).any(|window| window == b" 301"),
        "public target did not return the expected redirect: {}",
        String::from_utf8_lossy(&response)
    );
    Ok(())
}
