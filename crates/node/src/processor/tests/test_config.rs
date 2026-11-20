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

    Ok(())
}
