#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::Ipv4Addr;
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
use rings_gateway::testing::receive_matching_tcp;
use rings_gateway::testing::ChannelPacketIo;
use rings_gateway::testing::ClientPath;
use rings_gateway::GatewayConfig;
use rings_gateway::GatewayPlan;
use rings_gateway::GatewayRuntime;
use rings_gateway::Mtu;
use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpSeqNumber;
use tokio::sync::mpsc;
#[cfg(target_os = "linux")]
use tokio::sync::oneshot;

use super::common::*;
use super::*;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::NativeOnionGatewayConnector;
use crate::test_support::TestTask;
use crate::tests::activity::probe_on_activity;
use crate::tests::activity::AdmissionActivity;
use crate::tests::exit_descriptor::ExitDescriptorFixture;
use crate::tests::native::prepare_processor_with_onion_role;
use crate::tests::native::signal_edge;

const PUBLIC_HTTP_PORT: u16 = 80;
const PUBLIC_HTTP_IPV4: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
const CLIENT_PORT: u16 = 41_000;

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

/// The captured client's path to the public HTTP target.
const CLIENT: ClientPath = ClientPath {
    source: SocketAddrV4::new(Ipv4Addr::new(100, 64, 0, 2), CLIENT_PORT),
    target: SocketAddrV4::new(PUBLIC_HTTP_IPV4, PUBLIC_HTTP_PORT),
    window_len: 16_384,
};

/// Emit one client segment on [`CLIENT`].
fn client_packet(
    control: TcpControl,
    sequence: TcpSeqNumber,
    acknowledgment: Option<TcpSeqNumber>,
    payload: &[u8],
) -> Vec<u8> {
    CLIENT
        .packet(control, sequence, acknowledgment, payload)
        .expect("captured client packet")
}

/// Connect `first` to `second` and await, on admission activity, both admitting the edge.
///
/// Stabilization connects overlay neighbours by itself once some edges exist, so the manual
/// handshake may meet the overlay's own ([`signal_edge`]); the tie-break guarantees the
/// crossing handshakes end with one edge (#947).
async fn connect_gateway_edge(first: &Processor, second: &Processor, label: &str) {
    if let Err(error) = signal_edge(first, second).await {
        panic!("gateway WebRTC edge {label} handshake: {error:?}");
    }
    probe_on_activity(
        &format!("gateway WebRTC edge {label} admitted on both ends"),
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let admitted = processor_has_admitted_peer(first, second.did())
                && processor_has_admitted_peer(second, first.did());
            async move { Ok(admitted.then_some(())) }
        },
    )
    .await
    .unwrap_or_else(|error| panic!("gateway WebRTC edge {label} did not connect: {error:?}"));
}

/// Serve `provider`'s application inbox with the extension backend as a node task, with
/// [`AdmissionActivity`] observing it, so an edge awaited by [`connect_gateway_edge`] wakes its
/// probe on every admission and retirement.
fn serve_observing_admissions(provider: &Provider) -> Result<()> {
    provider.serve_application(Some(Arc::new(AdmissionActivity)))
}

/// Relays of the gateway fixture besides the exit: an onion loop over one symbol has
/// `H − 1 = 4` distinct hops, the exit included (#834 D5).
const GATEWAY_FIXTURE_RELAYS: usize = 3;

struct OnionLoopGatewayFixture {
    runtime: GatewayRuntime,
    _processors: Vec<Arc<Processor>>,
    _providers: Vec<Provider>,
}

