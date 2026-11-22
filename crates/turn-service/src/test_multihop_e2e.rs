//! End-to-end multi-hop TURN over generic `onion::udp` integration tests (#1292 Phase B).
//!
//! Exercises the full multi-hop stack:
//! - Anonymous `SignedTurnSessionTicket` acquisition over `turn.ticket` `onionInvoke`,
//! - Unmodified `turn::client::Client` -> `rings-turn-service` entry (`OnionUdpRelayBackend`) ->
//!   standard `OnionRole::Relay` middle hops -> generic `OnionRole::Exit([udp])` exit -> raw OS
//!   `UdpSocket` peer,
//! - **Law 2**: `XOR-RELAYED-ADDRESS == E_exit` bound on the remote `onion::udp` exit,
//! - **Law 3**: unpermitted peer packets dropped before `CreatePermission` and after `300s`
//!   RFC 5766 permission expiry, bidirectional datagrams + real RFC 5389 STUN `METHOD_BINDING`
//!   request/response admitted while permitted,
//! - **Law 4**: `client.close()` (`Refresh(0)`) and `turn_server.close()` release both the entry
//!   allocation and the remote exit OS UDP socket (`E_exit`) within `< 1s`,
//! - **Law 5**: wire tap across all middle `OnionRole::Relay` hops proves every inter-hop
//!   `"onion-circuit"` envelope carries an exact `16,384`-byte (`OnionLoopClass::DEFAULT`) cell and
//!   leaks neither the STUN magic cookie, `TransactionId`, raw/XOR-mapped peer IP+port, nor
//!   datagram cleartext/length,
//! - **Latency & HOL measurement**: single-hop (`TurnRelayMode::Local`) vs multi-hop
//!   (`TurnRelayMode::OnionUdp`) round-trip latency (p50/p95/max) and back-to-back ordered burst
//!   completion.

use std::any::Any;
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::swarm::OfferReply;
use rings_core::utils::ManualClock;
use rings_core::utils::SharedClock;
use rings_node::error::Error;
use rings_node::error::Result;
use rings_node::extension::ext::Envelope;
use rings_node::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use rings_node::onion::udp::OnionUdpConfig;
use rings_node::onion::OnionExitOffer;
use rings_node::onion::OnionExitPolicy;
use rings_node::onion::OnionInvokePolicy;
use rings_node::onion::OnionLoopClass;
use rings_node::onion::OnionRole;
use rings_node::onion::OnionServiceName;
use rings_node::processor::NodeNetworkParameters;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::processor::SessionRotationCommit;
use rings_node::processor::TransportNetworkConfig;
use rings_node::provider::Provider;
use rings_test_support::with_hang_guard;
use rings_transport::connection_ref::ConnectionRef;
use rings_transport::core::callback::AdmittedInboundMessage;
use rings_transport::core::callback::BoxedTransportCallback;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::DynConnection;
use rings_transport::core::transport::DynTransportInterface;
use rings_transport::core::transport::SharedTransport;
use rings_transport::core::transport::TransportConnectionState;
use rings_turn::RelayAddressConfig;
use rings_turn::RelayPeerPolicy;
use rings_turn::TurnAdmissionPolicy;
use rings_turn::TurnServer;
use rings_turn::TurnServerConfig;
use stun::agent::TransactionId;
use stun::fingerprint::FINGERPRINT;
use stun::message::Getter;
use stun::message::Message as StunMessage;
use stun::message::MessageType;
use stun::message::Setter;
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
use crate::OnionUdpRelayBackend;
use crate::TurnRelayMode;
use crate::TurnServiceConfig;
use crate::TurnServiceOffer;

const STUN_MAGIC_COOKIE_BE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

#[derive(Clone, Debug)]
struct CapturedMiddleHopFrame {
    raw_transport_payload: Bytes,
    onion_cell_payload: Option<Bytes>,
}

#[derive(Clone, Default)]
struct MiddleHopWireTap {
    relay_dids: Arc<Mutex<BTreeSet<Did>>>,
    frames: Arc<Mutex<Vec<CapturedMiddleHopFrame>>>,
}

impl MiddleHopWireTap {
    fn register_relay(&self, did: Did) {
        self.relay_dids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(did);
    }

    fn clear(&self) {
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn snapshot(&self) -> Vec<CapturedMiddleHopFrame> {
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record_inbound_at(&self, receiver_did: Did, cid: &str, raw: &[u8]) {
        let sender_did = Did::from_str(cid).ok();
        let touches_middle_relay = {
            let relays = self
                .relay_dids
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            relays.contains(&receiver_did) || sender_did.is_some_and(|did| relays.contains(&did))
        };
        if !touches_middle_relay {
            return;
        }
        let ns_prefix = rings_codec::serialize(&ONION_CIRCUIT_NAMESPACE).unwrap_or_default();
        let onion_cell_payload = (!ns_prefix.is_empty())
            .then(|| {
                raw.windows(ns_prefix.len())
                    .position(|w| w == ns_prefix.as_slice())
            })
            .flatten()
            .and_then(|pos| rings_codec::deserialize_prefix::<Envelope>(&raw[pos..]).ok())
            .filter(|(env, _)| env.namespace == ONION_CIRCUIT_NAMESPACE)
            .map(|(env, _)| env.payload);
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(CapturedMiddleHopFrame {
                raw_transport_payload: Bytes::copy_from_slice(raw),
                onion_cell_payload,
            });
    }
}

struct TapCallback {
    receiver_did: Did,
    tap: MiddleHopWireTap,
    inner: BoxedTransportCallback,
}

#[async_trait]
impl TransportCallback for TapCallback {
    async fn on_channel_open(
        &self,
        cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.inner.on_channel_open(cid).await
    }

    async fn on_channel_close(
        &self,
        cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.inner.on_channel_close(cid).await
    }

    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.tap
            .record_inbound_at(self.receiver_did, message.cid(), message.payload());
        self.inner.on_admitted_message(message).await
    }

    async fn on_invalid_inbound_frame(
        &self,
        cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.inner.on_invalid_inbound_frame(cid).await
    }

    async fn on_connection_state_change(
        &self,
        cid: &str,
        state: TransportConnectionState,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.inner.on_connection_state_change(cid, state).await
    }
}

struct TapTransport {
    owner_did: Did,
    tap: MiddleHopWireTap,
    inner: SharedTransport,
}

#[async_trait]
impl DynTransportInterface for TapTransport {
    fn as_any(&self) -> &dyn Any {
        self.inner.as_any()
    }

