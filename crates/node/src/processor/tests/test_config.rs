use rings_core::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use rings_core::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;

use super::common::*;
use super::*;
use crate::processor::config::parse_webrtc_udp_port_range;

#[test]
fn webrtc_udp_port_range_absent_by_default() {
    let range = parse_webrtc_udp_port_range(None, None);

    assert!(matches!(range, core::result::Result::Ok(None)));
}

#[test]
fn webrtc_udp_port_range_accepts_valid_bounds() {
    let range = parse_webrtc_udp_port_range(Some(49160), Some(49200));

    assert!(matches!(
        range,
        Ok(Some(range)) if range.min() == 49160 && range.max() == 49200
    ));
}

#[test]
fn webrtc_udp_port_range_rejects_partial_bounds() {
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
fn webrtc_udp_port_range_rejects_zero_bound() {
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
fn webrtc_udp_port_range_rejects_inverted_bounds() {
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
fn online_node_timing_requires_heartbeat_interval_less_than_ttl_when_enabled() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    )
    .online_node_heartbeat_interval_secs(90)
    .online_node_ttl_secs(30);

    assert!(matches!(
        ProcessorConfig::try_from(serialized),
        Err(Error::InvalidConfig(message))
            if message.contains("online_node_heartbeat_interval")
                && message.contains("online_node_ttl")
    ));
}

#[test]
fn presence_advertisement_can_be_disabled() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    )
    .online_node_heartbeat_interval_secs(90)
    .online_node_ttl_secs(30)
    .advertise_presence(false);

    let config = ProcessorConfig::try_from(serialized).unwrap();
    let builder = ProcessorBuilder::from_config(&config).unwrap();

    assert!(!builder.advertise_presence);
    assert!(builder.registration_tasks.is_empty());
}

#[test]
fn presence_advertisement_is_enabled_by_default() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    );

    let config = ProcessorConfig::try_from(serialized).unwrap();
    let builder = ProcessorBuilder::from_config(&config).unwrap();

    assert!(builder.advertise_presence);
    assert_eq!(
        builder.dht_virtual_nodes,
        DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
}

#[test]
fn dht_virtual_nodes_rejects_values_above_cost_bound() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    )
    .dht_virtual_nodes(MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER.saturating_add(1));

    assert!(matches!(
        ProcessorConfig::try_from(serialized),
        Err(Error::InvalidConfig(message))
            if message.contains("dht_virtual_nodes")
                && message.contains(&MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER.to_string())
    ));
}

#[test]
fn serialized_processor_config_defaults_dht_virtual_nodes() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let yaml = format!(
        r#"
network_id: 0
ice_servers: stun://stun.l.google.com:19302
external_address: null
webrtc_udp_port_min: null
webrtc_udp_port_max: null
session_sk: "{}"
stabilize_interval: 3
online_node_heartbeat_interval_secs: 30
online_node_ttl_secs: 60
online_node_type: Native
advertise_presence: true
"#,
        session_sk.dump().unwrap()
    );

    let serialized = serde_yaml::from_str::<ProcessorConfigSerialized>(&yaml).unwrap();
    let config = ProcessorConfig::try_from(serialized).unwrap();
    let builder = ProcessorBuilder::from_config(&config).unwrap();

    assert_eq!(
        builder.dht_virtual_nodes,
        DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
}

#[test]
fn onion_relay_requires_presence_advertisement() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    )
    .advertise_presence(false)
    .advertise_onion_relay(true);

    assert!(matches!(
        ProcessorConfig::try_from(serialized),
        Err(Error::InvalidConfig(message))
            if message.contains("advertise_onion_relay")
                && message.contains("advertise_presence")
    ));
}

#[test]
fn advertised_onion_exit_requires_open_policy() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    )
    .advertise_onion_exit(true);

    assert!(matches!(
        ProcessorConfig::try_from(serialized),
        Err(Error::InvalidConfig(message)) if message.contains("allowed target")
    ));
}

#[test]
fn onion_exit_registration_task_can_run_without_presence_advertisement() -> Result<()> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let serialized = ProcessorConfigSerialized::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk.dump().unwrap(),
        3,
    )
    .advertise_presence(false)
    .advertise_onion_exit(true)
    .onion_exit_policy(onion_policy(&["example.com:443"], &[])?);

    let config = ProcessorConfig::try_from(serialized).unwrap();
    let processor = ProcessorBuilder::from_config(&config)
        .unwrap()
        .storage(Box::new(MemStorage::new()))
        .dht_finger_table_size(8)
        .build()
        .unwrap();

    assert_eq!(processor.registration_tasks.len(), 1);
    Ok(())
}

#[tokio::test]
async fn onion_relay_capability_is_advertised_in_online_descriptor() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    )
    .advertise_onion_relay(true);

    let processor = ProcessorBuilder::from_config(&config)
        .unwrap()
        .storage(Box::new(MemStorage::new()))
        .dht_finger_table_size(8)
        .build()
        .unwrap();
    let descriptor = processor.online_node_descriptor_at(get_epoch_ms()).unwrap();

    assert!(descriptor
        .capabilities
        .iter()
        .any(|capability| capability == ONION_RELAY_CAPABILITY));
}

#[test]
fn https_onion_exit_config_uses_https_only_service() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    )
    .enable_https_onion_exit();

    assert!(config.advertise_onion_exit);
    assert_eq!(config.onion_exit_services, https_onion_exit_services());
}

#[test]
fn default_onion_exit_config_uses_native_tcp_service() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    )
    .enable_default_onion_exit();

    assert!(config.advertise_onion_exit);
    assert_eq!(config.onion_exit_services, default_onion_exit_services());
    assert_eq!(config.onion_exit_services, vec![OnionExitService::tcp()]);
}

#[test]
fn reserved_onion_exit_service_rejects_wrong_transport() {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let mut config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    )
    .advertise_onion_exit(true);
    config.onion_exit_services =
        vec![OnionExitService::new("https", OnionExitTransport::Tcp).expect("valid service")];

    assert!(matches!(
        ProcessorBuilder::from_config(&config),
        Err(Error::InvalidConfig(message))
            if message.contains("https")
                && message.contains("Https")
                && message.contains("Tcp")
    ));
}

#[test]
fn custom_onion_exit_service_allows_explicit_transport() -> Result<()> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    let mut config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_string(),
        session_sk,
        3,
    )
    .advertise_onion_exit(true);
    config.onion_exit_services = vec![OnionExitService::new("web", OnionExitTransport::Tcp)?];
    config.onion_exit_policy = onion_policy(&["example.com:443"], &[])?;

    assert!(ProcessorBuilder::from_config(&config).is_ok());
    Ok(())
}
