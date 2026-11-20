use rings_core::dht::RelayCustody;
use rings_core::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::message::OriginQuotaConfig;
use rings_core::message::OriginQuotaLaneConfig;

use super::common::*;
use super::*;
use crate::descriptor::registry_lease;
use crate::processor::config::parse_udp_port_range;
use crate::registration::registry_refresh_bound;
use crate::tests::test_processor_builder;

#[test]
fn test_udp_port_range_absent_by_default() {
    let range = parse_udp_port_range(None, None);

    assert!(matches!(range, core::result::Result::Ok(None)));
}

#[test]
fn test_udp_port_range_accepts_valid_bounds() {
    let range = parse_udp_port_range(Some(49160), Some(49200));

    assert!(matches!(
        range,
        Ok(Some(range)) if range.min() == 49160 && range.max() == 49200
    ));
}

#[test]
fn test_udp_port_range_rejects_partial_bounds() {
    let range = parse_udp_port_range(Some(49160), None);

    assert!(matches!(
        range,
        Err(Error::IncompleteUdpPortRange {
            min: Some(49160),
            max: None
        })
    ));
}

#[test]
fn test_udp_port_range_rejects_zero_bound() {
    let range = parse_udp_port_range(Some(0), Some(49200));

    assert!(matches!(
        range,
        Err(Error::InvalidUdpPortRange(
            rings_transport::network_config::UdpPortRangeError::ZeroBound { min: 0, max: 49200 }
        ))
    ));
}

#[test]
fn test_udp_port_range_rejects_inverted_bounds() {
    let range = parse_udp_port_range(Some(49200), Some(49160));

    assert!(matches!(
        range,
        Err(Error::InvalidUdpPortRange(
            rings_transport::network_config::UdpPortRangeError::Inverted {
                min: 49200,
                max: 49160
            }
        ))
    ));
}

#[test]
fn test_online_node_timing_requires_heartbeat_interval_less_than_ttl_when_enabled() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let mut config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    );
    config.online_node_heartbeat_interval = Duration::from_secs(90);
    config.online_node_ttl = Duration::from_secs(30);

    assert!(matches!(
        ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build),
        Err(Error::InvalidConfig(message))
            if message.contains("online_node_heartbeat_interval")
                && message.contains("online_node_ttl")
    ));
}

/// Both registries accept a heartbeat interval just below half the registry lease, `Λ / 2`, and
/// refuse one at it, at which a lease would no longer span two heartbeats (#918). The bound is
/// pinned to the 300 s the configuration documentation states.
#[test]
fn test_registry_heartbeat_must_refresh_before_a_descriptor_expires() -> Result<()> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let bound = registry_refresh_bound();
    assert_eq!(bound, Duration::from_secs(300));
    let below = bound - Duration::from_millis(1);

    let presence = |interval: Duration| {
        let mut config = ProcessorConfig::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            delegatee_key.clone(),
            3,
        );
        config.online_node_heartbeat_interval = interval;
        config.online_node_ttl = bound * 2;
        ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build)
    };
    assert!(presence(below).is_ok());
    assert!(matches!(
        presence(bound),
        Err(Error::InvalidConfig(message))
            if message.contains("online_node_heartbeat_interval")
                && message.contains("half the registry lease")
    ));

    let exit = |interval: Duration| -> Result<_> {
        let mut config = ProcessorConfig::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            delegatee_key.clone(),
            3,
        )
        .onion_role(OnionRole::Exit(OnionExitOffer::new(
            [OnionServiceName::tcp()],
            onion_policy(&["example.com:443"], &[])?,
        )?));
        config.onion_exit_heartbeat_interval = interval;
        config.onion_exit_ttl = bound * 2;
        Ok(ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build))
    };
    assert!(exit(below)?.is_ok());
    assert!(matches!(
        exit(bound)?,
        Err(Error::InvalidConfig(message))
            if message.contains("onion_exit_heartbeat_interval")
                && message.contains("half the registry lease")
    ));
    Ok(())
}

/// A registry TTL longer than the lease is refused at start: readers refuse a freshness window
/// past the lease (the lease law), so the node's own descriptor would never be listed.
#[test]
fn test_registry_ttl_must_not_exceed_the_lease() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let presence = |ttl: Duration| {
        let mut config = ProcessorConfig::new(
            0,
            "stun://stun.l.google.com:19302".to_string(),
            delegatee_key.clone(),
            3,
        );
        config.online_node_ttl = ttl;
        ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build)
    };
    let lease = registry_lease();
    assert!(presence(lease).is_ok());
    assert!(matches!(
        presence(lease + Duration::from_millis(1)),
        Err(Error::InvalidConfig(message))
            if message.contains("online_node_ttl") && message.contains("registry lease")
    ));
}

