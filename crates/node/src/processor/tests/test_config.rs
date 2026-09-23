use rings_core::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::message::OriginQuotaConfig;
use rings_core::message::OriginQuotaLaneConfig;

use super::common::*;
use super::*;
use crate::processor::config::parse_webrtc_udp_port_range;

#[test]
fn test_webrtc_udp_port_range_absent_by_default() {
    let range = parse_webrtc_udp_port_range(None, None);

    assert!(matches!(range, core::result::Result::Ok(None)));
}

#[test]
fn test_webrtc_udp_port_range_accepts_valid_bounds() {
    let range = parse_webrtc_udp_port_range(Some(49160), Some(49200));

    assert!(matches!(
        range,
        Ok(Some(range)) if range.min() == 49160 && range.max() == 49200
    ));
}

#[test]
fn test_webrtc_udp_port_range_rejects_partial_bounds() {
    let range = parse_webrtc_udp_port_range(Some(49160), None);

    assert!(matches!(
        range,
        Err(Error::IncompleteWebrtcUdpPortRange {
            min: Some(49160),
            max: None
        })
    ));
}

#[test]
fn test_webrtc_udp_port_range_rejects_zero_bound() {
    let range = parse_webrtc_udp_port_range(Some(0), Some(49200));

    assert!(matches!(
        range,
        Err(Error::InvalidWebrtcUdpPortRange(
            rings_transport::webrtc_config::WebrtcUdpPortRangeError::ZeroBound {
                min: 0,
                max: 49200
            }
        ))
    ));
}

#[test]
fn test_webrtc_udp_port_range_rejects_inverted_bounds() {
    let range = parse_webrtc_udp_port_range(Some(49200), Some(49160));

    assert!(matches!(
        range,
        Err(Error::InvalidWebrtcUdpPortRange(
            rings_transport::webrtc_config::WebrtcUdpPortRangeError::Inverted {
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
webrtc_udp_port_min: null
webrtc_udp_port_max: null
delegatee_key: "{}"
stabilize_interval: 15
online_node_heartbeat_interval_secs: 30
online_node_ttl_secs: 60
online_node_type: Native
advertise_presence: true
"#,
        delegatee_key.dump().unwrap()
    );

    let serialized = serde_yaml::from_str::<ProcessorConfigSerialized>(&yaml).unwrap();
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

#[test]
fn test_onion_relay_requires_presence_advertisement() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let mut config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .advertise_onion_relay(true);
    config.advertise_presence = false;

    assert!(matches!(
        ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build),
        Err(Error::InvalidConfig(message))
            if message.contains("advertise_onion_relay")
                && message.contains("advertise_presence")
    ));
}

#[test]
fn test_advertised_onion_exit_requires_open_policy() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .advertise_onion_relay(true)
    .advertise_onion_exit(true);

    assert!(matches!(
        ProcessorBuilder::from_config(&config).and_then(ProcessorBuilder::build),
        Err(Error::InvalidConfig(message)) if message.contains("allowed target")
    ));
}

/// Registering any symbol registers `relay` (#834 D2): an exit without the relay capability is
/// rejected at configuration, and so, through `relay ⇒ presence`, is an exit without presence.
#[test]
fn test_onion_exit_registration_requires_relay_registration() -> Result<()> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let exit_only = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .advertise_onion_exit(true)
    .onion_exit_policy(onion_policy(&["example.com:443"], &[])?);
    let mut without_presence = exit_only.clone().advertise_onion_relay(true);
    without_presence.advertise_presence = false;

    assert!(matches!(
        ProcessorBuilder::from_config(&exit_only).and_then(ProcessorBuilder::build),
        Err(Error::InvalidConfig(message))
            if message.contains("advertise_onion_exit")
                && message.contains("advertise_onion_relay")
    ));
    assert!(matches!(
        ProcessorBuilder::from_config(&without_presence).and_then(ProcessorBuilder::build),
        Err(Error::InvalidConfig(message)) if message.contains("advertise_presence")
    ));
    Ok(())
}

/// The relay capability carries the process epoch `e_n`, and every build (process start) draws a
/// fresh one (#834 D2).
#[tokio::test]
async fn test_onion_relay_capability_carries_a_fresh_process_epoch() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .advertise_onion_relay(true);
    let start = || {
        ProcessorBuilder::from_config(&config)
            .unwrap()
            .storage(Box::new(MemStorage::new()))
            .dht_finger_table_size(8)
            .build()
            .unwrap()
    };
    let processor = start();
    let restarted = start();
    let descriptor = processor.online_node_descriptor_at(get_epoch_ms()).unwrap();

    assert_eq!(
        descriptor.capabilities,
        OnlineNodeCapabilities::onion_relay(processor.onion_process_epoch())
    );
    assert_ne!(
        processor.onion_process_epoch(),
        restarted.onion_process_epoch()
    );
}

#[test]
fn test_https_onion_exit_config_uses_https_only_service() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .enable_https_onion_exit();

    assert!(config.advertise_onion_relay);
    assert!(config.advertise_onion_exit);
    assert_eq!(config.onion_exit_services, https_onion_exit_services());
}

#[test]
fn test_default_onion_exit_config_uses_native_tcp_backed_services() {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .enable_default_onion_exit();

    assert!(config.advertise_onion_relay);
    assert!(config.advertise_onion_exit);
    assert_eq!(config.onion_exit_services, default_onion_exit_services());
    assert_eq!(config.onion_exit_services, vec![
        OnionServiceName::tcp(),
        OnionServiceName::https()
    ]);
}

/// The reserved HTTPS name is valid without a parallel transport discriminator.
#[test]
fn test_reserved_https_onion_exit_service_is_accepted() -> Result<()> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let mut config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        delegatee_key,
        3,
    )
    .advertise_onion_relay(true)
    .advertise_onion_exit(true);
    config.onion_exit_services = vec![OnionServiceName::https()];
    config.onion_exit_policy = onion_policy(&["example.com:443"], &[])?;

    assert!(ProcessorBuilder::from_config(&config)
        .and_then(ProcessorBuilder::build)
        .is_ok());
    Ok(())
}

/// An exit registers world-facing symbols of the closed signature only: neither names outside
/// `Σ` nor the identity symbol `relay` parse as an exit service, in code or in a config file.
#[test]
fn test_onion_exit_service_must_be_a_world_facing_symbol() {
    assert_eq!(
        serde_yaml::from_str::<Vec<OnionServiceName>>("[tcp, https]").ok(),
        Some(vec![OnionServiceName::tcp(), OnionServiceName::https()])
    );
    for outside in ["web", "relay"] {
        assert!(OnionServiceName::parse(outside).is_err());
        assert!(serde_yaml::from_str::<Vec<OnionServiceName>>(&format!("[{outside}]")).is_err());
    }
}
