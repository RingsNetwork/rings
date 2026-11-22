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
use rings_core::lifecycle::StopSource;
use rings_core::swarm::OfferReply;
use rings_core::utils::ManualClock;
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
use stun::attributes::ATTR_NONCE;
use stun::attributes::ATTR_REALM;
use stun::attributes::ATTR_USERNAME;
use stun::fingerprint::FINGERPRINT;
use stun::integrity::MessageIntegrity;
use stun::message::Getter;
use stun::message::Message;
use stun::message::MessageType;
use stun::message::CLASS_ERROR_RESPONSE;
use stun::message::CLASS_INDICATION;
use stun::message::CLASS_REQUEST;
use stun::message::CLASS_SUCCESS_RESPONSE;
use stun::message::METHOD_ALLOCATE;
use stun::message::METHOD_BINDING;
use stun::message::METHOD_CREATE_PERMISSION;
use stun::message::METHOD_DATA;
use stun::message::METHOD_REFRESH;
use stun::message::METHOD_SEND;
use stun::textattrs::Nonce;
use stun::textattrs::Realm;
use stun::textattrs::Username;
use stun::xoraddr::XorMappedAddress;
use tokio::net::UdpSocket;
use turn::auth::generate_auth_key;
use turn::client::Client as TurnClient;
use turn::client::ClientConfig as TurnClientConfig;
use turn::proto::data::Data;
use turn::proto::lifetime::Lifetime;
use turn::proto::peeraddr::PeerAddress;
use turn::proto::relayaddr::RelayedAddress;
use turn::proto::reqtrans::RequestedTransport;
use turn::proto::PROTO_UDP;
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
    stop: StopSource,
    listen_task: tokio::task::JoinHandle<Result<()>>,
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
        let config = ProcessorConfig::new(1, String::new(), key, 1).onion_role(role);
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
        let stop = StopSource::new();
        let stop_token = stop.token();
        let listen_provider = Arc::clone(&provider);
        let listen_task =
            tokio::spawn(async move { listen_provider.listen_with(stop_token).await });
        Ok(Self {
            processor,
            provider,
            stop,
            listen_task,
        })
    }

    async fn shutdown(self) -> Result<()> {
        self.stop.request_stop();
        let _ = self.listen_task.await;
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

async fn send_authenticated_create_permission(
    sock: &UdpSocket,
    entry_listen_addr: SocketAddr,
    peer_addr: SocketAddr,
    username: &str,
    realm: &str,
    nonce: &str,
    mi_key: &[u8],
) {
    let mut perm_msg = Message::new();
    perm_msg
        .build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
            Box::new(PeerAddress {
                ip: peer_addr.ip(),
                port: peer_addr.port(),
            }),
            Box::new(Username::new(ATTR_USERNAME, username.to_string())),
            Box::new(Realm::new(ATTR_REALM, realm.to_string())),
            Box::new(Nonce::new(ATTR_NONCE, nonce.to_string())),
            Box::new(MessageIntegrity(mi_key.to_vec())),
            Box::new(FINGERPRINT),
        ])
        .unwrap();
    sock.send_to(&perm_msg.raw, entry_listen_addr)
        .await
        .unwrap();
    let mut resp_buf = vec![0_u8; 512];
    let (resp_len, _) = with_hang_guard(
        "receive 200 CreatePermission response",
        Duration::from_secs(10),
        sock.recv_from(&mut resp_buf),
    )
    .await
    .unwrap();
    let mut resp = Message {
        raw: resp_buf[..resp_len].to_vec(),
        ..Default::default()
    };
    resp.decode().unwrap();
    assert_eq!(resp.typ.class, CLASS_SUCCESS_RESPONSE);
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

    let turn_clock = Arc::new(ManualClock::new(rings_core::utils::get_epoch_ms()));
    let deferred_factory = OnionUdpRelayFactory::deferred(
        OnionLoopClass::DEFAULT,
        RelayPeerPolicy::AllowLocalAndPrivate,
    );
    let mut turn_cfg = TurnServerConfig::loopback_ephemeral();
    turn_cfg.clock = turn_clock.clone();
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
            loop {
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
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        },
    )
    .await;

    // 2. Acquire an unlinkable `tkt-…` session ticket over the Sphinx `invoke` loop.
    turn_clock.set(rings_core::utils::get_epoch_ms());
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
    let (peer_len, observed_by_peer) = with_hang_guard(
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
        raw: peer_buf[..peer_len].to_vec(),
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

    // 6. Law 5 (Middle-hop blindness): inspect every `onion-circuit` cell observed on the
    //    intermediate `relay` hops during the STUN + application datagram + 1 KiB burst exchange!
    //    Note: a bare 4-byte pattern across ~20 MiB of uniform AEZ ciphertext has ~0.5% random
    //    collision probability (`N / 2^32`), so we assert over the 16-byte STUN header slice
    //    (`magic_cookie || transaction_id`, `2^-128`), the 12-byte `transaction_id` (`2^-96`),
    //    the 31-byte markers (`2^-248`), and the 6-byte `(IPv4, port)` tuple (`2^-48`).
    let mut stun_cookie_and_tx_id = [0_u8; 16];
    stun_cookie_and_tx_id[..4].copy_from_slice(&STUN_MAGIC_COOKIE_BYTES);
    stun_cookie_and_tx_id[4..16].copy_from_slice(&stun_tx_id.0);
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
            !contains_subslice(cell, &stun_cookie_and_tx_id),
            "Law 5 violated at cell #{idx}: STUN cookie+TransactionId header leaked in wire cell"
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

    // 8. Multi-hop `Refresh` cancel-safety, permission expiry/renewal, and allocation expiry:
    //    Exercises a live `Refresh(Lifetime=600)` while `relay_packet_handler` is parked in
    //    `tokio::select! { changed = expiry_rx.changed(), recv_res = alloc.recv_payload_from_peer() }`,
    //    verifying inbound traffic survives `Refresh`, stops on 5-minute permission expiry, resumes
    //    after `CreatePermission` renewal, and releases the remote exit socket on allocation expiry.
    turn_clock.set(rings_core::utils::get_epoch_ms());
    let acquired_2 = with_hang_guard(
        "acquire second turn ticket for Refresh/expiry regression",
        Duration::from_secs(15),
        request_turn_ticket(&client_node.provider, Duration::from_secs(60), None),
    )
    .await?;
    let parsed_ice_2: rings_transport::ice_server::IceServer = acquired_2
        .ice_url
        .parse()
        .map_err(|e| Error::InvalidConfig(format!("invalid ICE URL #2: {e}")))?;
    let raw_turn_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut ctrl_buf = vec![0_u8; 2048];

    // 8a. Obtain realm + nonce challenge, then `Allocate` over the 5-hop `onion::udp` loop.
    let mut unauth_alloc = Message::new();
    unauth_alloc
        .build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
            Box::new(RequestedTransport {
                protocol: PROTO_UDP,
            }),
        ])
        .unwrap();
    raw_turn_sock
        .send_to(&unauth_alloc.raw, entry_listen_addr)
        .await
        .unwrap();
    let (chal_len, _) = raw_turn_sock.recv_from(&mut ctrl_buf).await.unwrap();
    let mut chal_msg = Message {
        raw: ctrl_buf[..chal_len].to_vec(),
        ..Default::default()
    };
    chal_msg.decode().unwrap();
    assert_eq!(chal_msg.typ.class, CLASS_ERROR_RESPONSE);
    let mut nonce_attr = Nonce::new(ATTR_NONCE, String::new());
    nonce_attr.get_from(&chal_msg).unwrap();
    let mut realm_attr = Realm::new(ATTR_REALM, String::new());
    realm_attr.get_from(&chal_msg).unwrap();
    let mi_key = generate_auth_key(
        &parsed_ice_2.username,
        &realm_attr.text,
        &parsed_ice_2.credential,
    );

    let mut auth_alloc = Message::new();
    auth_alloc
        .build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
            Box::new(RequestedTransport {
                protocol: PROTO_UDP,
            }),
            Box::new(Lifetime(Duration::from_secs(600))),
            Box::new(Username::new(ATTR_USERNAME, parsed_ice_2.username.clone())),
            Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
            Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
            Box::new(MessageIntegrity(mi_key.clone())),
            Box::new(FINGERPRINT),
        ])
        .unwrap();
    raw_turn_sock
        .send_to(&auth_alloc.raw, entry_listen_addr)
        .await
        .unwrap();
    let (alloc2_len, _) = with_hang_guard(
        "receive 200 Allocate Success #2",
        Duration::from_secs(15),
        raw_turn_sock.recv_from(&mut ctrl_buf),
    )
    .await
    .unwrap();
    let mut alloc2_resp = Message {
        raw: ctrl_buf[..alloc2_len].to_vec(),
        ..Default::default()
    };
    alloc2_resp.decode().unwrap();
    assert_eq!(alloc2_resp.typ.class, CLASS_SUCCESS_RESPONSE);
    let mut relayed_attr_2 = RelayedAddress::default();
    relayed_attr_2.get_from(&alloc2_resp).unwrap();
    let exit_relayed_addr_2 = SocketAddr::new(relayed_attr_2.ip, relayed_attr_2.port);

    // 8b. Install initial `CreatePermission(raw_peer_addr)` and send one outbound `SendIndication`
    //     barrier so `AllowPeerIp` (which precedes `SendDatagram` in the ordered 5-hop session
    //     stream) is confirmed committed at `exit_node` before `raw_peer` transmits inbound UDP.
    send_authenticated_create_permission(
        &raw_turn_sock,
        entry_listen_addr,
        raw_peer_addr,
        &parsed_ice_2.username,
        &realm_attr.text,
        &nonce_attr.text,
        &mi_key,
    )
    .await;
    let mut barrier_send = Message::new();
    barrier_send
        .build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
            Box::new(PeerAddress {
                ip: raw_peer_addr.ip(),
                port: raw_peer_addr.port(),
            }),
            Box::new(Data(b"sync-pinhole-barrier".to_vec())),
            Box::new(FINGERPRINT),
        ])
        .unwrap();
    raw_turn_sock
        .send_to(&barrier_send.raw, entry_listen_addr)
        .await
        .unwrap();
    let (bar_len, bar_src) = with_hang_guard(
        "raw UDP peer receives pinhole-sync barrier from E_exit_2",
        Duration::from_secs(10),
        raw_peer.recv_from(&mut peer_buf),
    )
    .await
    .unwrap();
    assert_eq!(bar_src, exit_relayed_addr_2);
    assert_eq!(&peer_buf[..bar_len], b"sync-pinhole-barrier");

    // 8c. Issue a live `Refresh(Lifetime=600)` while `relay_packet_handler` is parked awaiting
    //     `NativeOnionUdpSession::recv_from()`, then verify inbound UDP datagrams still arrive!
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut refresh_msg = Message::new();
    refresh_msg
        .build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
            Box::new(Lifetime(Duration::from_secs(600))),
            Box::new(Username::new(ATTR_USERNAME, parsed_ice_2.username.clone())),
            Box::new(Realm::new(ATTR_REALM, realm_attr.text.clone())),
            Box::new(Nonce::new(ATTR_NONCE, nonce_attr.text.clone())),
            Box::new(MessageIntegrity(mi_key.clone())),
            Box::new(FINGERPRINT),
        ])
        .unwrap();
    raw_turn_sock
        .send_to(&refresh_msg.raw, entry_listen_addr)
        .await
        .unwrap();
    let (ref_len, _) = with_hang_guard(
        "receive 200 Refresh Success",
        Duration::from_secs(10),
        raw_turn_sock.recv_from(&mut ctrl_buf),
    )
    .await
    .unwrap();
    let mut ref_resp = Message {
        raw: ctrl_buf[..ref_len].to_vec(),
        ..Default::default()
    };
    ref_resp.decode().unwrap();
    assert_eq!(ref_resp.typ.class, CLASS_SUCCESS_RESPONSE);

    raw_peer
        .send_to(b"post-refresh-inbound-ok", exit_relayed_addr_2)
        .await
        .unwrap();
    let (ind1_len, _) = with_hang_guard(
        "receive DataIndication after live Refresh",
        Duration::from_secs(10),
        raw_turn_sock.recv_from(&mut ctrl_buf),
    )
    .await
    .unwrap();
    let mut ind1 = Message {
        raw: ctrl_buf[..ind1_len].to_vec(),
        ..Default::default()
    };
    ind1.decode().unwrap();
    assert_eq!(ind1.typ.class, CLASS_INDICATION);
    assert_eq!(ind1.typ.method, METHOD_DATA);
    let mut ind1_data = Data::default();
    ind1_data.get_from(&ind1).unwrap();
    assert_eq!(ind1_data.0.as_slice(), b"post-refresh-inbound-ok");

    // 8d. Advance `turn_clock` by 301s: the 5-minute permission expires while the 600s allocation
    //     stays live, so inbound datagrams are dropped until `CreatePermission` renews it.
    //     Wait longer than one full 5-hop return transit (`steady_rtt_ms + 800 ms`) so the
    //     in-flight return cell reaches `entry_node` and is dropped before we renew the permission.
    turn_clock.advance(301_000);
    raw_peer
        .send_to(b"dropped-after-perm-expiry", exit_relayed_addr_2)
        .await
        .unwrap();
    let expiry_drain_wait =
        Duration::from_secs_f64((steady_rtt_ms / 1_000.0 + 0.8).clamp(1.5, 4.0));
    assert!(
        tokio::time::timeout(expiry_drain_wait, raw_turn_sock.recv_from(&mut ctrl_buf))
            .await
            .is_err(),
        "inbound datagram after 300s permission expiry must be dropped"
    );

    // Renew permission via `CreatePermission` and verify inbound delivery resumes immediately.
    send_authenticated_create_permission(
        &raw_turn_sock,
        entry_listen_addr,
        raw_peer_addr,
        &parsed_ice_2.username,
        &realm_attr.text,
        &nonce_attr.text,
        &mi_key,
    )
    .await;
    raw_peer
        .send_to(b"post-perm-renewal-ok", exit_relayed_addr_2)
        .await
        .unwrap();
    let (ind2_len, _) = with_hang_guard(
        "receive DataIndication after CreatePermission renewal",
        Duration::from_secs(10),
        raw_turn_sock.recv_from(&mut ctrl_buf),
    )
    .await
    .unwrap();
    let mut ind2 = Message {
        raw: ctrl_buf[..ind2_len].to_vec(),
        ..Default::default()
    };
    ind2.decode().unwrap();
    let mut ind2_data = Data::default();
    ind2_data.get_from(&ind2).unwrap();
    assert_eq!(ind2_data.0.as_slice(), b"post-perm-renewal-ok");

    // 8e. Advance `turn_clock` past the 600s allocation lifetime: entry prunes the expired
    //     allocation and the remote exit releases `E_exit_2`.
    turn_clock.advance(305_000);
    assert!(
        turn_server.allocations_info(None).await.unwrap().is_empty(),
        "expired multi-hop allocation must be pruned and closed"
    );
    with_hang_guard(
        "verify remote exit released E_exit_2 after allocation expiry",
        Duration::from_secs(3),
        async {
            loop {
                if let Ok(rebound) = UdpSocket::bind(exit_relayed_addr_2).await {
                    drop(rebound);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        },
    )
    .await;

    turn_server.close().await.unwrap();
    client_node.shutdown().await?;
    entry_node.shutdown().await?;
    exit_node.shutdown().await?;
    for r in relays {
        r.shutdown().await?;
    }
    Ok(())
}
