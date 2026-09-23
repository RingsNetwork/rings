use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;
use rings_core::message::MessageSigner;

use super::super::*;
use crate::tests::TEST_NETWORK_ID;

/// Stable epoch used by signed descriptor fixtures; its concrete byte value is not semantic.
const TEST_PROCESS_EPOCH: OnionExitEpoch = OnionExitEpoch::new([31; 16]);
/// Distinct epoch used to witness that epoch substitution invalidates the descriptor signature.
const TAMPERED_PROCESS_EPOCH: OnionExitEpoch = OnionExitEpoch::new([32; 16]);

fn signed_exit_at(heartbeat_at_ms: u128, expires_at_ms: u128) -> Result<OnionExitDescriptor> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).map_err(Error::CoreError)?;
    signed_exit_for_session_at(
        &delegatee_key,
        OnionServiceName::tcp(),
        heartbeat_at_ms,
        expires_at_ms,
        "test",
    )
}

fn signed_exit_for_session_at(
    delegatee_key: &DelegateeKey,
    service: OnionServiceName,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
    version: &str,
) -> Result<OnionExitDescriptor> {
    let did = delegatee_key.delegator_did();
    OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did,
            public_key: delegatee_key
                .delegation()
                .delegator_verification_pubkey()
                .map_err(Error::CoreError)?,
            delegatee_public_key: delegatee_key.delegatee_public_key(),
            process_epoch: TEST_PROCESS_EPOCH,
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service,
            policy: OnionExitPolicy {
                allowed_targets: vec![OnionExitTarget::parse("example.com:443")?],
                denied_targets: vec![],
                max_circuits: 16,
                max_streams_per_circuit: 4,
                max_bytes_per_minute: 1024,
            },
            started_at_ms: 1,
            heartbeat_at_ms,
            expires_at_ms,
            version: version.to_string(),
        },
        MessageSigner::new(delegatee_key, TEST_NETWORK_ID),
    )
    .map_err(Error::CoreError)
}

#[test]
fn test_default_exit_services_include_native_tcp_and_https() {
    assert_eq!(default_onion_exit_services(), vec![
        OnionServiceName::tcp(),
        OnionServiceName::https()
    ]);
    assert_eq!(https_onion_exit_services(), vec![OnionServiceName::https()]);
}

/// Reserved service names match only their canonical service identity.
#[test]
fn test_reserved_service_names_match_routes() {
    assert!(OnionServiceName::https().matches("https"));
    assert!(OnionServiceName::tcp().matches("tcp"));
    assert!(OnionServiceName::https().matches("HTTPS"));
}

#[test]
fn test_onion_exit_service_name_is_validated_and_canonicalized() -> Result<()> {
    let service = OnionServiceName::parse("TcP")?;

    assert_eq!(service.as_str(), "tcp");
    assert!(OnionServiceName::parse("").is_err());
    assert!(OnionServiceName::parse(" tcp").is_err());
    assert!(OnionServiceName::parse("web").is_err());
    Ok(())
}

#[test]
fn test_default_exit_policy_is_closed() -> Result<()> {
    let policy = OnionExitPolicy::default();
    let target = OnionExitTarget::parse("example.com:443")?;

    assert!(policy.is_closed());
    assert!(!policy.allows_target(&target));
    assert!(matches!(
        policy.validate_targets(),
        Err(Error::InvalidConfig(message)) if message.contains("allowed target")
    ));
    Ok(())
}

#[test]
fn test_exit_policy_allow_list_controls_targets() -> Result<()> {
    let policy = OnionExitPolicy::from_target_strings(
        vec![
            "Example.COM.:443".to_string(),
            "API.example.com:443".to_string(),
        ],
        vec!["api.example.com:443".to_string()],
    )?;
    let example = OnionExitTarget::parse("example.com:443")?;
    let api = OnionExitTarget::parse("api.example.com:443")?;
    let other = OnionExitTarget::parse("other.example.com:443")?;

    assert!(!policy.is_closed());
    assert!(policy.allows_target(&example));
    assert!(!policy.allows_target(&api));
    assert!(!policy.allows_target(&other));
    Ok(())
}

#[test]
fn test_exit_policy_wildcard_allows_all_targets_with_specific_denies() -> Result<()> {
    let policy = OnionExitPolicy::from_target_strings(vec!["*:*".to_string()], vec![
        "api.example.com:443".to_string(),
    ])?;
    let google = OnionExitTarget::parse("google.com:443")?;
    let example = OnionExitTarget::parse("example.com:8443")?;
    let api = OnionExitTarget::parse("api.example.com:443")?;

    assert!(!policy.is_closed());
    assert!(policy.allows_target(&google));
    assert!(policy.allows_target(&example));
    assert!(!policy.allows_target(&api));
    Ok(())
}

#[test]
fn test_exit_policy_rejects_invalid_target_entries() {
    assert!(matches!(
        OnionExitPolicy::from_target_strings(vec!["example.com".to_string()], vec![]),
        Err(Error::InvalidConfig(message)) if message.contains("expected host:port")
    ));

    assert!(matches!(
        OnionExitPolicy::from_target_strings(
            vec!["example.com:443".to_string()],
            vec!["blocked.example.com".to_string()]
        ),
        Err(Error::InvalidConfig(message)) if message.contains("expected host:port")
    ));
}