/// A client, three relays and a public TCP exit over real WebRTC edges.
///
/// Route selection draws the guard and both relay positions among the three relays, so the
/// relays form a full mesh with an edge to the client and to the exit each: every loop the
/// client can select has its forward prefix `g → r₀,₂ → exit` on live edges.
async fn prepare_onion_loop_public_gateway(
    config: GatewayConfig,
) -> Result<OnionLoopGatewayFixture> {
    // Use a literal global address so host DNS proxies and fake-IP modes cannot turn this into a
    // synthetic 198.18.0.0/15 target that the exit policy must reject.
    let authority = format!("{PUBLIC_HTTP_IPV4}:{PUBLIC_HTTP_PORT}");
    let mut exit_policy = onion_policy(&[authority.as_str()], &[])?;
    exit_policy.max_sessions = 8;
    exit_policy.max_bytes_per_minute = 1_048_576;

    let client = Arc::new(prepare_processor().await);
    let exit = Arc::new(
        prepare_processor_with_onion_role(OnionRole::Exit(OnionExitOffer::new(
            [OnionServiceName::tcp()],
            exit_policy.clone(),
        )?))
        .await,
    );
    let mut relays = Vec::with_capacity(GATEWAY_FIXTURE_RELAYS);
    for _ in 0..GATEWAY_FIXTURE_RELAYS {
        relays.push(Arc::new(
            prepare_processor_with_onion_role(OnionRole::Relay).await,
        ));
    }
    let client_provider = Provider::from_processor(Arc::clone(&client));
    let exit_provider = Provider::from_processor(Arc::clone(&exit));
    let relay_providers = relays
        .iter()
        .map(|relay| Provider::from_processor(Arc::clone(relay)))
        .collect::<Vec<_>>();
    let client_onion = client_provider.onion_circuit()?;
    for provider in relay_providers.iter() {
        provider.onion_circuit()?;
    }
    exit_provider.onion_circuit()?;
    serve_observing_admissions(&client_provider)?;
    serve_observing_admissions(&exit_provider)?;
    for provider in relay_providers.iter() {
        serve_observing_admissions(provider)?;
    }

    for (index, relay) in relays.iter().enumerate() {
        connect_gateway_edge(&client, relay, "client-relay").await;
        connect_gateway_edge(relay, &exit, "relay-exit").await;
        for later in relays.iter().skip(index + 1) {
            connect_gateway_edge(relay, later, "relay-relay").await;
        }
    }

    // The overlay's views still move here, so a lookup may reach an owner that never received
    // these writes; the client reads them all the same, as their writer (#948, #982).
    let now_ms = get_epoch_ms();
    client
        .storage_store(Processor::registry_entry(
            ONLINE_NODES_TOPIC,
            relays
                .iter()
                .chain([&exit])
                .map(|relay| online_relay_descriptor_for_processor(relay, now_ms))
                .collect::<Result<Vec<_>>>()?,
        )?)
        .await?;
    client
        .storage_store(Processor::registry_entry(ONION_EXITS_TOPIC, vec![
            ExitDescriptorFixture::live(&exit, OnionServiceName::tcp(), exit_policy, now_ms)
                .sign()?,
        ])?)
        .await?;

    let preview = client
        .build_onion_proxy_route(
            OnionProxyProtocol::TcpConnect,
            OnionProxyTarget::parse_authority(&authority)?,
        )
        .await?;
    let relay_dids = relays.iter().map(|relay| relay.did()).collect::<Vec<_>>();
    assert_eq!(preview.route.hops().positions().count(), 5);
    assert!(relay_dids.contains(&preview.route.hops().guard().did));
    assert_eq!(preview.route.exit_did(), exit.did());

    let connector = Arc::new(NativeOnionGatewayConnector::new(
        Arc::clone(&client),
        client_onion,
    ));
    let runtime =
        GatewayRuntime::new(config, connector, 23).expect("construct gateway runtime fixture");
    let mut processors = vec![client, exit];
    processors.extend(relays);
    let mut providers = vec![client_provider, exit_provider];
    providers.extend(relay_providers);
    Ok(OnionLoopGatewayFixture {
        runtime,
        _processors: processors,
        _providers: providers,
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
#[ignore = "requires public network access and five native WebRTC processors"]
async fn captured_tcp_reaches_public_http_only_through_onion_loop() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let OnionLoopGatewayFixture {
        mut runtime,
        _processors,
        _providers,
    } = prepare_onion_loop_public_gateway(gateway_config()).await?;
    runtime
        .activate("memory-packet-io".to_string())
        .expect("activate gateway runtime");
    let status = runtime.status_handle();
    let (ingress_tx, ingress_rx) = mpsc::channel(16);
    let (egress_tx, mut egress_rx) = mpsc::channel(32);
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = TestTask::spawn(async move {
        let mut device = ChannelPacketIo {
            ingress: ingress_rx,
            egress: egress_tx,
        };
        runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await
    });

    ingress_tx
        .send(client_packet(TcpControl::Syn, TcpSeqNumber(7), None, &[]))
        .await
        .expect("send captured SYN");
    let syn_ack = receive_matching_tcp(&mut egress_rx, Duration::from_secs(30), |packet| {
        packet.acknowledgment.is_some() && packet.payload.is_empty() && !packet.fin
    })
    .await
    .expect("gateway SYN-ACK");
    let server_next = syn_ack.sequence + 1;
    ingress_tx
        .send(client_packet(
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
            TcpControl::None,
            TcpSeqNumber(8),
            Some(server_next),
            request,
        ))
        .await
        .expect("send captured HTTP request");
    let response = receive_matching_tcp(&mut egress_rx, Duration::from_secs(35), |packet| {
        packet.rst || !packet.payload.is_empty()
    })
    .await
    .unwrap_or_else(|error| panic!("gateway HTTP response: {error}: {:?}", status.snapshot()));
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
async fn real_linux_tun_tcp_reaches_public_http_through_onion_loop() -> Result<()> {
    let _network_guard = network_test_guard().await;
    let config = gateway_config_for(&["1.1.1.1/32"]);
    let plan = config.plan.clone();
    let OnionLoopGatewayFixture {
        mut runtime,
        _processors,
        _providers,
    } = prepare_onion_loop_public_gateway(config).await?;

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
    let mut task = TestTask::spawn(async move {
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
