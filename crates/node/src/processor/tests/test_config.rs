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
        serde_saphyr::from_str::<Vec<OnionServiceName>>("[tcp, https, udp]").ok(),
        Some(vec![
            OnionServiceName::tcp(),
            OnionServiceName::https(),
            OnionServiceName::udp(),
        ])
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
/// optional `endpoint_hint`, optional `prepare_session_key_rotation`, and optional
/// `to_network_descriptor`) is model-complete for arbitrary transport parameters, fails closed on
/// malformed ICE/TURN configuration, and round-trips registered custom kinds through serialized YAML.
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

    /// Custom transport parameters advertising an explicit locator, tracking two-phase session-key
    /// rotations, and projecting into a registered `NetworkConfigDescriptor`.
    #[derive(Clone, Debug)]
    struct CustomRoutedParams {
        /// External endpoint locator advertised in the online-node descriptor.
        locator: String,
        /// Observed committed session-key rotation count.
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

        /// Prepare an infallible commit incrementing `rotations` once phase 2 executes.
        fn prepare_session_key_rotation(
            &self,
            _transport: &rings_transport::core::transport::SharedTransport,
            _next_key: &DelegateeKey,
        ) -> Result<crate::processor::SessionRotationCommit> {
            let rotations = Arc::clone(&self.rotations);
            Ok(Box::new(move || {
                rotations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }))
        }

        /// Project into a serde-extensible `NetworkConfigDescriptor`.
        fn to_network_descriptor(&self) -> Option<crate::processor::NetworkConfigDescriptor> {
            Some(crate::processor::NetworkConfigDescriptor {
                kind: "test-custom-routed".to_string(),
                params: serde_json::json!({ "locator": self.locator }),
            })
        }
    }

    // 1. Minimal transport implementing only `build_transport` uses defaults (`endpoint_hint = None`,
    //    `prepare_session_key_rotation = no-op`) and builds cleanly.
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

    // Malformed ICE server URLs and malformed `?pubkey=` TURN URLs fail closed at build time
    // instead of logging a warning and starting with zero traversal servers (Impl #3).
    for bad_spec in [
        "not-a-valid-ice-url",
        "turn://turn.rings.rs:3478?pubkey=NOT_VALID_BASE58",
        "turn:turn.rings.rs:3478?pubkey=NOT_VALID_BASE58",
        "turns:turn.rings.rs:5349?pubkey=",
        "turn://turn.rings.rs:3478?\npubkey=NOT_VALID_BASE58",
        &format!(
            "{}&pubkey=BAD",
            rings_turn::format_turn_server_url(
                "turn:turn.rings.rs:3478",
                SecretKey::random().pubkey()
            )
            .unwrap()
        ),
    ] {
        let bad_ice_cfg =
            ProcessorConfig::new(0, bad_spec.to_string(), adhoc_config.delegatee_key(), 3);
        assert!(
            matches!(
                test_processor_builder(&bad_ice_cfg).build(),
                Err(Error::InvalidConfig(_))
            ),
            "malformed ICE/TURN spec '{bad_spec}' must fail closed at build"
        );
    }

    // Standard RFC 7065 `turn:host:port?pubkey=<valid>` form validates and signs credentials cleanly!
    let turn_server_pk = SecretKey::random().pubkey();
    let rfc7065_valid =
        rings_turn::format_turn_server_url("turn:203.0.113.10:3478", turn_server_pk).unwrap();
    let rfc7065_cfg = ProcessorConfig::new(0, rfc7065_valid, adhoc_config.delegatee_key(), 3);
    let rfc7065_proc = test_processor_builder(&rfc7065_cfg).build()?;
    let committed_rfc7065 = rfc7065_proc
        .session_transport_credentials()
        .expect("dummy transport records signed credentials");
    assert!(
        committed_rfc7065.starts_with("turn://")
            && committed_rfc7065.contains("@203.0.113.10:3478")
            && !committed_rfc7065.contains("pubkey="),
        "RFC 7065 turn: URL must be signed into canonical turn://user:pass@host:port, got {committed_rfc7065}"
    );

    // Parse errors on serialized `ProcessorConfig` omit source snippets so secret `delegatee_key`
    // material cannot leak into error messages or logs (B2).
    let secret_marker = "SECRET_DELEGATEE_MARKER_DO_NOT_LEAK";
    let bad_serialized = format!("network_id: \"not-a-u32\"\ndelegatee_key: \"{secret_marker}\"\n");
    let parse_err = std::str::FromStr::from_str(&bad_serialized)
        .map(|_: ProcessorConfig| ())
        .unwrap_err()
        .to_string();
    assert!(
        !parse_err.contains(secret_marker),
        "ProcessorConfig parse error must not include YAML source snippets: {parse_err}"
    );

    // 2. Routed transport implementing `endpoint_hint`, `prepare_session_key_rotation`, and
    //    `to_network_descriptor` commits rotation callbacks and round-trips through YAML!
    crate::processor::register_network_transport_factory("test-custom-routed", |params| {
        let locator = params
            .get("locator")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::InvalidConfig("missing locator".to_string()))?
            .to_owned();
        Ok(Arc::new(CustomRoutedParams {
            locator,
            rotations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }))
    })?;
    assert!(
        matches!(
            crate::processor::register_network_transport_factory("test-custom-routed", |_| {
                Err(Error::InvalidConfig("hijack".to_string()))
            }),
            Err(Error::InvalidConfig(_))
        ),
        "duplicate custom transport kind registration must be rejected"
    );

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

    let serialized_routed = ProcessorConfigSerialized::try_from(routed_config.clone())?;
    assert_eq!(
        serialized_routed.network.as_ref().map(|d| d.kind.as_str()),
        Some("test-custom-routed")
    );
    let restored_routed = ProcessorConfig::try_from(serialized_routed)?;
    assert_eq!(
        restored_routed.network_parameters().endpoint_hint(),
        Some("wss://node.rings.network/ws")
    );

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

    // Explicit `"webrtc"` network descriptor round-trips as `network: Some({ kind: "webrtc", ... })`
    // without collapsing to flat fields, and preserves `udp_port_range()`!
    let mut webrtc_desc_serialized = ProcessorConfigSerialized::try_from(onion_test_config())?;
    webrtc_desc_serialized.ice_servers.clear();
    webrtc_desc_serialized.external_address = None;
    webrtc_desc_serialized.udp_port_min = None;
    webrtc_desc_serialized.udp_port_max = None;
    webrtc_desc_serialized.network = Some(crate::processor::NetworkConfigDescriptor {
        kind: "webrtc".to_string(),
        params: serde_json::json!({
            "ice_servers": "stun://stun.l.google.com:19302",
            "external_address": "https://node.rings.network:50000",
            "udp_port_min": 50000,
            "udp_port_max": 50100
        }),
    });
    let webrtc_desc_cfg = ProcessorConfig::try_from(webrtc_desc_serialized)?;
    assert_eq!(webrtc_desc_cfg.udp_port_range(), Some(range));
    let reserialized_webrtc_desc = ProcessorConfigSerialized::try_from(webrtc_desc_cfg)?;
    assert_eq!(
        reserialized_webrtc_desc
            .network
            .as_ref()
            .map(|d| d.kind.as_str()),
        Some("webrtc")
    );
    assert!(reserialized_webrtc_desc.ice_servers.is_empty());

    // 4. Attempting to losslessly serialize an unregistered programmatic `NodeNetworkParameters`
    //    implementation (returning `to_network_descriptor = None`) fails explicitly.
    let custom_cfg =
        onion_test_config().with_network_parameters(MinimalAdHocParams { valid: true });
    assert!(matches!(
        ProcessorConfigSerialized::try_from(custom_cfg),
        Err(Error::InvalidConfig(_))
    ));

    Ok(())
}