#[test]
fn test_presence_advertisement_can_be_disabled() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let mut config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    );
    config.online_node_heartbeat_interval = Duration::from_secs(90);
    config.online_node_ttl = Duration::from_secs(30);
    config.advertise_presence = false;

    let processor = ProcessorBuilder::from_config(&config)
        .unwrap()
        .build()
        .unwrap();

    assert!(processor.registration_tasks.is_empty());
}

#[test]
fn test_presence_advertisement_is_enabled_by_default() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    );

    let builder = ProcessorBuilder::from_config(&config).unwrap();

    assert!(builder.advertise_presence);
    assert_eq!(
        builder.dht_virtual_nodes,
        DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
    assert_eq!(builder.origin_quota, OriginQuotaConfig::default());
}

#[test]
fn test_dht_virtual_nodes_rejects_values_above_cost_bound() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .dht_virtual_nodes(MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER.saturating_add(1));

    assert!(matches!(
        ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build),
        Err(Error::InvalidConfig(message))
            if message.contains("dht_virtual_nodes")
                && message.contains(&MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER.to_string())
    ));
}

#[test]
fn test_serialized_processor_config_defaults_dht_virtual_nodes() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let yaml = format!(
        r#"
network_id: 0
ice_servers: stun://stun.l.google.com:19302
external_address: null
udp_port_min: null
udp_port_max: null
delegatee_key: "{}"
stabilize_interval: 15
online_node_heartbeat_interval_secs: 30
online_node_ttl_secs: 60
online_node_type: Native
advertise_presence: true
"#,
        delegatee_key.dump().unwrap()
    );

    let serialized = serde_saphyr::from_str::<ProcessorConfigSerialized>(&yaml).unwrap();
    let config = ProcessorConfig::try_from(serialized).unwrap();
    let builder = ProcessorBuilder::from_config(&config).unwrap();

    assert_eq!(
        builder.dht_virtual_nodes,
        DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
    assert_eq!(builder.origin_quota, OriginQuotaConfig::default());
}

#[test]
fn test_processor_construction_preserves_explicit_origin_quota() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let lane = OriginQuotaLaneConfig::new(2, 3, 5, 7, 11).unwrap();
    let quota = OriginQuotaConfig::new(lane, lane, lane, lane);
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .origin_quota(quota);

    let builder = ProcessorBuilder::from_config(&config).unwrap();

    assert_eq!(builder.origin_quota, quota);
}

/// A config for network `0` with a fresh key.
fn onion_test_config() -> ProcessorConfig {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
}

/// An exit offering `https` to `example.com:443`.
fn https_exit_role() -> Result<OnionRole<OnionExitOffer>> {
    Ok(OnionRole::Exit(OnionExitOffer::new(
        vec![OnionServiceName::https()],
        onion_policy(&["example.com:443"], &[])?,
    )?))
}

/// `relay ∈ Σ_n ⇒ advertise_presence`: the relay registration lives in the online-node
/// descriptor, for a relay and for an exit alike.
#[test]
fn test_onion_relay_requires_presence_advertisement() -> Result<()> {
    for role in [OnionRole::Relay, https_exit_role()?] {
        let mut config = onion_test_config().onion_role(role);
        config.advertise_presence = false;

        assert!(matches!(
            ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build),
            Err(Error::InvalidConfig(message))
                if message.contains("advertise_onion_relay")
                    && message.contains("advertise_presence")
        ));
    }
    Ok(())
}

/// A typed role survives the round trip through the serialized flags, which are parsed onto
/// the role once (`OnionRole::from_flags`, whose rejections `onion::role` tests).
#[test]
fn test_serialized_onion_flags_parse_onto_the_role() -> Result<()> {
    let config = onion_test_config().onion_role(https_exit_role()?);
    let serialized = ProcessorConfigSerialized::try_from(config.clone())?;

    assert!(serialized.advertise_onion_relay && serialized.advertise_onion_exit);
    assert_eq!(
        ProcessorConfig::try_from(serialized)?.onion_role,
        config.onion_role
    );
    Ok(())
}