#[test]
fn test_exit_descriptor_signature_covers_policy() -> Result<()> {
    let mut descriptor = signed_exit_at(20, 100)?;
    assert!(descriptor.verify_signature(TEST_NETWORK_ID));

    descriptor.policy.max_circuits = 32;

    assert!(!descriptor.verify_signature(TEST_NETWORK_ID));
    Ok(())
}

/// The process epoch is signed, so a registry observer cannot substitute another
/// replay-admission generation while preserving the descriptor signature.
#[test]
fn test_exit_descriptor_signature_covers_process_epoch() -> Result<()> {
    let mut descriptor = signed_exit_at(20, 100)?;
    assert!(descriptor.verify_signature(TEST_NETWORK_ID));

    descriptor.process_epoch = TAMPERED_PROCESS_EPOCH;

    assert!(!descriptor.verify_signature(TEST_NETWORK_ID));
    Ok(())
}

#[test]
fn test_latest_valid_by_service_did_filters_expired_and_keeps_newest() -> Result<()> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).map_err(Error::CoreError)?;
    let did = delegatee_key.delegator_did();
    let public_key = delegatee_key
        .delegation()
        .delegator_verification_pubkey()
        .map_err(Error::CoreError)?;

    let older = OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did,
            public_key: public_key.clone(),
            delegatee_public_key: delegatee_key.delegatee_public_key(),
            process_epoch: TEST_PROCESS_EPOCH,
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service: OnionServiceName::tcp(),
            policy: OnionExitPolicy::default(),
            started_at_ms: 1,
            heartbeat_at_ms: 10,
            expires_at_ms: 100,
            version: "old".to_string(),
        },
        MessageSigner::new(&delegatee_key, TEST_NETWORK_ID),
    )
    .map_err(Error::CoreError)?;
    let newer = OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did,
            public_key,
            delegatee_public_key: delegatee_key.delegatee_public_key(),
            process_epoch: TEST_PROCESS_EPOCH,
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service: OnionServiceName::tcp(),
            policy: OnionExitPolicy::default(),
            started_at_ms: 1,
            heartbeat_at_ms: 20,
            expires_at_ms: 100,
            version: "new".to_string(),
        },
        MessageSigner::new(&delegatee_key, TEST_NETWORK_ID),
    )
    .map_err(Error::CoreError)?;
    let other_live = signed_exit_at(25, 100)?;
    let expired = signed_exit_at(30, 40)?;

    let descriptors = OnionExitDescriptor::latest_valid_by_service_did(
        vec![
            older.clone(),
            newer.clone(),
            other_live.clone(),
            expired.clone(),
        ],
        50,
        TEST_NETWORK_ID,
        false,
    );

    assert_eq!(descriptors.len(), 2);
    assert!(descriptors.iter().any(|descriptor| descriptor == &newer));
    assert!(descriptors
        .iter()
        .any(|descriptor| descriptor == &other_live));

    let with_expired = OnionExitDescriptor::latest_valid_by_service_did(
        vec![older, newer, other_live, expired],
        50,
        TEST_NETWORK_ID,
        true,
    );
    assert_eq!(with_expired.len(), 3);
    Ok(())
}

#[test]
fn test_latest_valid_by_service_did_preserves_same_did_distinct_services() -> Result<()> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).map_err(Error::CoreError)?;
    let old_tcp =
        signed_exit_for_session_at(&delegatee_key, OnionServiceName::tcp(), 10, 100, "tcp-old")?;
    let new_tcp =
        signed_exit_for_session_at(&delegatee_key, OnionServiceName::tcp(), 20, 100, "tcp-new")?;
    let https =
        signed_exit_for_session_at(&delegatee_key, OnionServiceName::https(), 15, 100, "https")?;

    let descriptors = OnionExitDescriptor::latest_valid_by_service_did(
        vec![old_tcp, new_tcp.clone(), https.clone()],
        50,
        TEST_NETWORK_ID,
        false,
    );

    assert_eq!(descriptors.len(), 2);
    assert!(descriptors.iter().any(|descriptor| descriptor == &new_tcp));
    assert!(descriptors.iter().any(|descriptor| descriptor == &https));
    Ok(())
}

/// The signature is closed at decode: a descriptor naming a service outside `Σ` never decodes,
/// whatever its signature, while its well-formed original round-trips.
#[test]
fn test_descriptor_naming_a_service_outside_the_signature_is_rejected_at_decode() -> Result<()> {
    let descriptor = signed_exit_at(10, 100)?;
    let encoded = rings_codec::serialize(&descriptor).map_err(|_| Error::EncodeError)?;
    let service_offset = encoded
        .windows(4)
        .position(|window| window == b"\x03tcp")
        .expect("encoded tcp service name");
    let mut outside = encoded.clone();
    outside
        .get_mut(service_offset..service_offset + 4)
        .expect("service name bytes")
        .copy_from_slice(b"\x03web");

    assert_eq!(
        rings_codec::deserialize::<OnionExitDescriptor>(encoded.as_slice()).ok(),
        Some(descriptor)
    );
    assert!(rings_codec::deserialize::<OnionExitDescriptor>(outside.as_slice()).is_err());
    Ok(())
}