/// Constructive model-completeness proof for external transport backends (#1291, Impl #6):
/// implements a completely independent in-memory `TransportInterface` + `ConnectionInterface`
/// inside `rings-node` (an external crate relative to `rings-transport`) using **only** public
/// `rings-transport` APIs — with zero delegation to `DummyTransport` or `WebrtcTransport`:
/// 1. Stores `Weak<MemoryConnection>` in the switchboard (no leaked `Arc` cycles on close/drop).
/// 2. Uses `ConnectionStateCell` + `wait_for_data_channel_open` for async channel-open waiting.
/// 3. Implements `handshake_step` and `as_any`.
/// 4. Sends `3 * INBOUND_PEER_FRAME_CAPACITY` (192) frames across a single lane while injecting a
///    transient credit-frame send failure, proving `spawn_credit_driver` + `CreditFrame` RAII
///    requeue replenishes credit windows without deadlock and delivers every payload byte intact.
/// 5. Asserts `NodeFramePool::used_frames() == 0` after `close()`, and verifies end-to-end
///    `Processor` handshake + `Swarm::send_direct_message` application delivery.
#[tokio::test]
async fn test_independent_external_transport_backend_completes_handshake_and_credit_bounded_send(
) -> Result<()> {
    use std::any::Any;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU8;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use std::sync::Weak;

    use async_trait::async_trait;
    use bytes::Bytes;
    use rings_transport::callback::InnerTransportCallback;
    use rings_transport::callback::NodeFramePool;
    use rings_transport::callback::INBOUND_PEER_FRAME_CAPACITY;
    use rings_transport::connection_pool::Pool;
    use rings_transport::connection_ref::ConnectionRef;
    use rings_transport::core::callback::AdmittedInboundMessage;
    use rings_transport::core::callback::BoxedTransportCallback;
    use rings_transport::core::callback::TransportCallback;
    use rings_transport::core::pool::ChannelLane;
    use rings_transport::core::transport::ConnectionInterface;
    use rings_transport::core::transport::ConnectionStateCell;
    use rings_transport::core::transport::ConnectionStateSnapshot;
    use rings_transport::core::transport::HandshakeStep;
    use rings_transport::core::transport::LaneCreditView;
    use rings_transport::core::transport::SendPermit;
    use rings_transport::core::transport::SharedTransport;
    use rings_transport::core::transport::TransportConnectionState;
    use rings_transport::core::transport::TransportInterface;
    use rings_transport::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
    use rings_transport::delivery::DeliveryFuture;
    use rings_transport::delivery::SendCreditWait;
    use rings_transport::error::Error as TransportError;
    use rings_transport::notifier::wait_for_data_channel_open;
    use rings_transport::notifier::Notifier;

    /// Shared in-memory switchboard holding **weak** references to live `MemoryConnection`
    /// endpoints so closing or dropping a connection never leaks an `Arc` cycle.
    #[derive(Clone, Default)]
    struct MemorySwitchboard {
        endpoints: Arc<Mutex<HashMap<String, Weak<MemoryConnection>>>>,
        fail_next_credit_send: Arc<AtomicBool>,
    }

    impl std::fmt::Debug for MemorySwitchboard {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MemorySwitchboard").finish_non_exhaustive()
        }
    }

    impl MemorySwitchboard {
        fn resolve(&self, id: &str) -> Option<Arc<MemoryConnection>> {
            self.endpoints
                .lock()
                .unwrap()
                .get(id)
                .and_then(Weak::upgrade)
        }
    }

    struct MemoryConnection {
        endpoint_id: String,
        switchboard: MemorySwitchboard,
        peer_id: Mutex<Option<String>>,
        stage: AtomicU8,
        state: ConnectionStateCell,
        callback: Arc<InnerTransportCallback>,
        open_notifier: Notifier,
    }

    impl MemoryConnection {
        async fn activate(&self) {
            self.state
                .observe_phase(TransportConnectionState::Connected);
            self.state.observe_channels(true);
            self.open_notifier.wake();
            self.callback
                .on_connection_state_change(TransportConnectionState::Connected)
                .await;
            self.callback.on_channel_open().await;
        }
    }

    #[async_trait]
    impl ConnectionInterface for MemoryConnection {
        type Error = TransportError;

        fn as_any(&self) -> &dyn Any {
            self
        }

        async fn send_message_with_permit(
            &self,
            payload: Bytes,
            lane: ChannelLane,
            permit: SendPermit,
        ) -> std::result::Result<DeliveryFuture, TransportError> {
            let peer_id = self.peer_id.lock().unwrap().clone();
            let switchboard = self.switchboard.clone();
            let state = self.state.clone();
            let cb = Arc::clone(&self.callback);
            self.callback
                .send_custom_wire(
                    payload,
                    lane,
                    permit,
                    self.max_message_size(),
                    move || {
                        state.close();
                        cb.close_link_credit();
                    },
                    move |wire, target_lane, irrevocable| async move {
                        let id = peer_id.ok_or_else(|| {
                            TransportError::ChannelOpen("peer not linked".to_string())
                        })?;
                        let peer = switchboard.resolve(&id).ok_or_else(|| {
                            TransportError::ChannelOpen("peer offline".to_string())
                        })?;
                        let accepted = irrevocable.accept();
                        peer.callback.receive_inbound_frame(wire, target_lane);
                        Ok((
                            accepted,
                            Box::pin(std::future::ready(Ok(()))) as DeliveryFuture,
                        ))
                    },
                )
                .await
        }

        fn reserve_send_credit(&self, lane: ChannelLane) -> SendCreditWait<TransportError> {
            Box::pin(self.callback.reserve_credit(lane))
        }

        fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
            self.callback.send_credit_view(lane)
        }

        fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
            self.state.snapshot()
        }

        fn max_message_size(&self) -> usize {
            MAX_CHANNEL_MESSAGE_SIZE
        }

        async fn handshake_step(
            &self,
            inbound: Option<Bytes>,
        ) -> std::result::Result<HandshakeStep, TransportError> {
            match (self.stage.load(Ordering::Acquire), inbound) {
                (0, None) => {
                    self.stage.store(1, Ordering::Release);
                    self.state.observe_phase(TransportConnectionState::New);
                    Ok(HandshakeStep::Send(Bytes::from(self.endpoint_id.clone())))
                }
                (0, Some(offer_bytes)) => {
                    let remote_id = String::from_utf8(offer_bytes.to_vec()).map_err(|e| {
                        TransportError::RemoteHandshakeApplyError(e.to_string())
                    })?;
                    *self.peer_id.lock().unwrap() = Some(remote_id);
                    self.stage.store(2, Ordering::Release);
                    self.state
                        .observe_phase(TransportConnectionState::Connecting);
                    Ok(HandshakeStep::SendFinal(Bytes::from(self.endpoint_id.clone())))
                }
                (1, Some(answer_bytes)) => {
                    let remote_id = String::from_utf8(answer_bytes.to_vec()).map_err(|e| {
                        TransportError::RemoteHandshakeApplyError(e.to_string())
                    })?;
                    *self.peer_id.lock().unwrap() = Some(remote_id.clone());
                    self.stage.store(2, Ordering::Release);
                    self.activate().await;
                    if let Some(peer) = self.switchboard.resolve(&remote_id) {
                        peer.activate().await;
                    }
                    Ok(HandshakeStep::Done)
                }
                (stage, other) => Err(TransportError::RemoteHandshakeApplyError(format!(
                    "unexpected MemoryConnection handshake transition stage={stage} inbound={other:?}"
                ))),
            }
        }

        async fn wait_for_channel_open(&self) -> std::result::Result<(), TransportError> {
            wait_for_data_channel_open(|| self.state.snapshot(), &self.open_notifier, 5).await
        }

        async fn close(&self) -> std::result::Result<(), TransportError> {
            self.state.close();
            self.open_notifier.wake();
            self.callback.close_link_credit();
            self.switchboard
                .endpoints
                .lock()
                .unwrap()
                .remove(&self.endpoint_id);
            self.callback
                .on_connection_state_change(TransportConnectionState::Closed)
                .await;
            self.callback.on_channel_close().await;
            Ok(())
        }
    }

    struct MemoryTransport {
        switchboard: MemorySwitchboard,
        pool: Pool<MemoryConnection>,
        frame_pool: NodeFramePool,
        seq: std::sync::atomic::AtomicU64,
        prefix: String,
    }

    impl MemoryTransport {
        fn new(switchboard: MemorySwitchboard, prefix: &str) -> Self {
            Self {
                switchboard,
                pool: Pool::new(),
                frame_pool: NodeFramePool::new(),
                seq: std::sync::atomic::AtomicU64::new(0),
                prefix: prefix.to_owned(),
            }
        }
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
            self.pool.ensure_peer_slot_available(cid)?;
            let id = format!(
                "{}-{}",
                self.prefix,
                self.seq.fetch_add(1, Ordering::SeqCst)
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
                stage: AtomicU8::new(0),
                state: ConnectionStateCell::new(),
                callback: Arc::clone(&inner_cb),
                open_notifier: notifier,
            };
            let conn_ref = self.pool.safely_insert(cid, conn).await?;
            let live = conn_ref.upgrade()?;
            let weak_conn = Arc::downgrade(&live);
            self.switchboard
                .endpoints
                .lock()
                .unwrap()
                .insert(id, Weak::clone(&weak_conn));

            let sb = self.switchboard.clone();
            inner_cb.spawn_credit_driver(move |wire, lane| {
                let weak_conn = Weak::clone(&weak_conn);
                let sb = sb.clone();
                async move {
                    let Some(conn) = weak_conn.upgrade() else {
                        return Err(TransportError::ConnectionReleased(
                            "memory connection dropped".to_string(),
                        ));
                    };
                    if sb.fail_next_credit_send.swap(false, Ordering::AcqRel) {
                        return Err(TransportError::Backend {
                            class: rings_transport::error::BackendSendFailureClass::Ambiguous,
                            source: Box::new(std::io::Error::other(
                                "injected transient credit-frame send error",
                            )),
                        });
                    }
                    let Some(peer_id) = conn.peer_id.lock().unwrap().clone() else {
                        return Err(TransportError::ChannelOpen(
                            "peer not yet linked".to_string(),
                        ));
                    };
                    let Some(peer) = sb.resolve(&peer_id) else {
                        return Err(TransportError::ConnectionReleased(
                            "remote memory connection closed".to_string(),
                        ));
                    };
                    peer.callback.receive_inbound_frame(wire, lane);
                    Ok(())
                }
            })?;
            drop(live);
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

    // Part A: Direct multi-window transfer (`3 * INBOUND_PEER_FRAME_CAPACITY` = 192 frames) with
    // transient credit-send failure injection, payload byte verification, weak switchboard cleanup,
    // and `NodeFramePool::used_frames() == 0` after close!
    struct RecordingCallback {
        received: Arc<Mutex<Vec<Bytes>>>,
        notify: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl TransportCallback for RecordingCallback {
        async fn on_admitted_message(
            &self,
            message: AdmittedInboundMessage<'_>,
        ) -> std::result::Result<(), Box<dyn std::error::Error>> {
            let (_cid, payload, _permit) = message.into_parts();
            self.received.lock().unwrap().push(payload);
            self.notify.notify_waiters();
            Ok(())
        }
    }

    let direct_sb = MemorySwitchboard::default();
    let t_sender = MemoryTransport::new(direct_sb.clone(), "raw-tx");
    let t_receiver = MemoryTransport::new(direct_sb.clone(), "raw-rx");
    let received_payloads = Arc::new(Mutex::new(Vec::new()));
    let rx_notify = Arc::new(tokio::sync::Notify::new());

    let c_tx = t_sender
        .new_connection(
            "peer-rx",
            Box::new(RecordingCallback {
                received: Arc::new(Mutex::new(Vec::new())),
                notify: Arc::new(tokio::sync::Notify::new()),
            }),
        )
        .await
        .unwrap();
    let c_rx = t_receiver
        .new_connection(
            "peer-tx",
            Box::new(RecordingCallback {
                received: Arc::clone(&received_payloads),
                notify: Arc::clone(&rx_notify),
            }),
        )
        .await
        .unwrap();

    let HandshakeStep::Send(offer_bytes) = c_tx.handshake_step(None).await.unwrap() else {
        panic!("expected step 0 Send");
    };
    let HandshakeStep::SendFinal(answer_bytes) =
        c_rx.handshake_step(Some(offer_bytes)).await.unwrap()
    else {
        panic!("expected step 1 SendFinal");
    };
    let HandshakeStep::Done = c_tx.handshake_step(Some(answer_bytes)).await.unwrap() else {
        panic!("expected step 2 Done");
    };
    c_tx.wait_for_channel_open().await.unwrap();
    c_rx.wait_for_channel_open().await.unwrap();

    let data_lane = ChannelLane::new(1);
    let total_frames = 3 * INBOUND_PEER_FRAME_CAPACITY;
    let mut expected_payloads = Vec::with_capacity(total_frames);
    for idx in 0..total_frames {
        if idx == INBOUND_PEER_FRAME_CAPACITY {
            // Arm a transient credit-frame failure right as window replenishment is needed!
            direct_sb
                .fail_next_credit_send
                .store(true, Ordering::Release);
        }
        let body = Bytes::from(format!("external-frame-{idx:04}"));
        expected_payloads.push(body.clone());
        let credit =
            tokio::time::timeout(Duration::from_secs(5), c_tx.reserve_send_credit(data_lane))
                .await
                .expect("credit pump must replenish window after transient failure")
                .unwrap();
        let delivery = c_tx
            .send_message_with_permit(body, data_lane, SendPermit::always().with_credit(credit))
            .await
            .unwrap();
        delivery.await.unwrap();
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        while received_payloads.lock().unwrap().len() < total_frames {
            rx_notify.notified().await;
        }
    })
    .await
    .expect("all 192 frames delivered to receiver callback");
    assert_eq!(*received_payloads.lock().unwrap(), expected_payloads);
    assert!(
        !direct_sb.fail_next_credit_send.load(Ordering::Acquire),
        "injected transient credit failure must have fired and recovered"
    );

    t_sender.close_connection_if_current(&c_tx).await.unwrap();
    t_receiver.close_connection_if_current(&c_rx).await.unwrap();
    drop(c_tx);
    drop(c_rx);
    assert_eq!(t_sender.frame_pool.used_frames(), 0);
    assert_eq!(t_receiver.frame_pool.used_frames(), 0);
    assert!(
        direct_sb.endpoints.lock().unwrap().is_empty(),
        "weak switchboard must hold zero endpoints after close"
    );

    // Part B: Full two-`Processor` handshake and `Swarm::send_direct_message` application delivery!
    #[derive(Clone, Debug)]
    struct MemoryNetworkParams {
        switchboard: MemorySwitchboard,
        prefix: String,
    }

    impl NodeNetworkParameters for MemoryNetworkParams {
        fn build_transport(&self, _delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
            Ok(Arc::new(MemoryTransport::new(
                self.switchboard.clone(),
                &self.prefix,
            )))
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
    let p2_inbox = p2.swarm.application_inbox().map_err(Error::CoreError)?;

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

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if p1.swarm.is_peer_admitted(p2.did()).unwrap_or(false)
                && p2.swarm.is_peer_admitted(p1.did()).unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("p1 and p2 must admit each other over MemoryTransport");

    let app_payload =
        rings_core::swarm::application::ApplicationData::custom(b"hello-from-external-transport");
    p1.swarm
        .send_direct_message(app_payload.clone(), expected_peer)
        .await
        .map_err(Error::CoreError)?;

    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(delivery) = p2_inbox.next().await {
            if let rings_core::swarm::application::ApplicationDelivery::Message(msg) = delivery {
                assert_eq!(msg.from, p1.did());
                assert_eq!(msg.data, app_payload);
                return;
            }
        }
        panic!("p2 application inbox closed before delivering direct message");
    })
    .await
    .expect("p2 application inbox must receive p1 direct message");

    p1.close().await?;
    p2.close().await?;
    Ok(())
}