/// The reserved HTTPS name is valid without a parallel transport discriminator; an exit
/// registers `relay` with it, whose capability carries the process epoch `e_n`, and every build
/// (process start) draws a fresh one (#834 D2).
#[tokio::test]
async fn test_https_onion_exit_registers_relay_with_a_fresh_process_epoch() -> Result<()> {
    let config = onion_test_config().onion_role(https_exit_role()?);
    let start = || test_processor_builder(&config).build();
    let processor = start()?;
    let restarted = start()?;
    let descriptor = processor.online_node_descriptor_at(get_epoch_ms())?;

    assert_eq!(processor.onion_role(), &https_exit_role()?);
    // The test processor keeps no relay store, so it declares that it holds no relay mail.
    assert_eq!(descriptor.capabilities, OnlineNodeCapabilities {
        relay_custody: RelayCustody::HoldsNone,
        ..OnlineNodeCapabilities::onion_relay(processor.onion_process_epoch.get())
    });
    assert_ne!(
        processor.onion_process_epoch.get(),
        restarted.onion_process_epoch.get()
    );
    Ok(())
}

/// An exit registers world-facing symbols of the closed signature only: neither names outside
/// `Σ` nor the identity symbol `relay` parse as an exit service, in code or in a config file.
#[test]
fn test_onion_exit_service_must_be_a_world_facing_symbol() {
    assert_eq!(
        serde_saphyr::from_str::<Vec<OnionServiceName>>("[tcp, https]").ok(),
        Some(vec![OnionServiceName::tcp(), OnionServiceName::https()])
    );
    for outside in ["web", "relay"] {
        assert!(OnionServiceName::parse(outside).is_err());
        assert!(serde_saphyr::from_str::<Vec<OnionServiceName>>(&format!("[{outside}]")).is_err());
    }
}

#[test]
fn test_serialized_remote_rpc_proxy_url_round_trips_and_validates() -> Result<()> {
    use crate::remote_endpoint::RemoteRpcProxy;

    let proxy = RemoteRpcProxy::parse("http://127.0.0.1:18080")?;
    let config = onion_test_config().remote_rpc_proxy(proxy.clone());
    let serialized = ProcessorConfigSerialized::try_from(config)?;
    assert_eq!(
        serialized.remote_rpc_proxy_url.as_deref(),
        Some("http://127.0.0.1:18080/")
    );
    let restored = ProcessorConfig::try_from(serialized.clone())?;
    assert_eq!(restored.remote_rpc_proxy, Some(proxy));

    let mut invalid = serialized;
    invalid.remote_rpc_proxy_url = Some("socks5://127.0.0.1:1080".to_string());
    assert!(matches!(
        ProcessorConfig::try_from(invalid),
        Err(Error::InvalidConfig(_))
    ));
    Ok(())
}