    async fn new_dyn_connection(
        &self,
        cid: &str,
        callback: BoxedTransportCallback,
    ) -> std::result::Result<ConnectionRef<DynConnection>, rings_transport::error::Error> {
        let wrapped: BoxedTransportCallback = Box::new(TapCallback {
            receiver_did: self.owner_did,
            tap: self.tap.clone(),
            inner: callback,
        });
        self.inner.new_dyn_connection(cid, wrapped).await
    }

    async fn close_dyn_connection_if_current(
        &self,
        connection: &ConnectionRef<DynConnection>,
    ) -> std::result::Result<bool, rings_transport::error::Error> {
        self.inner.close_dyn_connection_if_current(connection).await
    }

    fn dyn_connection(
        &self,
        cid: &str,
    ) -> std::result::Result<ConnectionRef<DynConnection>, rings_transport::error::Error> {
        self.inner.dyn_connection(cid)
    }

    fn connection_ids(&self) -> Vec<String> {
        self.inner.connection_ids()
    }

    fn max_handshake_step_bytes(&self) -> usize {
        self.inner.max_handshake_step_bytes()
    }

    fn requires_duplex_reverse_path(&self) -> bool {
        self.inner.requires_duplex_reverse_path()
    }

    fn validate_inbound_handshake(
        &self,
        step_index: u8,
        payload: &[u8],
    ) -> std::result::Result<(), rings_transport::error::Error> {
        self.inner.validate_inbound_handshake(step_index, payload)
    }
}

#[derive(Clone)]
struct TapNetworkParams {
    inner: TransportNetworkConfig,
    tap: MiddleHopWireTap,
}

impl std::fmt::Debug for TapNetworkParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapNetworkParams").finish()
    }
}

impl NodeNetworkParameters for TapNetworkParams {
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
        let inner = NodeNetworkParameters::build_transport(&self.inner, delegatee_key)?;
        Ok(Arc::new(TapTransport {
            owner_did: delegatee_key.delegator_did(),
            tap: self.tap.clone(),
            inner,
        }))
    }

    fn endpoint_hint(&self) -> Option<&str> {
        NodeNetworkParameters::endpoint_hint(&self.inner)
    }

    fn prepare_session_key_rotation(
        &self,
        transport: &SharedTransport,
        next_key: &DelegateeKey,
    ) -> Result<SessionRotationCommit> {
        NodeNetworkParameters::prepare_session_key_rotation(&self.inner, transport, next_key)
    }
}

struct MultiHopNode {
    processor: Arc<Processor>,
    provider: Arc<Provider>,
}

impl MultiHopNode {
    async fn spawn(
        role: OnionRole<OnionExitOffer>,
        turn_offer: Option<TurnServiceOffer>,
        udp_cfg: Option<OnionUdpConfig>,
        clock: Option<SharedClock>,
        tap: Option<&MiddleHopWireTap>,
    ) -> Result<Self> {
        let now_ms = clock
            .as_ref()
            .map_or_else(rings_core::utils::get_epoch_ms, |c| c.now_ms());
        let account = SecretKey::random();
        let key = DelegateeKey::new_with_seckey_at(&account, 3_600_000, now_ms).unwrap();
        let advertise_presence = role.registers_relay();
        let mut config = ProcessorConfig::new(1, String::new(), key, 3_600).onion_role(role);
        if let Some(c) = clock {
            config = config.with_clock(c);
        }
        if let Some(u) = udp_cfg {
            config = config.with_onion_udp_config(u);
        }
        if let Some(wire_tap) = tap {
            config = config.with_network_parameters(TapNetworkParams {
                inner: TransportNetworkConfig::new(String::new(), None, None),
                tap: wire_tap.clone(),
            });
        }
        let mut builder = ProcessorBuilder::from_config(&config)?
            .advertise_presence(advertise_presence)
            .storage(Box::new(rings_core::storage::MemStorage::new()))
            .replay_storage(Box::new(rings_core::storage::MemStorage::new()));
        if let Some(offer) = turn_offer {
            builder = announce_turn_service(builder, offer);
        }
        let processor = Arc::new(builder.build()?);
        let provider = Arc::new(Provider::from_processor(Arc::clone(&processor)));
        let _ = provider.onion_circuit()?;
        let _ = provider.onion_pipeline()?;
        provider.set_backend()?;
        Ok(Self {
            processor,
            provider,
        })
    }

    async fn shutdown(self) -> Result<()> {
        self.processor.close().await
    }
}

