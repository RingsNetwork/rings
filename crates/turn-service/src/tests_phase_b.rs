//! Phase B end-to-end integration tests: multi-hop private TURN over `onion::udp`, middle-hop
//! Sphinx cell blindness (Law 5), OS-level teardown verification (Law 4), and RTT / burst HOL
//! measurements (#1292).

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::swarm::OfferReply;
use rings_node::error::Error;
use rings_node::error::Result;
use rings_node::extension::ext::Envelope;
use rings_node::extension::BackendObserver;
use rings_node::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use rings_node::onion::OnionExitOffer;
use rings_node::onion::OnionExitPolicy;
use rings_node::onion::OnionInvokePolicy;
use rings_node::onion::OnionLoopClass;
use rings_node::onion::OnionRole;
use rings_node::onion::OnionServiceName;
use rings_node::onion::OnionUdpBindConfig;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_test_support::with_hang_guard;
use rings_turn::RelayPeerPolicy;
use rings_turn::TurnAdmissionPolicy;
use rings_turn::TurnDatagramRelayFactory;
use rings_turn::TurnServer;
use rings_turn::TurnServerConfig;
use stun::agent::TransactionId;
use stun::fingerprint::FINGERPRINT;
use stun::message::Getter;
use stun::message::Message;
use stun::message::MessageType;
use stun::message::CLASS_REQUEST;
use stun::message::CLASS_SUCCESS_RESPONSE;
use stun::message::METHOD_BINDING;
use stun::xoraddr::XorMappedAddress;
use tokio::net::UdpSocket;
use turn::client::Client as TurnClient;
use turn::client::ClientConfig as TurnClientConfig;
use webrtc_util::Conn;

use crate::announce_turn_service;
use crate::register_turn_ticket_invoke_backend;
use crate::request_turn_ticket;
use crate::turn_ticket_invoke_offer;
use crate::OnionUdpRelayFactory;
use crate::TurnServiceOffer;

const STUN_MAGIC_COOKIE_BYTES: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

#[derive(Default)]
struct RelayEnvelopeTap {
    recording: Mutex<bool>,
    onion_cells: Mutex<Vec<Vec<u8>>>,
}

impl RelayEnvelopeTap {
    fn start_recording(&self) {
        self.onion_cells.lock().unwrap().clear();
        *self.recording.lock().unwrap() = true;
    }

    fn take_cells(&self) -> Vec<Vec<u8>> {
        *self.recording.lock().unwrap() = false;
        std::mem::take(&mut *self.onion_cells.lock().unwrap())
    }
}

impl BackendObserver for RelayEnvelopeTap {
    fn custom_envelope(&self, _from: Did, bytes: &[u8]) {
        if !*self.recording.lock().unwrap() {
            return;
        }
        if let Ok(env) = Envelope::decode(bytes) {
            if env.namespace == ONION_CIRCUIT_NAMESPACE {
                self.onion_cells.lock().unwrap().push(env.payload.to_vec());
            }
        }
    }
}

struct PhaseBNode {
    processor: Arc<Processor>,
    provider: Arc<Provider>,
}

impl PhaseBNode {
    async fn spawn(
        role: OnionRole<OnionExitOffer>,
        turn_offer: Option<TurnServiceOffer>,
        udp_bind_cfg: Option<OnionUdpBindConfig>,
        observer: Option<Arc<dyn BackendObserver>>,
    ) -> Result<Self> {
        let key = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
        let advertise_presence = role.registers_relay();
        let config = ProcessorConfig::new(1, String::new(), key, 3_600).onion_role(role);
        let mut builder = ProcessorBuilder::from_config(&config)?
            .advertise_presence(advertise_presence)
            .storage(Box::new(rings_core::storage::MemStorage::new()))
            .replay_storage(Box::new(rings_core::storage::MemStorage::new()));
        if let Some(udp_cfg) = udp_bind_cfg {
            builder = builder.onion_udp_bind_config(udp_cfg);
        }
        if let Some(offer) = turn_offer {
            builder = announce_turn_service(builder, offer);
        }
        let processor = Arc::new(builder.build()?);
        let provider = Arc::new(Provider::from_processor(Arc::clone(&processor)));
        // Ensure onion circuit & pipeline protocol handlers are registered before claiming inbox.
        let _ = provider.onion_circuit()?;
        let _ = provider.onion_pipeline()?;
        match observer {
            Some(obs) => provider.set_backend_with_observer(obs)?,
            None => provider.set_backend()?,
        }
        Ok(Self {
            processor,
            provider,
        })
    }