/// Verifies that the first-principles [`NodeNetworkParameters`] contract (`build_transport`,
/// optional `endpoint_hint`, and optional `on_session_key_rotated`) is model-complete for
/// arbitrary transport parameters without requiring IP/UDP or relay server fields when a
/// medium does not use them.
#[tokio::test]
async fn test_node_network_parameters_contract_is_complete_for_custom_transports() -> Result<()> {
    /// Custom transport parameters requiring only `build_transport` (e.g. proximity ad-hoc media
    /// with no public locator and no session-signed relay servers).
    #[derive(Clone, Debug)]
    struct MinimalAdHocParams {
        /// Whether this parameter set is valid.
        valid: bool,
    }

    impl NodeNetworkParameters for MinimalAdHocParams {
        /// Validate custom parameters and construct the [`SharedTransport`].
        fn build_transport(
            &self,
            _delegatee_key: &DelegateeKey,
        ) -> Result<rings_transport::core::transport::SharedTransport> {
            if !self.valid {
                return Err(Error::InvalidConfig(
                    "invalid custom medium parameter".to_string(),
                ));
            }
            Ok(TransportNetworkConfig::default().build_shared_transport(None))
        }
    }

    /// Custom transport parameters advertising an explicit locator and tracking session-key
    /// rotations via `on_session_key_rotated`.
    #[derive(Clone, Debug)]
    struct CustomRoutedParams {
        /// External endpoint locator advertised in the online-node descriptor.
        locator: String,
        /// Observed session-key rotation count.
        rotations: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl NodeNetworkParameters for CustomRoutedParams {
        /// Construct the [`SharedTransport`].
        fn build_transport(
            &self,
            _delegatee_key: &DelegateeKey,
        ) -> Result<rings_transport::core::transport::SharedTransport> {
            Ok(
                TransportNetworkConfig::new("", Some(self.locator.clone()), None)
                    .build_shared_transport(None),
            )
        }

        /// Return the advertised external locator.
        fn endpoint_hint(&self) -> Option<&str> {
            Some(&self.locator)
        }

        /// Record that the node rotated its session key.
        fn on_session_key_rotated(
            &self,
            _transport: &rings_transport::core::transport::SharedTransport,
            _next_key: &DelegateeKey,
        ) -> Result<()> {
            self.rotations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    // 1. Minimal transport implementing only `build_transport` uses defaults (`endpoint_hint = None`,
    //    `on_session_key_rotated = Ok(())`) and rotates session keys cleanly.
    let adhoc_config =
        onion_test_config().with_network_parameters(MinimalAdHocParams { valid: true });
    let adhoc_processor = test_processor_builder(&adhoc_config).build()?;
    assert_eq!(adhoc_processor.network_parameters().endpoint_hint(), None);

    // Invalid custom parameters are rejected at `ProcessorBuilder::build`.
    let invalid_config =
        onion_test_config().with_network_parameters(MinimalAdHocParams { valid: false });
    assert!(matches!(
        test_processor_builder(&invalid_config).build(),
        Err(Error::InvalidConfig(_))
    ));

    // 2. Routed transport implementing `endpoint_hint` and `on_session_key_rotated` receives
    //    lockstep rotation callbacks when `renew_delegatee_key` succeeds.
    let t1 = get_epoch_ms() + 1_000;
    let delegator = SecretKey::random();
    let initial_key = DelegateeKey::new_with_seckey_at(
        &delegator,
        u64::try_from(4 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX),
        t1,
    )
    .unwrap();
    let renewed_key = DelegateeKey::new_with_seckey_at(
        &delegator,
        u64::try_from(4 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX),
        t1 + SIGNING_HORIZON_LEAD_MS,
    )
    .unwrap();
    let rotations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut routed_config = ProcessorConfig::new(0, String::new(), initial_key, 3)
        .with_network_parameters(CustomRoutedParams {
            locator: "wss://node.rings.network/ws".to_string(),
            rotations: Arc::clone(&rotations),
        });
    routed_config.advertise_presence = false;
    let routed_processor = test_processor_builder(&routed_config).build_at(t1)?;
    assert_eq!(
        routed_processor.network_parameters().endpoint_hint(),
        Some("wss://node.rings.network/ws")
    );
    routed_processor.renew_delegatee_key(renewed_key, t1 + SIGNING_HORIZON_LEAD_MS)?;
    assert_eq!(rotations.load(std::sync::atomic::Ordering::SeqCst), 1);

    // 3. Injecting `TransportNetworkConfig` via `with_network_parameters` preserves
    //    `udp_port_range()` and round-trips through `ProcessorConfigSerialized` without dual-state divergence.
    let range = UdpPortRange::new(50000, 50100)?;
    let injected_traversal =
        onion_test_config().with_network_parameters(TransportNetworkConfig::new(
            "stun://stun.l.google.com:19302",
            Some("https://node.rings.network:50000".to_string()),
            Some(range),
        ));
    assert_eq!(injected_traversal.udp_port_range(), Some(range));
    let serialized = ProcessorConfigSerialized::try_from(injected_traversal)?;
    assert_eq!(serialized.ice_servers, "stun://stun.l.google.com:19302");
    assert_eq!(
        serialized.external_address.as_deref(),
        Some("https://node.rings.network:50000")
    );
    assert_eq!(serialized.udp_port_min, Some(50000));
    assert_eq!(serialized.udp_port_max, Some(50100));

    // 4. Attempting to losslessly serialize a custom `NodeNetworkParameters` implementation
    //    into `ProcessorConfigSerialized` must fail explicitly instead of silently replacing it
    //    with an empty `TransportNetworkConfig`.
    let custom_cfg =
        onion_test_config().with_network_parameters(MinimalAdHocParams { valid: true });
    assert!(matches!(
        ProcessorConfigSerialized::try_from(custom_cfg),
        Err(Error::InvalidConfig(_))
    ));

    Ok(())
}

/// Constructive model-completeness proof for external transport backends (#1291):
/// implements a completely independent in-memory `TransportInterface` + `ConnectionInterface`
/// inside `rings-node` (an external crate relative to `rings-transport`) using **only** public
/// `rings-transport` APIs — with zero delegation to `DummyTransport` or `WebrtcTransport` —
/// injects it into two `Processor` instances via [`NodeNetworkParameters`], performs a full
/// 3-step handshake (`create_offer -> answer_offer -> accept_answer`), and verifies credit-bounded
/// custom payload delivery across the live connection.
#[tokio::test]
async fn test_independent_external_transport_backend_completes_handshake_and_credit_bounded_send(
) -> Result<()> {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use bytes::Bytes;
    use rings_transport::callback::InnerTransportCallback;
    use rings_transport::callback::NodeFramePool;
    use rings_transport::connection_pool::Pool;
    use rings_transport::connection_ref::ConnectionRef;
    use rings_transport::core::callback::BoxedTransportCallback;
    use rings_transport::core::credit::CreditIndex;
    use rings_transport::core::pool::ChannelLane;
    use rings_transport::core::transport::ConnectionInterface;
    use rings_transport::core::transport::ConnectionStateSnapshot;
    use rings_transport::core::transport::LaneCreditView;
    use rings_transport::core::transport::SendPermit;
    use rings_transport::core::transport::SharedTransport;
    use rings_transport::core::transport::TransportConnectionState;
    use rings_transport::core::transport::TransportInterface;
    use rings_transport::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
    use rings_transport::delivery::DeliveryFuture;
    use rings_transport::delivery::SendCreditWait;
    use rings_transport::error::Error as TransportError;
    use rings_transport::notifier::Notifier;

    /// Shared in-memory switchboard connecting independent `MemoryConnection` endpoints by id.
    #[derive(Clone, Default)]
    struct MemorySwitchboard {
        /// Registry of live memory endpoints keyed by their handshake token.
        endpoints: Arc<Mutex<HashMap<String, Arc<MemoryConnection>>>>,
    }

    impl std::fmt::Debug for MemorySwitchboard {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MemorySwitchboard").finish_non_exhaustive()
        }
    }

    /// Independent external connection backend implementing [`ConnectionInterface`] purely via
    /// public `rings-transport` credit and callback APIs (zero built-in transport delegation).
    struct MemoryConnection {
        /// Unique endpoint token exchanged as the opaque handshake payload.
        endpoint_id: String,
        /// Switchboard used to resolve the peer endpoint during handshake and frame delivery.
        switchboard: MemorySwitchboard,
        /// Negotiated peer endpoint token once handshake completes.
        peer_id: Mutex<Option<String>>,
        /// Connection lifecycle state snapshot.
        state: Mutex<ConnectionStateSnapshot>,
        /// Public credit and inbound-admission callback adapter from `rings-transport`.
        callback: Arc<InnerTransportCallback>,
        /// Notifier woken when channel opens or closes.
        open_notifier: Notifier,
    }

    impl MemoryConnection {
        /// Deliver a raw wire frame into this endpoint's credit-admitted inbound pipeline.
        fn deliver_raw(&self, raw: Bytes, lane: ChannelLane) {
            self.callback.receive_inbound_frame(raw, lane);
        }

        /// Spawn one credit pump per [`CreditIndex::ALL`] using public
        /// [`InnerTransportCallback::next_credit_frame`].
        #[allow(clippy::disallowed_methods)]
        fn spawn_credit_pumps(self: &Arc<Self>) {
            for index in CreditIndex::ALL {
                let conn = Arc::downgrade(self);
                let cb = Arc::clone(&self.callback);
                tokio::spawn(async move {
                    while let Some(frame) = cb.next_credit_frame(index).await {
                        let Some(conn) = conn.upgrade() else {
                            break;
                        };
                        let peer_id = conn.peer_id.lock().unwrap().clone();
                        if let Some(peer) = peer_id.and_then(|id| {
                            conn.switchboard.endpoints.lock().unwrap().get(&id).cloned()
                        }) {
                            peer.deliver_raw(frame, index.lane());
                        }
                    }
                });
            }
        }
    }

    #[async_trait]
    impl ConnectionInterface for MemoryConnection {
        type Handshake = String;
        type Error = TransportError;

        async fn send_message_with_permit(
            &self,
            payload: Bytes,
            lane: ChannelLane,
            permit: SendPermit,
        ) -> std::result::Result<DeliveryFuture, TransportError> {
            self.callback
                .send_custom_wire(payload, lane, permit, |wire, lane, permit| async move {
                    let Some(proof) = permit.try_mark_irrevocable() else {
                        return Err(TransportError::SendPermitRevoked);
                    };
                    let peer_id = self.peer_id.lock().unwrap().clone().ok_or_else(|| {
                        TransportError::ChannelOpen("peer not linked".to_string())
                    })?;
                    let peer = self
                        .switchboard
                        .endpoints
                        .lock()
                        .unwrap()
                        .get(&peer_id)
                        .cloned()
                        .ok_or_else(|| TransportError::ChannelOpen("peer offline".to_string()))?;
                    peer.deliver_raw(wire, lane);
                    proof.mark_accepted();
                    let delivered: DeliveryFuture = Box::pin(std::future::ready(Ok(())));
                    Ok(delivered)
                })
                .await
        }

        fn reserve_send_credit(&self, lane: ChannelLane) -> SendCreditWait<TransportError> {
            Box::pin(self.callback.reserve_credit(lane))
        }

        fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
            self.callback.send_credit_view(lane)
        }

        fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
            *self.state.lock().unwrap()
        }

        fn max_message_size(&self) -> usize {
            MAX_CHANNEL_MESSAGE_SIZE
        }

        async fn create_offer(&self) -> std::result::Result<String, TransportError> {
            *self.state.lock().unwrap() =
                ConnectionStateSnapshot::new(TransportConnectionState::New, false);
            Ok(self.endpoint_id.clone())
        }

        async fn answer_offer(&self, offer: String) -> std::result::Result<String, TransportError> {
            *self.peer_id.lock().unwrap() = Some(offer);
            *self.state.lock().unwrap() =
                ConnectionStateSnapshot::new(TransportConnectionState::Connecting, false);
            Ok(self.endpoint_id.clone())
        }

        async fn accept_answer(&self, answer: String) -> std::result::Result<(), TransportError> {
            *self.peer_id.lock().unwrap() = Some(answer.clone());
            *self.state.lock().unwrap() =
                ConnectionStateSnapshot::new(TransportConnectionState::Connected, true);
            self.open_notifier.wake();
            self.callback
                .dispatch_peer_connection_state(TransportConnectionState::Connected);
            self.callback.on_channel_open().await;

            let peer = self
                .switchboard
                .endpoints
                .lock()
                .unwrap()
                .get(&answer)
                .cloned();
            if let Some(peer) = peer {
                *peer.state.lock().unwrap() =
                    ConnectionStateSnapshot::new(TransportConnectionState::Connected, true);
                peer.open_notifier.wake();
                peer.callback
                    .dispatch_peer_connection_state(TransportConnectionState::Connected);
                peer.callback.on_channel_open().await;
            }
            Ok(())
        }

        async fn wait_for_channel_open(&self) -> std::result::Result<(), TransportError> {
            if self.channel_is_open() {
                Ok(())
            } else {
                Err(TransportError::ChannelOpen("channel not open".to_string()))
            }
        }

        async fn close(&self) -> std::result::Result<(), TransportError> {
            *self.state.lock().unwrap() =
                ConnectionStateSnapshot::new(TransportConnectionState::Closed, false);
            self.switchboard
                .endpoints
                .lock()
                .unwrap()
                .remove(&self.endpoint_id);
            self.callback
                .dispatch_peer_connection_state(TransportConnectionState::Closed);
            self.callback.on_channel_close().await;
            Ok(())
        }
    }

    /// Independent external [`TransportInterface`] managing [`MemoryConnection`] instances.
    struct MemoryTransport {
        /// Shared switchboard linking peers in this test network.
        switchboard: MemorySwitchboard,
        /// Per-peer connection pool.
        pool: Pool<MemoryConnection>,
        /// Shared node frame pool for credit window lending.
        frame_pool: NodeFramePool,
        /// Monotonic endpoint id counter.
        seq: std::sync::atomic::AtomicU64,
        /// Prefix differentiating endpoints across nodes.
        prefix: String,
    }

    #[async_trait]
    impl TransportInterface for MemoryTransport {
        type Connection = MemoryConnection;
        type Error = TransportError;

        async fn new_connection(
            &self,
            cid: &str,
            callback: BoxedTransportCallback,
        ) -> std::result::Result<ConnectionRef<MemoryConnection>, TransportError> {
            let id = format!(
                "{}-{}",
                self.prefix,
                self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            let notifier = Notifier::default();
            let inner_cb = Arc::new(InnerTransportCallback::new(
                cid,
                callback,
                notifier.clone(),
                self.frame_pool.clone(),
            ));
            let conn = MemoryConnection {
                endpoint_id: id.clone(),
                switchboard: self.switchboard.clone(),
                peer_id: Mutex::new(None),
                state: Mutex::new(ConnectionStateSnapshot::new(
                    TransportConnectionState::New,
                    false,
                )),
                callback: inner_cb,
                open_notifier: notifier,
            };
            let conn_ref = self.pool.safely_insert(cid, conn).await?;
            let live = conn_ref.upgrade()?;
            self.switchboard
                .endpoints
                .lock()
                .unwrap()
                .insert(id, Arc::clone(&live));
            live.spawn_credit_pumps();
            Ok(conn_ref)
        }

        async fn close_connection_if_current(
            &self,
            connection: &ConnectionRef<MemoryConnection>,
        ) -> std::result::Result<bool, TransportError> {
            self.pool.safely_remove_if_current(connection).await
        }

        fn connection(
            &self,
            cid: &str,
        ) -> std::result::Result<ConnectionRef<MemoryConnection>, TransportError> {
            self.pool.connection(cid)
        }

        fn connection_ids(&self) -> Vec<String> {
            self.pool.connection_ids()
        }
    }

    /// [`NodeNetworkParameters`] implementation that injects [`MemoryTransport`] into `Processor`.
    #[derive(Clone, Debug)]
    struct MemoryNetworkParams {
        /// Shared switchboard connecting nodes in this test.
        switchboard: MemorySwitchboard,
        /// Node label prefix.
        prefix: String,
    }

    impl NodeNetworkParameters for MemoryNetworkParams {
        fn build_transport(&self, _delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
            Ok(Arc::new(MemoryTransport {
                switchboard: self.switchboard.clone(),
                pool: Pool::new(),
                frame_pool: NodeFramePool::new(),
                seq: std::sync::atomic::AtomicU64::new(0),
                prefix: self.prefix.clone(),
            }))
        }
    }

    let switchboard = MemorySwitchboard::default();
    let cfg1 = onion_test_config().with_network_parameters(MemoryNetworkParams {
        switchboard: switchboard.clone(),
        prefix: "node1".to_string(),
    });
    let cfg2 = onion_test_config().with_network_parameters(MemoryNetworkParams {
        switchboard,
        prefix: "node2".to_string(),
    });
    let p1 = test_processor_builder(&cfg1).build()?;
    let p2 = test_processor_builder(&cfg2).build()?;

    let expected_peer = p2.did();
    let offer = p1
        .swarm
        .create_offer(expected_peer)
        .await
        .map_err(Error::CoreError)?;
    let answer_reply = p2
        .swarm
        .answer_offer(offer)
        .await
        .map_err(Error::CoreError)?;
    let rings_core::swarm::OfferReply::Answer(answer) = answer_reply else {
        panic!("expected direct OfferReply::Answer from p2");
    };
    p1.swarm
        .accept_answer(answer)
        .await
        .map_err(Error::CoreError)?;

    // Both processors completed handshake over the independent external transport and can send
    // credit-admitted frames through `ConnectionInterface::reserve_send_credit` +
    // `send_message_with_permit`.
    let conn = p1
        .swarm
        .transport()
        .dyn_connection(&expected_peer.to_string())
        .map_err(rings_core::error::Error::Transport)
        .map_err(Error::CoreError)?;
    let live = conn
        .upgrade()
        .map_err(rings_core::error::Error::Transport)
        .map_err(Error::CoreError)?;
    let credit = live
        .reserve_send_credit(ChannelLane::PRIORITY)
        .await
        .map_err(rings_core::error::Error::Transport)
        .map_err(Error::CoreError)?;
    let delivery = live
        .send_message_with_permit(
            Bytes::from_static(b"hello-from-external-transport"),
            ChannelLane::PRIORITY,
            SendPermit::always().with_credit(credit),
        )
        .await
        .map_err(rings_core::error::Error::Transport)
        .map_err(Error::CoreError)?;
    delivery
        .await
        .map_err(rings_core::error::Error::Transport)
        .map_err(Error::CoreError)?;

    p1.close().await?;
    p2.close().await?;
    Ok(())
}