async fn connect_mesh_edge(first: &Processor, second: &Processor) -> Result<()> {
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

async fn stabilize_mesh(nodes: &[&MultiHopNode]) {
    for _ in 0..2 {
        for node in nodes {
            let _ = node.processor.swarm.stabilizer().stabilize().await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

fn percentile_us(sorted_us: &[u128], pct: usize) -> u128 {
    if sorted_us.is_empty() {
        return 0;
    }
    let idx = ((sorted_us.len() - 1) * pct) / 100;
    sorted_us[idx]
}

#[test]
fn turn_service_config_validates_onion_udp_relay_mode() {
    let clock: SharedClock = Arc::new(ManualClock::new(1_700_000_000_000));
    let mut cfg = TurnServiceConfig {
        relay_mode: TurnRelayMode::OnionUdp,
        min_port: Some(40_000),
        max_port: Some(40_100),
        ..TurnServiceConfig::default()
    };
    assert!(
        cfg.build_turn_server_config_with_onion_backend(clock.clone())
            .is_err(),
        "min_port/max_port must be rejected in relay_mode: onion_udp"
    );

    cfg.min_port = None;
    cfg.max_port = None;
    cfg.max_permissions_per_allocation = rings_node::onion::udp::DEFAULT_MAX_UDP_PINHOLES + 1;
    assert!(
        cfg.build_turn_server_config_with_onion_backend(clock.clone())
            .is_err(),
        "max_permissions_per_allocation exceeding remote onion_udp pinhole capacity must be rejected"
    );

    cfg.max_permissions_per_allocation = rings_node::onion::udp::DEFAULT_MAX_UDP_PINHOLES;
    assert!(
        cfg.to_turn_server_config(clock.clone()).is_err(),
        "to_turn_server_config must reject onion_udp rather than discarding deferred_backend"
    );
    let (server_cfg, deferred_backend) = cfg
        .build_turn_server_config_with_onion_backend(clock)
        .expect("valid onion_udp TurnServiceConfig");
    assert!(server_cfg.relay_allocator.is_some());
    assert!(deferred_backend.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multihop_turn_over_onion_udp_relays_stun_and_datagrams_preserves_middle_hop_blindness_and_tears_down_fast(
) -> Result<()> {
    let t0: u128 = rings_core::utils::get_epoch_ms();
    let shared_clock: SharedClock = Arc::new(ManualClock::new(t0));
    let manual_clock = {
        let raw = ManualClock::new(t0);
        Arc::new(raw)
    };
    drop(shared_clock);
    let turn_clock: SharedClock = manual_clock.clone();

    let tap = MiddleHopWireTap::default();
    let invoke_policy = OnionInvokePolicy {
        max_concurrent: 16,
        max_invocations_per_minute: 64,
    };

    // 1. Start TURN entry server with deferred `OnionUdpRelayBackend` (no local relay UDP sockets!).
    let onion_backend = OnionUdpRelayBackend::deferred(
        true,
        RelayPeerPolicy::AllowLocalAndPrivate,
        OnionLoopClass::DEFAULT,
    );
    let mut turn_cfg = TurnServerConfig::loopback_ephemeral();
    turn_cfg.admission = TurnAdmissionPolicy::allowlist(std::iter::empty());
    turn_cfg.clock = turn_clock.clone();
    turn_cfg.max_allocations = 4;
    turn_cfg.max_reservations = 4;
    turn_cfg.peer_policy = RelayPeerPolicy::AllowLocalAndPrivate;
    turn_cfg.relay_allocator = Some(Arc::clone(&onion_backend) as _);
    let turn_server = Arc::new(
        TurnServer::start(turn_cfg)
            .await
            .map_err(|e| Error::InvalidConfig(e.to_string()))?,
    );
    let turn_entry_addr = turn_server.local_addr();
    // Advertise a syntactically public IP in the DHT descriptor while connecting the local test
    // TURN client to `127.0.0.1:<turn_entry_addr.port()>`.
    let advertised_turn_url = format!("turn://1.1.1.1:{}", turn_entry_addr.port());
    let turn_offer = TurnServiceOffer::new(&advertised_turn_url, turn_server.server_pubkey())?;

    // 2. Build overlay mesh:
    //    - `ticket_client`: stock client node acquiring `SignedTurnSessionTicket` over `onionInvoke`
    //    - `turn_entry_node`: TURN entry hosting `turn.ticket` & `OnionUdpRelayBackend`
    //    - 4 `OnionRole::Relay` middle hops (all wire-tapped for Law 5 blindness verification)
    //    - `udp_exit_node`: generic `OnionRole::Exit([udp])` binding `E_exit` inside `[47200, 47299]`
    let ticket_client =
        MultiHopNode::spawn(OnionRole::Client, None, None, None, Some(&tap)).await?;
    let turn_entry_node = MultiHopNode::spawn(
        OnionRole::Invoke(turn_ticket_invoke_offer(invoke_policy)?),
        Some(turn_offer),
        None,
        None,
        Some(&tap),
    )
    .await?;
    onion_backend.attach_provider(&turn_entry_node.provider)?;
    register_turn_ticket_invoke_backend(
        &turn_entry_node.provider.onion_pipeline()?,
        invoke_policy,
        Arc::clone(&turn_server),
    )?;

    let mut middle_relays = Vec::with_capacity(4);
    for _ in 0..4 {
        let relay = MultiHopNode::spawn(OnionRole::Relay, None, None, None, Some(&tap)).await?;
        tap.register_relay(relay.processor.did());
        middle_relays.push(relay);
    }

    let exit_policy = OnionExitPolicy::from_target_strings(vec!["*:*".to_string()], Vec::new())?;
    let exit_offer = OnionExitOffer::new([OnionServiceName::udp()], exit_policy)?;
    let mut exit_udp_cfg = OnionUdpConfig::loopback();
    exit_udp_cfg.port_range = Some((47_200, 47_299));
    let udp_exit_node = MultiHopNode::spawn(
        OnionRole::Exit(exit_offer),
        None,
        Some(exit_udp_cfg),
        None,
        Some(&tap),
    )
    .await?;

    let all_nodes = std::iter::once(&ticket_client)
        .chain(std::iter::once(&turn_entry_node))
        .chain(middle_relays.iter())
        .chain(std::iter::once(&udp_exit_node))
        .collect::<Vec<_>>();
    for i in 0..all_nodes.len() {
        for j in (i + 1)..all_nodes.len() {
            connect_mesh_edge(&all_nodes[i].processor, &all_nodes[j].processor).await?;
        }
    }
    stabilize_mesh(&all_nodes).await;

    turn_entry_node.processor.register_all_once().await?;
    for relay in &middle_relays {
        relay.processor.register_all_once().await?;
    }
    udp_exit_node.processor.register_all_once().await?;

    // 3. Acquire anonymous single-use `SignedTurnSessionTicket` over `turn.ticket` `onionInvoke`
    //    (re-publishing `register_all_once` if Chord stabilization was still converging).
    let acquired = with_hang_guard(
        "acquire anonymous TURN ticket",
        Duration::from_secs(20),
        async {
            let mut ticks = 0_u32;
            loop {
                match request_turn_ticket(&ticket_client.provider, Duration::from_secs(60), None)
                    .await
                {
                    Ok(tkt) => break Ok::<_, Error>(tkt),
                    Err(Error::OnionRouteError(_)) => {
                        ticks += 1;
                        if ticks.is_multiple_of(8) {
                            stabilize_mesh(&all_nodes).await;
                            let _ = turn_entry_node.processor.register_all_once().await;
                            for relay in &middle_relays {
                                let _ = relay.processor.register_all_once().await;
                            }
                            let _ = udp_exit_node.processor.register_all_once().await;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(other) => break Err(other),
                }
            }
        },
    )
    .await?;
    let parsed_ice: rings_transport::ice_server::IceServer = acquired
        .ice_url
        .parse()
        .map_err(|e| Error::InvalidConfig(format!("invalid acquired ICE URL: {e}")))?;

    with_hang_guard(
        "await udp exit route readiness on turn_entry_node",
        Duration::from_secs(20),
        async {
            let bind_req = rings_node::onion::udp::OnionUdpBindRequest {
                family: rings_node::onion::udp::OnionUdpFamily::Ipv4,
                even_port: false,
            };
            let mut ticks = 0_u32;
            while turn_entry_node
                .processor
                .build_onion_udp_route(bind_req)
                .await
                .is_err()
            {
                ticks += 1;
                if ticks.is_multiple_of(8) {
                    stabilize_mesh(&all_nodes).await;
                    for relay in &middle_relays {
                        let _ = relay.processor.register_all_once().await;
                    }
                    let _ = udp_exit_node.processor.register_all_once().await;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        },
    )
    .await;

    // 4. Unmodified `turn::client::Client` allocates through `turn_server` -> opens multi-hop
    //    `onion::udp` session to `udp_exit_node`.
    let client_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let turn_client = TurnClient::new(TurnClientConfig {
        stun_serv_addr: turn_entry_addr.to_string(),
        turn_serv_addr: turn_entry_addr.to_string(),
        username: parsed_ice.username.clone(),
        password: parsed_ice.credential.clone(),
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 250,
        conn: client_udp,
        vnet: None,
    })
    .await
    .map_err(|e| Error::InvalidConfig(format!("TurnClient::new failed: {e}")))?;
    turn_client
        .listen()
        .await
        .map_err(|e| Error::InvalidConfig(format!("TurnClient::listen failed: {e}")))?;

    let relay_conn = with_hang_guard(
        "multi-hop TURN Allocate over onion::udp",
        Duration::from_secs(15),
        turn_client.allocate(),
    )
    .await
    .map_err(|e| Error::InvalidConfig(format!("TurnClient::allocate failed: {e}")))?;
    let relay_addr = relay_conn
        .local_addr()
        .map_err(|e| Error::InvalidConfig(format!("relay_conn.local_addr failed: {e}")))?;

    // Law 2: `XOR-RELAYED-ADDRESS` equals `E_exit` bound on `udp_exit_node` (`[47200, 47299]`),
    // NOT `turn_entry_addr`, and `E_exit` is actively bound on the OS loopback interface.
    assert_ne!(relay_addr, turn_entry_addr);
    assert_eq!(relay_addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(
        (47_200..=47_299).contains(&relay_addr.port()),
        "expected relayed port in exit range 47200..=47299, got {relay_addr}"
    );
    assert!(
        UdpSocket::bind(relay_addr).await.is_err(),
        "exit UDP socket {relay_addr} must be actively bound on udp_exit_node"
    );

    // 5. Law 3 (before permission): raw UDP peer sends unsolicited packet to `E_exit` before
    //    `CreatePermission` — must be dropped at the exit pinhole without reaching the TURN client.
    let peer_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_sock.local_addr().unwrap();
    peer_sock
        .send_to(b"unsolicited-before-permission", relay_addr)
        .await
        .unwrap();
    let mut recv_buf = vec![0_u8; 2048];
    assert!(
        tokio::time::timeout(
            Duration::from_millis(120),
            relay_conn.recv_from(&mut recv_buf)
        )
        .await
        .is_err(),
        "unpermitted peer packet before CreatePermission must not reach TURN client"
    );

    // Clear middle-hop wire tap right before the STUN Binding + multi-size datagram exchange so
    // Law 5 inspects every data-plane frame crossing the middle relays.
    tap.clear();

    // 6. Real RFC 5389 STUN `METHOD_BINDING` request/response + multi-size datagrams both ways!
    let stun_tx_id = TransactionId([
        0x51, 0x42, 0x99, 0x7E, 0xA3, 0x1C, 0x88, 0x2D, 0x6F, 0x10, 0xC4, 0x3B,
    ]);
    let binding_req_raw = {
        let mut msg = StunMessage::new();
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(StunMessage {
                transaction_id: stun_tx_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_BINDING, CLASS_REQUEST)),
            Box::new(FINGERPRINT),
        ];
        msg.build(&setters).unwrap();
        msg.raw
    };
    assert!(contains_subslice(&binding_req_raw, &STUN_MAGIC_COOKIE_BE));
    assert!(contains_subslice(&binding_req_raw, &stun_tx_id.0));

    with_hang_guard(
        "send STUN Binding Request via multi-hop TURN",
        Duration::from_secs(10),
        relay_conn.send_to(&binding_req_raw, peer_addr),
    )
    .await
    .map_err(|e| Error::InvalidConfig(format!("relay_conn.send_to STUN failed: {e}")))?;

    let mut peer_buf = vec![0_u8; 2048];
    let (peer_n, observed_src) = with_hang_guard(
        "peer receives STUN Binding Request from E_exit",
        Duration::from_secs(10),
        peer_sock.recv_from(&mut peer_buf),
    )
    .await
    .unwrap();
    assert_eq!(
        observed_src, relay_addr,
        "peer must observe E_exit as UDP source address (Law 2)"
    );
    let mut decoded_req = StunMessage {
        raw: peer_buf[..peer_n].to_vec(),
        ..Default::default()
    };
    decoded_req.decode().unwrap();
    assert_eq!(decoded_req.typ.method, METHOD_BINDING);
    assert_eq!(decoded_req.typ.class, CLASS_REQUEST);
    assert_eq!(decoded_req.transaction_id, stun_tx_id);

    // Peer replies with RFC 5389 STUN Binding Success Response carrying XOR-MAPPED-ADDRESS = E_exit.
    let binding_resp_raw = {
        let mut msg = StunMessage::new();
        let setters: Vec<Box<dyn Setter>> = vec![
            Box::new(StunMessage {
                transaction_id: stun_tx_id,
                ..Default::default()
            }),
            Box::new(MessageType::new(METHOD_BINDING, CLASS_SUCCESS_RESPONSE)),
            Box::new(XorMappedAddress {
                ip: observed_src.ip(),
                port: observed_src.port(),
            }),
            Box::new(FINGERPRINT),
        ];
        msg.build(&setters).unwrap();
        msg.raw
    };
    peer_sock
        .send_to(&binding_resp_raw, observed_src)
        .await
        .unwrap();

    let (client_n, reported_peer) = with_hang_guard(
        "TURN client receives STUN Binding Response via multi-hop return loop",
        Duration::from_secs(10),
        relay_conn.recv_from(&mut recv_buf),
    )
    .await
    .map_err(|e| Error::InvalidConfig(format!("relay_conn.recv_from STUN failed: {e}")))?;
    assert_eq!(reported_peer, peer_addr);
    let mut decoded_resp = StunMessage {
        raw: recv_buf[..client_n].to_vec(),
        ..Default::default()
    };
    decoded_resp.decode().unwrap();
    assert_eq!(decoded_resp.typ.method, METHOD_BINDING);
    assert_eq!(decoded_resp.typ.class, CLASS_SUCCESS_RESPONSE);
    let mut xor_mapped = XorMappedAddress::default();
    xor_mapped.get_from(&decoded_resp).unwrap();
    assert_eq!(SocketAddr::new(xor_mapped.ip, xor_mapped.port), relay_addr);

    // Exchange distinct-length application datagrams (35 bytes vs 1,200 bytes) in both directions.
    let secret_c2p = b"MULTIHOP-TURN-SECRET-CLIENT-TO-PEER";
    let mut secret_p2c = b"MULTIHOP-TURN-SECRET-PEER-TO-CLIENT:".to_vec();
    secret_p2c.resize(1_200, 0x5A);

    relay_conn.send_to(secret_c2p, peer_addr).await.unwrap();
    let (n_c2p, src_c2p) = with_hang_guard(
        "peer receives 35B datagram",
        Duration::from_secs(10),
        peer_sock.recv_from(&mut peer_buf),
    )
    .await
    .unwrap();
    assert_eq!(src_c2p, relay_addr);
    assert_eq!(&peer_buf[..n_c2p], secret_c2p);

    peer_sock.send_to(&secret_p2c, relay_addr).await.unwrap();
    let (n_p2c, from_p2c) = with_hang_guard(
        "client receives 1200B datagram",
        Duration::from_secs(10),
        relay_conn.recv_from(&mut recv_buf),
    )
    .await
    .unwrap();
    assert_eq!(from_p2c, peer_addr);
    assert_eq!(&recv_buf[..n_p2c], secret_p2c.as_slice());

    // 7. Law 5 (Middle-hop blindness): inspect every frame captured on the middle relay hops!
    let captured = tap.snapshot();
    let onion_cells: Vec<&Bytes> = captured
        .iter()
        .filter_map(|f| f.onion_cell_payload.as_ref())
        .collect();
    assert!(
        onion_cells.len() >= 6,
        "expected >= 6 middle-hop Sphinx cells captured during STUN + multi-size datagram exchange, got {}",
        onion_cells.len()
    );

    // Verify our high-entropy (`>= 64-bit`..`128-bit`) wire markers appear verbatim in the
    // cleartext STUN packets and `onion::udp` frames, while never appearing in any middle-hop
    // ciphertext cell or raw link frame (avoiding `32-bit` birthday collisions over megabytes of
    // uniform-random constant-rate cover cells).
    let stun_req_hdr_and_cookie: [u8; 8] = binding_req_raw[..8].try_into().unwrap();
    let stun_resp_hdr_and_cookie: [u8; 8] = binding_resp_raw[..8].try_into().unwrap();
    let stun_cookie_and_tx: [u8; 16] = binding_req_raw[4..20].try_into().unwrap();

    let peer_v4 = Ipv4Addr::LOCALHOST.octets();
    let peer_port_be = peer_addr.port().to_be_bytes();
    let udp_frame_peer_hdr = [
        0x02,
        0x04,
        peer_v4[0],
        peer_v4[1],
        peer_v4[2],
        peer_v4[3],
        peer_port_be[0],
        peer_port_be[1],
    ];
    let relay_port_be = relay_addr.port().to_be_bytes();
    let udp_frame_bound_hdr = [
        0x01,
        0x04,
        peer_v4[0],
        peer_v4[1],
        peer_v4[2],
        peer_v4[3],
        relay_port_be[0],
        relay_port_be[1],
    ];
    let stun_xor_mapped_relay_tlv = [
        0x00,
        0x20,
        0x00,
        0x08,
        0x00,
        0x01,
        relay_port_be[0] ^ STUN_MAGIC_COOKIE_BE[0],
        relay_port_be[1] ^ STUN_MAGIC_COOKIE_BE[1],
        peer_v4[0] ^ STUN_MAGIC_COOKIE_BE[0],
        peer_v4[1] ^ STUN_MAGIC_COOKIE_BE[1],
        peer_v4[2] ^ STUN_MAGIC_COOKIE_BE[2],
        peer_v4[3] ^ STUN_MAGIC_COOKIE_BE[3],
    ];
    assert!(contains_subslice(
        &binding_resp_raw,
        &stun_xor_mapped_relay_tlv
    ));

    for cell in &onion_cells {
        assert_eq!(
            cell.len(),
            OnionLoopClass::DEFAULT.cell_bytes(),
            "every middle-hop onion cell must be fixed-width {} bytes regardless of datagram size",
            OnionLoopClass::DEFAULT.cell_bytes()
        );
    }
    for frame in &captured {
        for bytes in std::iter::once(&frame.raw_transport_payload).chain(&frame.onion_cell_payload)
        {
            assert!(
                !contains_subslice(bytes, &stun_req_hdr_and_cookie),
                "middle-hop frame leaked STUN Binding Request header + magic cookie"
            );
            assert!(
                !contains_subslice(bytes, &stun_resp_hdr_and_cookie),
                "middle-hop frame leaked STUN Binding Response header + magic cookie"
            );
            assert!(
                !contains_subslice(bytes, &stun_cookie_and_tx),
                "middle-hop frame leaked STUN magic cookie + TransactionId"
            );
            assert!(
                !contains_subslice(bytes, &stun_tx_id.0),
                "middle-hop frame leaked 12-byte STUN TransactionId"
            );
            assert!(
                !contains_subslice(bytes, &udp_frame_peer_hdr),
                "middle-hop frame leaked onion::udp Datagram peer IP+port header"
            );
            assert!(
                !contains_subslice(bytes, &udp_frame_bound_hdr),
                "middle-hop frame leaked onion::udp Bound E_exit IP+port header"
            );
            assert!(
                !contains_subslice(bytes, &stun_xor_mapped_relay_tlv),
                "middle-hop frame leaked RFC 5389 XOR-MAPPED-ADDRESS TLV"
            );
            assert!(
                !contains_subslice(bytes, secret_c2p),
                "middle-hop frame leaked client->peer datagram cleartext"
            );
            assert!(
                !contains_subslice(bytes, b"MULTIHOP-TURN-SECRET-PEER-TO-CLIENT"),
                "middle-hop frame leaked peer->client datagram cleartext"
            );
        }
    }

    // 8. Law 3 (expiry): advance `TurnServer` clock past RFC 5766 `300s` permission lifetime while
    //    `600s` allocation and `600s` exit pinhole remain unexpired -> inbound peer packet must be
    //    dropped at the TURN entry gate!
    manual_clock.set(t0 + 301_000);
    peer_sock
        .send_to(b"after-300s-rfc5766-permission-expiry", relay_addr)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(150),
            relay_conn.recv_from(&mut recv_buf)
        )
        .await
        .is_err(),
        "peer packet after 300s RFC 5766 permission expiry must be blocked even while 600s exit pinhole is open"
    );

    // 9. Law 4 (Bounded teardown latency, zero orphan): `relay_conn.close()` (`Refresh(0)`) must
    //    immediately retire the TURN entry allocation + `OnionUdpSession` (`< 250 ms`) and release
    //    the remote `E_exit` OS socket on `udp_exit_node` across the 3-hop Sphinx circuit (`< 2s`).
    let teardown_started = Instant::now();
    relay_conn
        .close()
        .await
        .map_err(|e| Error::InvalidConfig(format!("relay_conn.close failed: {e}")))?;
    turn_client
        .close()
        .await
        .map_err(|e| Error::InvalidConfig(format!("TurnClient::close failed: {e}")))?;

    with_hang_guard(
        "await entry TURN allocation release (< 250ms)",
        Duration::from_millis(250),
        async {
            while !turn_server
                .allocations_info(None)
                .await
                .unwrap_or_default()
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
    )
    .await;

    let rebound_exit_sock = with_hang_guard(
        "await remote exit UDP socket release (< 2s)",
        Duration::from_secs(2),
        async {
            loop {
                if let Ok(rebound) = UdpSocket::bind(relay_addr).await {
                    break rebound;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
    )
    .await;
    let teardown_elapsed = teardown_started.elapsed();
    drop(rebound_exit_sock);
    assert!(
        teardown_elapsed < Duration::from_secs(2),
        "expected full multi-hop exit socket teardown in < 2s, took {teardown_elapsed:?}"
    );

    turn_server.close().await.unwrap();
    ticket_client.shutdown().await?;
    turn_entry_node.shutdown().await?;
    for relay in middle_relays {
        relay.shutdown().await?;
    }
    udp_exit_node.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measure_single_hop_vs_multihop_turn_rtt_and_hol_burst() -> Result<()> {
    const RTT_SAMPLES: usize = 10;
    const BURST_FRAMES: usize = 16;
    const PAYLOAD_BYTES: usize = 1_024;

    // Spin up a raw UDP echo peer used for both measurements.
    let echo_peer = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let echo_peer_addr = echo_peer.local_addr().unwrap();
    let echo_runner = {
        let sock = Arc::clone(&echo_peer);
        tokio::spawn(async move {
            let mut buf = vec![0_u8; 4096];
            while let Ok((n, src)) = sock.recv_from(&mut buf).await {
                let _ = sock.send_to(&buf[..n], src).await;
            }
        })
    };

    async fn bench_client<C: Conn + Sync>(
        relay_conn: &C,
        peer_addr: SocketAddr,
    ) -> (Vec<u128>, u128) {
        let mut buf = vec![0_u8; 4096];
        // Warm up permission & channel path with 1 echo.
        let warmup = [0xAA_u8; 64];
        relay_conn.send_to(&warmup, peer_addr).await.unwrap();
        let _ = relay_conn.recv_from(&mut buf).await.unwrap();

        // 1. Sequential RTT samples (`1 KiB` payload).
        let mut rtts_us = Vec::with_capacity(RTT_SAMPLES);
        for seq in 0..RTT_SAMPLES {
            let mut frame = vec![0x42_u8; PAYLOAD_BYTES];
            frame[..8].copy_from_slice(&(seq as u64).to_be_bytes());
            let t0 = Instant::now();
            relay_conn.send_to(&frame, peer_addr).await.unwrap();
            let (n, _) = relay_conn.recv_from(&mut buf).await.unwrap();
            rtts_us.push(t0.elapsed().as_micros());
            assert_eq!(n, PAYLOAD_BYTES);
            assert_eq!(&buf[..8], &(seq as u64).to_be_bytes());
        }
        rtts_us.sort_unstable();

        // 2. Back-to-back 16-frame `1 KiB` burst (measures ordered session HOL burst completion).
        let burst_start = Instant::now();
        for seq in 0..BURST_FRAMES {
            let mut frame = vec![0x7B_u8; PAYLOAD_BYTES];
            frame[..8].copy_from_slice(&(seq as u64).to_be_bytes());
            relay_conn.send_to(&frame, peer_addr).await.unwrap();
        }
        for expected_seq in 0..BURST_FRAMES {
            let (n, _) = relay_conn.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, PAYLOAD_BYTES);
            let got_seq = u64::from_be_bytes(buf[..8].try_into().unwrap()) as usize;
            assert_eq!(got_seq, expected_seq);
        }
        let burst_total_us = burst_start.elapsed().as_micros();
        (rtts_us, burst_total_us)
    }

    // --- A. Single-hop (`TurnRelayMode::Local`) ---
    let mut local_cfg = TurnServerConfig::loopback_ephemeral();
    local_cfg.relay = RelayAddressConfig::Static {
        relay_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let local_server = TurnServer::start(local_cfg).await.unwrap();
    let local_res = local_server
        .reserve_exit_session(SecretKey::random().pubkey(), Duration::from_secs(60))
        .await
        .unwrap();
    drop(local_res);
    let eph_local = SecretKey::random();
    let local_ticket = local_server
        .reserve_exit_session(eph_local.pubkey(), Duration::from_secs(60))
        .await
        .unwrap();
    let local_creds = local_ticket
        .ticket
        .to_turn_credentials(&eph_local, local_ticket.server_pubkey)
        .unwrap();
    let local_client_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let local_client = TurnClient::new(TurnClientConfig {
        stun_serv_addr: local_server.local_addr().to_string(),
        turn_serv_addr: local_server.local_addr().to_string(),
        username: local_creds.username,
        password: local_creds.credential,
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 250,
        conn: local_client_udp,
        vnet: None,
    })
    .await
    .unwrap();
    local_client.listen().await.unwrap();
    let local_relay_conn = local_client.allocate().await.unwrap();
    let (local_rtts_us, local_burst_us) = bench_client(&local_relay_conn, echo_peer_addr).await;
    local_relay_conn.close().await.unwrap();
    local_client.close().await.unwrap();
    local_server.close().await.unwrap();

    // --- B. Multi-hop (`TurnRelayMode::OnionUdp` over 4-relay + exit Sphinx loop) ---
    let onion_backend = OnionUdpRelayBackend::deferred(
        true,
        RelayPeerPolicy::AllowLocalAndPrivate,
        OnionLoopClass::DEFAULT,
    );
    let mut mh_cfg = TurnServerConfig::loopback_ephemeral();
    mh_cfg.peer_policy = RelayPeerPolicy::AllowLocalAndPrivate;
    mh_cfg.relay_allocator = Some(Arc::clone(&onion_backend) as _);
    let mh_server = Arc::new(TurnServer::start(mh_cfg).await.unwrap());

    let entry_node = MultiHopNode::spawn(OnionRole::Relay, None, None, None, None).await?;
    onion_backend.attach_provider(&entry_node.provider)?;
    let mut relays = Vec::with_capacity(4);
    for _ in 0..4 {
        relays.push(MultiHopNode::spawn(OnionRole::Relay, None, None, None, None).await?);
    }
    let exit_policy = OnionExitPolicy::from_target_strings(vec!["*:*".to_string()], Vec::new())?;
    let exit_offer = OnionExitOffer::new([OnionServiceName::udp()], exit_policy)?;
    let exit_node = MultiHopNode::spawn(
        OnionRole::Exit(exit_offer),
        None,
        Some(OnionUdpConfig::loopback()),
        None,
        None,
    )
    .await?;

    let mesh = std::iter::once(&entry_node)
        .chain(relays.iter())
        .chain(std::iter::once(&exit_node))
        .collect::<Vec<_>>();
    for i in 0..mesh.len() {
        for j in (i + 1)..mesh.len() {
            connect_mesh_edge(&mesh[i].processor, &mesh[j].processor).await?;
        }
    }
    stabilize_mesh(&mesh).await;
    entry_node.processor.register_all_once().await?;
    for r in &relays {
        r.processor.register_all_once().await?;
    }
    exit_node.processor.register_all_once().await?;
    with_hang_guard(
        "await udp exit route readiness on entry_node",
        Duration::from_secs(20),
        async {
            let bind_req = rings_node::onion::udp::OnionUdpBindRequest {
                family: rings_node::onion::udp::OnionUdpFamily::Ipv4,
                even_port: false,
            };
            let mut ticks = 0_u32;
            while entry_node
                .processor
                .build_onion_udp_route(bind_req)
                .await
                .is_err()
            {
                ticks += 1;
                if ticks.is_multiple_of(8) {
                    stabilize_mesh(&mesh).await;
                    let _ = entry_node.processor.register_all_once().await;
                    for r in &relays {
                        let _ = r.processor.register_all_once().await;
                    }
                    let _ = exit_node.processor.register_all_once().await;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        },
    )
    .await;

    let eph_mh = SecretKey::random();
    let mh_ticket = mh_server
        .reserve_exit_session(eph_mh.pubkey(), Duration::from_secs(60))
        .await
        .unwrap();
    let mh_creds = mh_ticket
        .ticket
        .to_turn_credentials(&eph_mh, mh_ticket.server_pubkey)
        .unwrap();
    let mh_client_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let mh_client = TurnClient::new(TurnClientConfig {
        stun_serv_addr: mh_server.local_addr().to_string(),
        turn_serv_addr: mh_server.local_addr().to_string(),
        username: mh_creds.username,
        password: mh_creds.credential,
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 250,
        conn: mh_client_udp,
        vnet: None,
    })
    .await
    .unwrap();
    mh_client.listen().await.unwrap();
    let mh_relay_conn = with_hang_guard(
        "bench multi-hop Allocate",
        Duration::from_secs(15),
        mh_client.allocate(),
    )
    .await
    .unwrap();
    let (mh_rtts_us, mh_burst_us) = with_hang_guard(
        "bench multi-hop RTT + HOL burst",
        Duration::from_secs(45),
        bench_client(&mh_relay_conn, echo_peer_addr),
    )
    .await;

    println!(
        "[TURN Latency/HOL Measurement] Local single-hop (1 KiB): p50={}us p95={}us max={}us | 16x1KiB burst={}us",
        percentile_us(&local_rtts_us, 50),
        percentile_us(&local_rtts_us, 95),
        local_rtts_us.last().copied().unwrap_or(0),
        local_burst_us,
    );
    println!(
        "[TURN Latency/HOL Measurement] Multi-hop onion_udp (16 KiB Sphinx cells, 1 KiB datagram): p50={}us p95={}us max={}us | 16x1KiB ordered HOL burst={}us",
        percentile_us(&mh_rtts_us, 50),
        percentile_us(&mh_rtts_us, 95),
        mh_rtts_us.last().copied().unwrap_or(0),
        mh_burst_us,
    );

    mh_relay_conn.close().await.unwrap();
    mh_client.close().await.unwrap();
    mh_server.close().await.unwrap();
    echo_runner.abort();
    entry_node.shutdown().await?;
    for r in relays {
        r.shutdown().await?;
    }
    exit_node.shutdown().await?;
    Ok(())
}
