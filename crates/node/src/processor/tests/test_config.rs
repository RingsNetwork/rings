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

/// Verifies model completeness of [`NetworkParameters`] and [`NodeNetworkParameters`] across
/// 6 heterogeneous transport media (Packet Radio, Bluetooth Ad-Hoc, Ultrasonic Ad-Hoc,
/// Traditional TCP/IP, HTTPS/WebSocket, and Assisted Traversal), as well as rejection of illegal
/// kinematic profiles (e.g. multi-lane half-duplex carrier collision) at [`ProcessorBuilder::build`].
#[tokio::test]
async fn test_custom_network_parameters_trait_supports_heterogeneous_media_and_enforces_kinematic_laws(
) -> Result<()> {
    /// Universal parameter descriptor used to model heterogeneous transport media in tests.
    #[derive(Clone, Debug)]
    struct HeterogeneousMediumParams {
        /// Canonical transport medium identifier.
        kind: &'static str,
        /// Physical and link-layer kinematic profile of the medium.
        kinematics: TransportKinematics,
        /// Optional advertised external endpoint locator or frequency/channel descriptor.
        endpoint_hint: Option<String>,
        /// Optional session-credential template for relay-assisted media (`None` for ad-hoc/direct).
        session_template: Option<String>,
        /// Medium-specific key-value parameters (e.g. modulation, carrier frequency, L2CAP PSM).
        params: std::collections::BTreeMap<String, String>,
    }

    impl NetworkParameters for HeterogeneousMediumParams {
        /// Return the medium's canonical identifier.
        fn kind(&self) -> &'static str {
            self.kind
        }

        /// Return the medium's kinematic profile.
        fn kinematics(&self) -> TransportKinematics {
            self.kinematics
        }

        /// Return the medium's endpoint locator hint, if any.
        fn endpoint_hint(&self) -> Option<&str> {
            self.endpoint_hint.as_deref()
        }

        /// Return the medium's session credential template, if any.
        fn session_credential_template(&self) -> Option<&str> {
            self.session_template.as_deref()
        }

        /// Return the medium's custom key-value parameter dictionary.
        fn parameter_map(&self) -> std::collections::BTreeMap<String, String> {
            self.params.clone()
        }

        /// Validate the medium's kinematic profile and instantiate a [`SharedTransport`].
        fn build_transport(
            &self,
            initial_session_credential: Option<&str>,
        ) -> std::result::Result<
            rings_transport::core::transport::SharedTransport,
            TransportParameterError,
        > {
            NetworkParameters::validate_parameters(self)?;
            NetworkParameters::build_transport(
                &TransportNetworkConfig::new(
                    initial_session_credential.unwrap_or(""),
                    self.endpoint_hint.clone(),
                    None,
                ),
                initial_session_credential,
            )
        }
    }

    let media = [
        // 1. Packet Radio (HF/VHF AX.25 / LoRa half-duplex ad-hoc)
        HeterogeneousMediumParams {
            kind: "packet-radio",
            kinematics: TransportKinematics::packet_radio(),
            endpoint_hint: Some("ax25://N0CALL-1@144.390MHz".to_string()),
            session_template: None,
            params: std::collections::BTreeMap::from([
                ("modulation".to_string(), "LoRa-SF9-BW125".to_string()),
                ("carrier_hz".to_string(), "433920000".to_string()),
            ]),
        },
        // 2. Bluetooth Ad-Hoc (BLE L2CAP CoC proximity mesh)
        HeterogeneousMediumParams {
            kind: "bluetooth-adhoc",
            kinematics: TransportKinematics::bluetooth_adhoc(),
            endpoint_hint: Some("ble://psm:0x0080".to_string()),
            session_template: None,
            params: std::collections::BTreeMap::from([(
                "l2cap_psm".to_string(),
                "128".to_string(),
            )]),
        },
        // 3. Ultrasonic Ad-Hoc (half-duplex acoustic air medium, no external endpoint advertisement)
        HeterogeneousMediumParams {
            kind: "ultrasonic-adhoc",
            kinematics: TransportKinematics::ultrasonic_adhoc(),
            endpoint_hint: None,
            session_template: None,
            params: std::collections::BTreeMap::from([
                ("carrier_khz".to_string(), "18.5".to_string()),
                ("modulation".to_string(), "OFDM-acoustic".to_string()),
            ]),
        },
        // 4. Traditional TCP/IP (direct routed full-duplex socket transport)
        HeterogeneousMediumParams {
            kind: "tcp-ip",
            kinematics: TransportKinematics::low_latency_routed(),
            endpoint_hint: Some("tcp://198.51.100.7:9443".to_string()),
            session_template: None,
            params: std::collections::BTreeMap::from([(
                "listen_addr".to_string(),
                "0.0.0.0:9443".to_string(),
            )]),
        },
        // 5. HTTPS / WebSocket (routed full-duplex offer/answer over TLS)
        HeterogeneousMediumParams {
            kind: "https-ws",
            kinematics: TransportKinematics {
                handshake: HandshakeSemantic::OfferAnswer,
                ..TransportKinematics::low_latency_routed()
            },
            endpoint_hint: Some("wss://node.rings.network/ws".to_string()),
            session_template: None,
            params: std::collections::BTreeMap::from([(
                "alpn".to_string(),
                "h2,http/1.1".to_string(),
            )]),
        },
    ];

    for medium in media {
        let expected_kind = medium.kind;
        let expected_kinematics = medium.kinematics;
        let expected_endpoint = medium.endpoint_hint.clone();
        let expected_params = medium.params.clone();

        let config = onion_test_config().with_network_parameters(medium);
        let processor = test_processor_builder(&config).build()?;

        assert_eq!(processor.network_parameters().kind(), expected_kind);
        assert_eq!(
            processor.network_parameters().kinematics(),
            expected_kinematics
        );
        assert_eq!(
            processor.network_parameters().endpoint_hint(),
            expected_endpoint.as_deref()
        );
        assert_eq!(
            processor.network_parameters().parameter_map(),
            expected_params
        );
        assert_eq!(
            processor
                .network_parameters()
                .derive_session_credential(&processor.delegatee_key())?,
            None,
            "proximity/routed media without relay template must not require ICE/TURN credentials"
        );

        // Session key renewal succeeds cleanly even when the medium has no external relay servers.
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
        let mut cfg_for_renew = ProcessorConfig::new(0, String::new(), initial_key, 3)
            .with_network_parameters(HeterogeneousMediumParams {
                kind: expected_kind,
                kinematics: expected_kinematics,
                endpoint_hint: expected_endpoint,
                session_template: None,
                params: expected_params,
            });
        cfg_for_renew.advertise_presence = false;
        let proc_for_renew = test_processor_builder(&cfg_for_renew).build_at(t1)?;
        assert!(proc_for_renew
            .renew_delegatee_key(renewed_key, t1 + SIGNING_HORIZON_LEAD_MS)
            .is_ok());
    }

    // Standard assisted-traversal config preserves UDP port range and endpoint hint in parameter_map.
    let range = UdpPortRange::new(50000, 50100)?;
    let traversal = TransportNetworkConfig::new(
        "stun://stun.l.google.com:19302",
        Some("https://node.rings.network:50000".to_string()),
        Some(range),
    );
    let config = onion_test_config().with_network_parameters(traversal);
    assert_eq!(config.udp_port_range(), Some(range));
    let processor = test_processor_builder(&config).build()?;
    assert_eq!(processor.network_parameters().kind(), "assisted-traversal");
    assert_eq!(
        processor.session_transport_credentials().as_deref(),
        Some("stun://stun.l.google.com:19302")
    );

    // Invalid kinematics (e.g. half-duplex ultrasonic medium claiming 2 simultaneous lanes)
    // is rejected at `ProcessorBuilder::build`.
    let invalid_ultrasonic = HeterogeneousMediumParams {
        kind: "ultrasonic-adhoc",
        kinematics: TransportKinematics {
            ordered_lanes: 2,
            ..TransportKinematics::ultrasonic_adhoc()
        },
        endpoint_hint: None,
        session_template: None,
        params: std::collections::BTreeMap::new(),
    };
    let bad_config = onion_test_config().with_network_parameters(invalid_ultrasonic);
    assert!(matches!(
        test_processor_builder(&bad_config).build(),
        Err(Error::InvalidConfig(msg)) if msg.contains("half-duplex")
    ));

    Ok(())
}