    async fn shutdown(self) -> Result<()> {
        self.processor.close().await
    }
}

async fn signal_edge(first: &Processor, second: &Processor) -> Result<()> {
    let res = async {
        let (attempt, offer) = first.swarm.offer_connection(second.did()).await?;
        match second.swarm.answer_offer(offer).await? {
            OfferReply::Answer(answer) => first.swarm.accept_answer_for(attempt, answer).await,
            OfferReply::CounterOffer(counter_offer) => {
                let answer = first
                    .swarm
                    .answer_offer(counter_offer)
                    .await?
                    .answer()
                    .ok_or(rings_core::error::Error::AlreadyConnected)?;
                second.swarm.accept_answer(answer).await
            }
        }
    }
    .await;
    match res {
        Ok(())
        | Err(
            rings_core::error::Error::AlreadyConnected
            | rings_core::error::Error::ConnectionAttemptSuperseded { .. },
        ) => {}
        Err(other) => return Err(Error::CoreError(other)),
    }
    with_hang_guard(
        "await mutual peer admission",
        Duration::from_secs(30),
        async {
            while !first.swarm.peer_dids().contains(&second.did())
                || !second.swarm.peer_dids().contains(&first.did())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
    )
    .await;
    Ok(())
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn multi_hop_turn_over_onion_udp_preserves_middle_hop_blindness_and_tears_down_cleanly(
) -> Result<()> {
    #[cfg(not(feature = "dummy"))]
    let _webrtc_serial = crate::tests::webrtc_test_guard().await;
    // 1. Build the overlay nodes:
    //    - `client_node`: stock `OnionRole::Client` (acquires anonymous `tkt-…` ticket)
    //    - `entry_node`: `OnionRole::Invoke(turn.ticket)` + `TurnServer` with `OnionUdpRelayFactory`
    //    - 4 `relay_nodes`: `OnionRole::Relay` with `RelayEnvelopeTap` observing wire cells
    //    - `exit_node`: generic `OnionRole::Exit([udp])` with `OnionUdpBindConfig::loopback_for_test()`
    let invoke_policy = OnionInvokePolicy {
        max_concurrent: 16,
        max_invocations_per_minute: 64,
    };
    let exit_policy = OnionExitPolicy::from_target_strings(vec!["*:*".to_string()], Vec::new())?;
    let exit_offer = OnionExitOffer::new([OnionServiceName::udp()], exit_policy)?;

    let deferred_factory = OnionUdpRelayFactory::deferred(
        OnionLoopClass::DEFAULT,
        RelayPeerPolicy::AllowLocalAndPrivate,
    );
    let mut turn_cfg = TurnServerConfig::loopback_ephemeral();
    turn_cfg.admission = TurnAdmissionPolicy::allowlist(std::iter::empty());
    turn_cfg.max_allocations = 4;
    turn_cfg.max_reservations = 4;
    turn_cfg.custom_relay_factory =
        Some(Arc::clone(&deferred_factory) as Arc<dyn TurnDatagramRelayFactory>);
    let turn_server = Arc::new(
        TurnServer::start(turn_cfg)
            .await
            .map_err(|e| Error::InvalidConfig(e.to_string()))?,
    );
    let entry_listen_addr = turn_server.local_addr();
    let advertised_turn_url = format!("turn://1.1.1.1:{}", entry_listen_addr.port());
    let turn_offer = TurnServiceOffer::new(&advertised_turn_url, turn_server.server_pubkey())?;

    let client_node = PhaseBNode::spawn(OnionRole::Client, None, None, None).await?;
    let entry_node = PhaseBNode::spawn(
        OnionRole::Invoke(turn_ticket_invoke_offer(invoke_policy)?),
        Some(turn_offer),
        None,
        None,
    )
    .await?;
    deferred_factory.attach_provider(&entry_node.provider)?;
    register_turn_ticket_invoke_backend(
        &entry_node.provider.onion_pipeline()?,
        invoke_policy,
        Arc::clone(&turn_server),
    )?;

    let relay_tap = Arc::new(RelayEnvelopeTap::default());
    let mut relays = Vec::with_capacity(4);
    for _ in 0..4 {
        let obs: Arc<dyn BackendObserver> = Arc::clone(&relay_tap) as Arc<dyn BackendObserver>;
        relays.push(PhaseBNode::spawn(OnionRole::Relay, None, None, Some(obs)).await?);
    }
    let exit_node = PhaseBNode::spawn(
        OnionRole::Exit(exit_offer),
        None,
        Some(OnionUdpBindConfig::loopback_for_test()),
        None,
    )
    .await?;

    // Connect full mesh and publish DHT descriptors (`online_nodes`, `onion_exits`, `onion_invokes`, `turn_services`).
    let all_nodes = std::iter::once(&client_node)
        .chain(std::iter::once(&entry_node))
        .chain(relays.iter())
        .chain(std::iter::once(&exit_node))
        .collect::<Vec<_>>();
    for i in 0..all_nodes.len() {
        for j in (i + 1)..all_nodes.len() {
            signal_edge(&all_nodes[i].processor, &all_nodes[j].processor).await?;
        }
    }
    with_hang_guard(
        "await DHT convergence of online_nodes, onion_invokes, onion_exits, and turn_services",
        Duration::from_secs(30),
        async {
            let mut last_publish: Option<Instant> = None;
            loop {
                if last_publish.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
                    let mut set = tokio::task::JoinSet::new();
                    let en = Arc::clone(&entry_node.processor);
                    set.spawn(async move { en.register_all_once().await });
                    let ex = Arc::clone(&exit_node.processor);
                    set.spawn(async move { ex.register_all_once().await });
                    for r in &relays {
                        let rp = Arc::clone(&r.processor);
                        set.spawn(async move { rp.register_all_once().await });
                    }
                    while set.join_next().await.is_some() {}
                    last_publish = Some(Instant::now());
                }
                let c_online = client_node
                    .provider
                    .lookup_registry_raw("online_nodes")
                    .await
                    .map(|r| r.elements.len())
                    .unwrap_or(0);
                let e_online = entry_node
                    .provider
                    .lookup_registry_raw("online_nodes")
                    .await
                    .map(|r| r.elements.len())
                    .unwrap_or(0);
                let c_invokes = client_node
                    .provider
                    .lookup_registry_raw("onion_invokes")
                    .await
                    .map(|r| r.elements.len())
                    .unwrap_or(0);
                let e_exits = entry_node
                    .provider
                    .lookup_registry_raw("onion_exits")
                    .await
                    .map(|r| r.elements.len())
                    .unwrap_or(0);
                let c_turn = client_node
                    .provider
                    .lookup_registry_raw("turn_services")
                    .await
                    .map(|r| r.elements.len())
                    .unwrap_or(0);
                if c_online >= 6 && e_online >= 6 && c_invokes >= 1 && e_exits >= 1 && c_turn >= 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        },
    )
    .await;

    // 2. Acquire an unlinkable `tkt-…` session ticket over the Sphinx `invoke` loop.
    let acquired = with_hang_guard(
        "acquire turn ticket for multi-hop TURN",
        Duration::from_secs(15),
        request_turn_ticket(&client_node.provider, Duration::from_secs(60), None),
    )
    .await?;
    let parsed_ice: rings_transport::ice_server::IceServer = acquired
        .ice_url
        .parse()
        .map_err(|e| Error::InvalidConfig(format!("invalid ICE URL: {e}")))?;
    assert!(parsed_ice.username.starts_with("tkt-"));

    // 3. Connect an unmodified `turn::client::Client` to `entry_listen_addr` using the ticket.
    let client_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let turn_client = TurnClient::new(TurnClientConfig {
        stun_serv_addr: entry_listen_addr.to_string(),
        turn_serv_addr: entry_listen_addr.to_string(),
        username: parsed_ice.username,
        password: parsed_ice.credential,
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 250,
        conn: client_udp,
        vnet: None,
    })
    .await
    .unwrap();
    turn_client.listen().await.unwrap();

    let alloc_t0 = Instant::now();
    let relayed_conn = with_hang_guard(
        "multi-hop TURN Allocate over onion::udp",
        Duration::from_secs(15),
        turn_client.allocate(),
    )
    .await
    .unwrap();
    let allocate_ms = alloc_t0.elapsed().as_secs_f64() * 1_000.0;
    let exit_relayed_addr = relayed_conn.local_addr().unwrap();

    // Law 2: XOR-RELAYED-ADDRESS must be the exit's bound loopback UDP socket (`E_exit`), distinct
    // from the entry TURN listener port and non-unspecified!
    assert_eq!(exit_relayed_addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_ne!(exit_relayed_addr.port(), 0);
    assert_ne!(exit_relayed_addr, entry_listen_addr);

    // 4. Law 3: Unpermitted external UDP peer sending to `E_exit` before `CreatePermission` is
    //    dropped at the exit pinhole gate and never reaches the TURN client.
    let rogue_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    rogue_peer
        .send_to(b"unpermitted-probe-packet", exit_relayed_addr)
        .await
        .unwrap();
    let mut rx_buf = vec![0_u8; 2048];
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            relayed_conn.recv_from(&mut rx_buf)
        )
        .await
        .is_err(),
        "unpermitted peer datagram must not reach the TURN client"
    );

    // 5. Bind raw OS UDP peer (`127.0.0.1:0`) and start recording intermediate `relay` hop wire cells.
    let raw_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let raw_peer_addr = raw_peer.local_addr().unwrap();
    let mut peer_endpoint_bytes = [0_u8; 6];
    if let IpAddr::V4(v4) = raw_peer_addr.ip() {
        peer_endpoint_bytes[..4].copy_from_slice(&v4.octets());
    }
    peer_endpoint_bytes[4..6].copy_from_slice(&raw_peer_addr.port().to_be_bytes());

    relay_tap.start_recording();

    // Build a real RFC 5389 STUN Binding Request with distinctive non-zero 96-bit TransactionId.
    let stun_tx_id = TransactionId([
        0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE, 0x12, 0x34, 0x56, 0x78,
    ]);
    let mut stun_req = Message::new();
    stun_req
        .build(&[
            Box::new(Message {
                transaction_id: stun_tx_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_BINDING, CLASS_REQUEST)),
            Box::new(FINGERPRINT),
        ])
        .unwrap();

    // First send triggers `CreatePermission` + `Send` inside `turn::client::Client` -> `AllowPeerIp`
    // + `SendDatagram` over the 5-hop `onion::udp` loop -> `raw_peer` receives from `E_exit`!
    let rtt_t0 = Instant::now();
    relayed_conn
        .send_to(&stun_req.raw, raw_peer_addr)
        .await
        .unwrap();

    let mut peer_buf = vec![0_u8; 2048];
    let (pn, observed_by_peer) = with_hang_guard(
        "raw UDP peer receives STUN Binding Request from E_exit",
        Duration::from_secs(10),
        raw_peer.recv_from(&mut peer_buf),
    )
    .await
    .unwrap();
    assert_eq!(
        observed_by_peer, exit_relayed_addr,
        "remote UDP peer must observe exit's E_exit, never entry's address"
    );
    let mut decoded_stun_at_peer = Message {
        raw: peer_buf[..pn].to_vec(),
        ..Default::default()
    };
    decoded_stun_at_peer.decode().unwrap();
    assert_eq!(decoded_stun_at_peer.transaction_id, stun_tx_id);

    // Raw peer replies with a real STUN Binding Success Response (`XorMappedAddress = E_exit`).
    let mut stun_resp = Message::new();
    stun_resp
        .build(&[
            Box::new(Message {
                transaction_id: stun_tx_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_BINDING, CLASS_SUCCESS_RESPONSE)),
            Box::new(XorMappedAddress {
                ip: observed_by_peer.ip(),
                port: observed_by_peer.port(),
            }),
            Box::new(FINGERPRINT),
        ])
        .unwrap();
    raw_peer
        .send_to(&stun_resp.raw, observed_by_peer)
        .await
        .unwrap();

    let (cn, from_peer_at_client) = with_hang_guard(
        "TURN client receives STUN Binding Response via onion::udp",
        Duration::from_secs(10),
        relayed_conn.recv_from(&mut rx_buf),
    )
    .await
    .unwrap();
    let first_stun_rtt_ms = rtt_t0.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(from_peer_at_client, raw_peer_addr);
    let mut decoded_stun_at_client = Message {
        raw: rx_buf[..cn].to_vec(),
        ..Default::default()
    };
    decoded_stun_at_client.decode().unwrap();
    assert_eq!(decoded_stun_at_client.typ.class, CLASS_SUCCESS_RESPONSE);
    assert_eq!(decoded_stun_at_client.transaction_id, stun_tx_id);
    let mut xma = XorMappedAddress::default();
    xma.get_from(&decoded_stun_at_client).unwrap();
    assert_eq!(SocketAddr::new(xma.ip, xma.port), exit_relayed_addr);

    // Exchange distinct variable-length application datagrams (`31 B` and `1024 B`) to verify
    // constant cell width and measure steady-state RTT + 8-datagram burst HOL completion.
    let outbound_marker = b"phase-b-outbound-alpha-9f8e7d6c";
    let inbound_marker = b"phase-b-inbound-omega-1a2b3c4d!";
    let steady_t0 = Instant::now();
    relayed_conn
        .send_to(outbound_marker, raw_peer_addr)
        .await
        .unwrap();
    let (an, _) = raw_peer.recv_from(&mut peer_buf).await.unwrap();
    assert_eq!(&peer_buf[..an], outbound_marker);
    raw_peer
        .send_to(inbound_marker, exit_relayed_addr)
        .await
        .unwrap();
    let (bn, _) = relayed_conn.recv_from(&mut rx_buf).await.unwrap();
    let steady_rtt_ms = steady_t0.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(&rx_buf[..bn], inbound_marker);

    // 8-datagram 1 KiB burst to measure head-of-line (HOL) / ordered-session burst completion.
    let burst_count = 8_usize;
    let burst_t0 = Instant::now();
    for seq in 0..burst_count {
        let mut pkt = vec![0x5A_u8; 1_024];
        pkt[0] = u8::try_from(seq).unwrap();
        relayed_conn.send_to(&pkt, raw_peer_addr).await.unwrap();
    }
    for seq in 0..burst_count {
        let (len, src) = with_hang_guard(
            "receive 1KiB burst datagram at peer",
            Duration::from_secs(10),
            raw_peer.recv_from(&mut peer_buf),
        )
        .await
        .unwrap();
        assert_eq!(src, exit_relayed_addr);
        assert_eq!(len, 1_024);
        assert_eq!(peer_buf[0], u8::try_from(seq).unwrap());
        raw_peer
            .send_to(&peer_buf[..len], exit_relayed_addr)
            .await
            .unwrap();
    }
    for seq in 0..burst_count {
        let (len, src) = with_hang_guard(
            "receive 1KiB burst echo at client",
            Duration::from_secs(10),
            relayed_conn.recv_from(&mut rx_buf),
        )
        .await
        .unwrap();
        assert_eq!(src, raw_peer_addr);
        assert_eq!(len, 1_024);
        assert_eq!(rx_buf[0], u8::try_from(seq).unwrap());
    }
    let burst_hol_ms = burst_t0.elapsed().as_secs_f64() * 1_000.0;

    // 6. Law 5 (Middle-hop blindness): inspect every `onion_circuit` cell observed on the
    //    intermediate `relay` hops during the STUN + application datagram + 1 KiB burst exchange!
    let captured_cells = relay_tap.take_cells();
    assert!(
        captured_cells.len() >= 8,
        "expected intermediate relay hops to observe >= 8 onion cells, got {}",
        captured_cells.len()
    );
    let expected_cell_width = OnionLoopClass::DEFAULT.cell_bytes();
    for (idx, cell) in captured_cells.iter().enumerate() {
        assert_eq!(
            cell.len(),
            expected_cell_width,
            "Law 5 violated at cell #{idx}: non-constant cell width {} != {expected_cell_width}",
            cell.len()
        );
        assert!(
            !contains_subslice(cell, &STUN_MAGIC_COOKIE_BYTES),
            "Law 5 violated at cell #{idx}: STUN magic cookie 0x2112A442 leaked in wire cell"
        );
        assert!(
            !contains_subslice(cell, &stun_tx_id.0),
            "Law 5 violated at cell #{idx}: 96-bit STUN TransactionId leaked in wire cell"
        );
        assert!(
            !contains_subslice(cell, outbound_marker),
            "Law 5 violated at cell #{idx}: outbound application payload leaked in wire cell"
        );
        assert!(
            !contains_subslice(cell, inbound_marker),
            "Law 5 violated at cell #{idx}: inbound application payload leaked in wire cell"
        );
        assert!(
            !contains_subslice(cell, &peer_endpoint_bytes),
            "Law 5 violated at cell #{idx}: peer (IPv4, port) bytes leaked in wire cell"
        );
    }

    // 7. Law 4 (Bounded teardown & OS socket release on remote exit):
    //    `turn::client::RelayConn::close()` sends `Refresh(Lifetime=0)` with `dont_wait = true`,
    //    so await the entry server processing that UDP packet and verify bounded exit socket release.
    let close_t0 = Instant::now();
    relayed_conn.close().await.unwrap();
    turn_client.close().await.unwrap();
    with_hang_guard(
        "verify entry TurnServer allocation table empties after Refresh(Lifetime=0)",
        Duration::from_secs(2),
        async {
            while !turn_server.allocations_info(None).await.unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
    )
    .await;

    // Prove at the OS kernel level that the remote exit node dropped its `UdpSocket` bound to `E_exit`:
    let exit_port_rebound = with_hang_guard(
        "verify remote exit released bound UDP socket E_exit",
        Duration::from_secs(3),
        async {
            loop {
                if let Ok(rebound) = UdpSocket::bind(exit_relayed_addr).await {
                    drop(rebound);
                    break close_t0.elapsed().as_secs_f64() * 1_000.0;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        },
    )
    .await;

    println!(
        "[phase-b-metrics] cells_inspected={} cell_bytes={} allocate_ms={allocate_ms:.2} first_stun_rtt_ms={first_stun_rtt_ms:.2} steady_rtt_ms={steady_rtt_ms:.2} burst_8x1kib_ms={burst_hol_ms:.2} exit_socket_release_ms={exit_port_rebound:.2}",
        captured_cells.len(),
        expected_cell_width,
    );

    turn_server.close().await.unwrap();
    client_node.shutdown().await?;
    entry_node.shutdown().await?;
    exit_node.shutdown().await?;
    for r in relays {
        r.shutdown().await?;
    }
    Ok(())
}
