//! Golden vectors of the `rings-node` wire, pinned together with [`WIRE_VERSION`] (#1141): the
//! sibling of `rings-core`'s `message::payload::test_wire_goldens` (#1107).
//!
//! ```text
//!   goldens = formats             the serde format of every node wire root and nested enum
//!           ∪ descriptors         signed DHT descriptors, signing transcripts, and topic names
//!           ∪ mail                sealed mail manifest, chunk, digest, and AEAD associated data
//!           ∪ extensions          extension Envelope, every Frame variant, every RelayCommand variant
//!           ∪ onion               KDF salts/infos, derived carry seeds/slots, loop classes, symbols,
//!                                 uniform layers, session arguments/frames, SURB, seeded loop cell,
//!                                 every OnionLinkFact variant, HTTPS fetch codec
//!   PINNED  = (WIRE_VERSION, keccak256(goldens))
//! ```
//!
//! Every golden is built from fixed keys, fixed scalars, and seeded `ChaCha20Rng` streams at a
//! fixed instant (`GOLDEN_EPOCH_MS`), so it moves only when the wire does. Coverage is
//! exhaustive by construction:
//!
//! - The formats are traced by serde reflection from each `rings-node` serde wire root
//!   (`OnlineNodeDescriptor`, `OnionExitDescriptor`, `ServiceDescriptor`, `MailDescriptor` and
//!   their signed bodies, `MailElement`/`MailManifest`/`MailDigest`/`AeadCiphertext`, extension
//!   `Envelope`, transport `Frame`, `RelayCommand`, `OnionLinkFact`, and `OnionHttpsRequest`/
//!   `OnionHttpsResponse`/`OnionHttpsOutcome`) plus every nested enum (`OnlineNodeType`,
//!   `ProxyProtocol`, `Initiator`, `SocketAddr`, `OnionExitFailure`).
//! - The canonical frames and elements cover custom and signed encodings that format reflection
//!   alone does not show: signed descriptors as stored in the DHT, sealed mail (`#1124`),
//!   extension envelopes, every `Frame` and `RelayCommand` variant (checked against
//!   [`declared_variants`]), Sphinx headers, uniform layers, session arguments and frames,
//!   reply blocks (SURBs), carry seeds/slots, KDF salts and info labels, every `OnionLinkFact`
//!   variant, and HTTPS fetch request/outcome encodings.
//!
//! Law (pin): `PINNED = (WIRE_VERSION, hash(goldens))`. A change that moves a byte fails here
//! until the same commit updates `PINNED`, and with it [`WIRE_VERSION`] by the bump rule in
//! `rings_core::message::payload::frame`; a change that moves no byte never trips it.

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;

use bytes::Bytes;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use rings_core::consts::DEFAULT_DELEGATION_TTL_MS;
use rings_core::delegation::Account;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::keccak256;
use rings_core::ecc::PublicKey;
use rings_core::ecc::SecretKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::message::MessageSigner;
use rings_core::message::SigningDomain;
use rings_core::message::WIRE_VERSION;
use rings_dns_relay::ProxyProtocol;
use serde::de::Visitor;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_reflection::Samples;
use serde_reflection::Tracer;
use serde_reflection::TracerConfig;

use crate::descriptor::RegistryElement;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;
use crate::dns_relay::relay_service;
use crate::dns_relay::service_topic;
use crate::dns_relay::ServedPort;
use crate::dns_relay::ServedTxt;
use crate::dns_relay::ServiceDescriptor;
use crate::dns_relay::ServiceDescriptorBody;
use crate::extension::ext::Envelope;
use crate::extension::protocols::echo;
use crate::extension::protocols::relay;
use crate::extension::protocols::relay::RelayCommand;
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
use crate::extension::transport::RelaySessionId;
use crate::mail::descriptor::descriptor_topic;
use crate::mail::descriptor::MailDescriptor;
use crate::mail::descriptor::MailDescriptorBody;
use crate::mail::descriptor::MailRecipient;
use crate::mail::envelope::mail_aad;
use crate::mail::envelope::seal;
use crate::mail::envelope::MailDigest;
use crate::mail::envelope::MailElement;
use crate::mail::envelope::MailManifest;
use crate::mail::mail_topic;
use crate::mail::open::open_mail;
use crate::mail::open::OpenedMail;
use crate::mail::open::Opening;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::OnionLinkFact;
use crate::onion::circuit::OnionReplayNonce;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::https::decode_outcome;
use crate::onion::https::encode_outcome;
use crate::onion::https::encode_request;
use crate::onion::https::OnionHttpsOutcome;
use crate::onion::https::OnionHttpsRequest;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::session::frame::OnionFrame;
use crate::onion::session::frame::OnionSequence;
use crate::onion::session::OnionSessionArguments;
use crate::onion::session::OnionSessionId;
use crate::onion::session::OnionTargetDigest;
use crate::onion::signature::OnionServiceName;
use crate::onion::signature::OnionSymbol;
use crate::onion::sphinx::builder::build_loop;
use crate::onion::sphinx::builder::build_surb;
use crate::onion::sphinx::carry;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header;
use crate::onion::sphinx::header::OnionHeaderMac;
use crate::onion::sphinx::layer::OnionApplication;
use crate::onion::sphinx::layer::OnionLayer;
use crate::onion::sphinx::layer::OnionLayerApplication;
use crate::onion::sphinx::layer::OnionLayerHead;
use crate::onion::sphinx::seed;
use crate::onion::sphinx::seed::OnionCarrySeed;
use crate::onion::sphinx::seed::OnionSegmentSeed;
use crate::onion::tests::fixture::fixture_secret;
use crate::onion::tests::fixture::grid_expiry;
use crate::onion::tests::fixture::session_loop;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitFailure;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionLoop;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRouteHop;
use crate::onion::ONION_EXITS_TOPIC;
use crate::online::OnlineNodeCapabilities;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;
use crate::online::ONLINE_NODES_TOPIC;

/// The pinned `(WIRE_VERSION, keccak256(goldens))`.
///
/// Both parts are one literal so a diff shows the version and the hash moving together.
const PINNED: (u16, &str) = (
    2,
    "7a0a9edf42724e6496f666077c55e44da1ab388e8c0b6aa7eb194c08aaef4be5",
);

const GOLDEN_EPOCH_MS: u128 = 1_700_000_000_000;
const GOLDEN_NETWORK_ID: u32 = 7;
const ALICE_ACCOUNT_SEED: u8 = 0x11;
const ALICE_DELEGATEE_SEED: u8 = 0x12;
const BOB_ACCOUNT_SEED: u8 = 0x21;

fn to_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct GoldenActor {
    did: Did,
    public_key: VerificationPublicKey,
    delegatee_key: DelegateeKey,
}

impl GoldenActor {
    fn new(account_seed: u8, delegatee_seed: u8) -> Self {
        let account_key: SecretKey = fixture_secret(account_seed);
        let delegatee_secret_key: SecretKey = fixture_secret(delegatee_seed);
        let did = Did::from(account_key.address());
        let public_key = VerificationPublicKey::Secp256k1(account_key.pubkey());
        let delegatee_did = Did::from(delegatee_secret_key.address());
        let delegator = Account::Secp256k1(did);
        let ttl_ms = DEFAULT_DELEGATION_TTL_MS;
        let ts_ms = GOLDEN_EPOCH_MS;
        let proof = format!("{delegatee_did}\n{ts_ms}\n{ttl_ms}");
        let delegator_signature = account_key.sign(&proof).unwrap().to_vec();
        let delegatee_key: DelegateeKey = serde_json::from_value(serde_json::json!({
            "delegation": {
                "delegatee_did": delegatee_did,
                "delegator": delegator,
                "ttl_ms": ttl_ms,
                "ts_ms": ts_ms,
                "delegator_signature": delegator_signature,
            },
            "delegatee_secret_key": delegatee_secret_key,
        }))
        .unwrap();
        delegatee_key
            .delegation()
            .verify_delegator_authorization_at(GOLDEN_EPOCH_MS)
            .unwrap();
        Self {
            did,
            public_key,
            delegatee_key,
        }
    }

    fn signer(&self) -> MessageSigner<&DelegateeKey> {
        MessageSigner::new(&self.delegatee_key, GOLDEN_NETWORK_ID)
    }
}

/// The variant names `E`'s `Deserialize` impl declares to serde.
///
/// A derived `Deserialize` passes its `VARIANTS` slice to `deserialize_enum` before reading from
/// the deserializer, so capturing that slice from a no-op deserializer yields the enum's
/// compile-time variant list without constructing any variant.
fn declared_variants<E: for<'de> Deserialize<'de>>() -> &'static [&'static str] {
    struct Capture<'a>(&'a mut &'static [&'static str]);

    #[derive(Debug)]
    struct Captured;

    impl std::fmt::Display for Captured {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("captured")
        }
    }

    impl std::error::Error for Captured {}

    impl serde::de::Error for Captured {
        fn custom<T: std::fmt::Display>(_: T) -> Self {
            Self
        }
    }

    impl<'de> Deserializer<'de> for Capture<'_> {
        type Error = Captured;

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _visitor: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            *self.0 = variants;
            Err(Captured)
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct identifier ignored_any
        }

        fn deserialize_any<V: Visitor<'de>>(
            self,
            _visitor: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            Err(Captured)
        }
    }

    let mut variants: &'static [&'static str] = &[];
    let _ = E::deserialize(Capture(&mut variants));
    assert!(
        !variants.is_empty(),
        "{} did not declare enum variants to deserialize_enum",
        std::any::type_name::<E>()
    );
    variants
}

/// Trace the serde format of a type that is serialized as a `Vec` of every variant, checking
/// that the slice covers every variant the enum declares to serde.
fn trace_enum_samples<E>(tracer: &mut Tracer, samples: &mut Samples, all: Vec<E>)
where E: Serialize + for<'de> Deserialize<'de> {
    let declared = declared_variants::<E>();
    assert_eq!(
        all.len(),
        declared.len(),
        "{} sample list must cover every declared variant ({declared:?})",
        std::any::type_name::<E>()
    );
    for value in &all {
        tracer.trace_value(samples, value).unwrap();
    }
}

/// The serde format of every type `rings-node` puts on the wire: descriptors, sealed mail,
/// extension envelopes, transport frames, relay commands, onion link facts, and HTTPS fetch
/// messages.
fn format_goldens() -> (String, Vec<u8>) {
    let alice = GoldenActor::new(ALICE_ACCOUNT_SEED, ALICE_DELEGATEE_SEED);
    let did = alice.did;
    let pk33 = alice.delegatee_key.delegatee_public_key();
    let pk48 = PublicKey([0x42; 48]);
    let public_key = match serde_json::to_value(pk33).unwrap() {
        serde_json::Value::String(text) => text,
        other => panic!("unexpected public key value: {other}"),
    };

    let config = TracerConfig::default()
        .default_borrowed_bytes_value(&[0; 16])
        .default_byte_buf_value(vec![0; 16])
        .default_u64_value(1)
        .default_string_value(public_key.clone())
        .default_borrowed_str_value(Box::leak(public_key.into_boxed_str()))
        .record_samples_for_structs(true);
    let mut tracer = Tracer::new(config);
    let mut samples = Samples::new();

    tracer.trace_value(&mut samples, &did).unwrap();
    tracer
        .trace_value(&mut samples, &alice.delegatee_key.delegation())
        .unwrap();
    trace_enum_samples(&mut tracer, &mut samples, vec![
        Account::Secp256k1(did),
        Account::Secp256r1(pk33),
        Account::EIP191(did),
        Account::BIP137(did),
        Account::Ed25519(pk33),
        Account::Bls12381(pk48),
    ]);
    trace_enum_samples(&mut tracer, &mut samples, vec![
        VerificationPublicKey::Secp256k1(pk33),
        VerificationPublicKey::Eip191(pk33),
        VerificationPublicKey::Bip137(pk33),
        VerificationPublicKey::Secp256r1(pk33),
        VerificationPublicKey::Ed25519(pk33),
        VerificationPublicKey::Bls12381(pk48),
    ]);

    // Sample descriptors whose fields embed `VerificationPublicKey` (which includes a 48-byte
    // BLS12-381 variant alongside 33-byte keys) or validated string types (`OnionServiceName`,
    // `OnionExitTarget`).
    let expires_at_ms = GOLDEN_EPOCH_MS + 180_000;
    let fresh_for_ms = 90_000;
    let online_body = OnlineNodeDescriptorBody {
        did,
        public_key: alice.public_key.clone(),
        delegatee_public_key: pk33,
        node_type: OnlineNodeType::Native,
        network_id: GOLDEN_NETWORK_ID,
        storage_redundancy: 3,
        dht_virtual_nodes: 3,
        capabilities: OnlineNodeCapabilities::onion_relay(OnionProcessEpoch::new([0x41; 16])),
        endpoint_hint: Some("https://node.example:8443".to_string()),
        started_at_ms: GOLDEN_EPOCH_MS,
        fresh_for_ms,
        expires_at_ms,
        version: "0.7.0".to_string(),
    };
    let (_, _, online_descriptor): (_, _, OnlineNodeDescriptor) =
        sign_descriptor_at(online_body.clone(), &alice);
    tracer.trace_value(&mut samples, &online_body).unwrap();
    tracer
        .trace_value(&mut samples, &online_descriptor)
        .unwrap();

    let exit_body = OnionExitDescriptorBody {
        did,
        public_key: alice.public_key.clone(),
        delegatee_public_key: pk33,
        process_epoch: OnionProcessEpoch::new([0x42; 16]),
        node_type: OnlineNodeType::Native,
        network_id: GOLDEN_NETWORK_ID,
        service: OnionServiceName::tcp(),
        policy: OnionExitPolicy {
            allowed_targets: vec![OnionExitTarget::parse("*:*").unwrap()],
            denied_targets: vec![OnionExitTarget::parse("127.0.0.1:80").unwrap()],
            max_sessions: 32,
            max_bytes_per_minute: 1_048_576,
        },
        started_at_ms: GOLDEN_EPOCH_MS,
        fresh_for_ms,
        expires_at_ms,
        version: "0.7.0".to_string(),
    };
    let (_, _, exit_descriptor): (_, _, OnionExitDescriptor) =
        sign_descriptor_at(exit_body.clone(), &alice);
    tracer.trace_value(&mut samples, &exit_body).unwrap();
    tracer.trace_value(&mut samples, &exit_descriptor).unwrap();

    let service_body = ServiceDescriptorBody {
        did,
        public_key: alice.public_key.clone(),
        network_id: GOLDEN_NETWORK_ID,
        ports: vec![ServedPort {
            port: 443,
            proxy_protocol: ProxyProtocol::V2,
        }],
        txt: vec![ServedTxt {
            name: "_acme-challenge".to_string(),
            values: vec!["token-1".to_string()],
        }],
        fresh_for_ms,
        expires_at_ms,
    };
    let (_, _, service_descriptor): (_, _, ServiceDescriptor) =
        sign_descriptor_at(service_body.clone(), &alice);
    tracer.trace_value(&mut samples, &service_body).unwrap();
    tracer
        .trace_value(&mut samples, &service_descriptor)
        .unwrap();

    let mail_body = MailDescriptorBody {
        did,
        public_key: alice.public_key.clone(),
        network_id: GOLDEN_NETWORK_ID,
        fresh_for_ms,
        expires_at_ms,
    };
    let (_, _, mail_descriptor): (_, _, MailDescriptor) =
        sign_descriptor_at(mail_body.clone(), &alice);
    tracer.trace_value(&mut samples, &mail_body).unwrap();
    tracer.trace_value(&mut samples, &mail_descriptor).unwrap();

    // Sealed mail ciphertext (`AeadCiphertext`): its `Deserialize` enforces
    // `encrypted_key.len() == 2`, so it is traced from a canonical sealed sample.
    let mut aead_rng = ChaCha20Rng::from_seed([0x51; 32]);
    let sample_ciphertext =
        encrypt_aead_with_rng(b"sample-mail", &mail_aad(did), pk33, &mut aead_rng).unwrap();
    tracer
        .trace_value(&mut samples, &sample_ciphertext)
        .unwrap();

    // Node wire roots and nested enums.
    tracer.trace_type::<MailElement>(&samples).unwrap();
    tracer.trace_type::<MailManifest>(&samples).unwrap();
    tracer.trace_type::<MailDigest>(&samples).unwrap();
    tracer.trace_type::<Envelope>(&samples).unwrap();
    tracer.trace_type::<Frame>(&samples).unwrap();
    tracer
        .trace_type::<RelayCommand<SocketAddr>>(&samples)
        .unwrap();
    tracer.trace_type::<OnionLinkFact>(&samples).unwrap();
    tracer.trace_type::<OnionHttpsRequest>(&samples).unwrap();
    tracer.trace_type::<OnionHttpsResponse>(&samples).unwrap();
    tracer.trace_type::<OnionHttpsOutcome>(&samples).unwrap();

    // Nested enums without custom deserializers: `trace_type` visits every variant.
    tracer.trace_type::<OnlineNodeType>(&samples).unwrap();
    tracer.trace_type::<ProxyProtocol>(&samples).unwrap();
    tracer.trace_type::<Initiator>(&samples).unwrap();
    tracer.trace_type::<SocketAddr>(&samples).unwrap();
    tracer.trace_type::<OnionExitFailure>(&samples).unwrap();

    let registry = tracer.registry().unwrap();
    let bytes = serde_json::to_vec_pretty(&registry).unwrap();
    ("format/registry".to_string(), bytes)
}

fn sign_descriptor_at<B>(body: B, actor: &GoldenActor) -> (Vec<u8>, Bytes, B::Descriptor)
where
    B: SignedDescriptorBody,
    B::Descriptor: SignedDescriptor<Body = B> + RegistryElement + PartialEq + std::fmt::Debug,
{
    let signing_data = body.body_signing_data().unwrap();
    let signature = actor
        .signer()
        .sign_at(B::DOMAIN_TAG, &signing_data, GOLDEN_EPOCH_MS)
        .unwrap();
    assert!(signature.verify_live_at(
        SigningDomain::new(B::DOMAIN_TAG, GOLDEN_NETWORK_ID),
        &signing_data,
        GOLDEN_EPOCH_MS,
    ));
    let descriptor = body.into_signed_descriptor(signature);
    assert_eq!(descriptor.descriptor_signing_data().unwrap(), signing_data);
    let element = descriptor.to_element().unwrap();
    let decoded = B::Descriptor::from_element(element.as_ref()).unwrap();
    assert_eq!(decoded, descriptor);
    (signing_data, element, descriptor)
}

/// Signed DHT descriptors, their signing transcripts, their domain tags, and their topic names.
fn descriptor_goldens() -> Vec<(String, Vec<u8>)> {
    let alice = GoldenActor::new(ALICE_ACCOUNT_SEED, ALICE_DELEGATEE_SEED);
    let expires_at_ms = GOLDEN_EPOCH_MS + 180_000;
    let fresh_for_ms = 90_000;

    let mut out = vec![
        (
            "descriptor/topic/online_nodes".to_string(),
            ONLINE_NODES_TOPIC.as_bytes().to_vec(),
        ),
        (
            "descriptor/topic/onion_exits".to_string(),
            ONION_EXITS_TOPIC.as_bytes().to_vec(),
        ),
        (
            "descriptor/topic/service".to_string(),
            service_topic(alice.did).into_bytes(),
        ),
        (
            "descriptor/topic/mail_descriptor".to_string(),
            descriptor_topic(alice.did).into_bytes(),
        ),
        (
            "descriptor/topic/mail_inbox".to_string(),
            mail_topic(alice.did).into_bytes(),
        ),
        (
            "descriptor/domain_tag/online_node".to_string(),
            <OnlineNodeDescriptorBody as SignedDescriptorBody>::DOMAIN_TAG
                .as_bytes()
                .to_vec(),
        ),
        (
            "descriptor/domain_tag/onion_exit".to_string(),
            <OnionExitDescriptorBody as SignedDescriptorBody>::DOMAIN_TAG
                .as_bytes()
                .to_vec(),
        ),
        (
            "descriptor/domain_tag/service".to_string(),
            <ServiceDescriptorBody as SignedDescriptorBody>::DOMAIN_TAG
                .as_bytes()
                .to_vec(),
        ),
        (
            "descriptor/domain_tag/mail".to_string(),
            <MailDescriptorBody as SignedDescriptorBody>::DOMAIN_TAG
                .as_bytes()
                .to_vec(),
        ),
    ];

    let online_body = OnlineNodeDescriptorBody {
        did: alice.did,
        public_key: alice.public_key.clone(),
        delegatee_public_key: alice.delegatee_key.delegatee_public_key(),
        node_type: OnlineNodeType::Native,
        network_id: GOLDEN_NETWORK_ID,
        storage_redundancy: 3,
        dht_virtual_nodes: 3,
        capabilities: OnlineNodeCapabilities::onion_relay(OnionProcessEpoch::new([0x41; 16])),
        endpoint_hint: Some("https://node.example:8443".to_string()),
        started_at_ms: GOLDEN_EPOCH_MS,
        fresh_for_ms,
        expires_at_ms,
        version: "0.7.0".to_string(),
    };
    let (online_signing_data, online_element, _) = sign_descriptor_at(online_body, &alice);
    out.push((
        "descriptor/OnlineNodeDescriptor/signing_data".to_string(),
        online_signing_data,
    ));
    out.push((
        "descriptor/OnlineNodeDescriptor/element".to_string(),
        online_element.to_vec(),
    ));

    let exit_body = OnionExitDescriptorBody {
        did: alice.did,
        public_key: alice.public_key.clone(),
        delegatee_public_key: alice.delegatee_key.delegatee_public_key(),
        process_epoch: OnionProcessEpoch::new([0x42; 16]),
        node_type: OnlineNodeType::Native,
        network_id: GOLDEN_NETWORK_ID,
        service: OnionServiceName::tcp(),
        policy: OnionExitPolicy {
            allowed_targets: vec![
                OnionExitTarget::parse("*:*").unwrap(),
                OnionExitTarget::parse("*:443").unwrap(),
                OnionExitTarget::parse("example.com:8443").unwrap(),
            ],
            denied_targets: vec![OnionExitTarget::parse("127.0.0.1:80").unwrap()],
            max_sessions: 32,
            max_bytes_per_minute: 1_048_576,
        },
        started_at_ms: GOLDEN_EPOCH_MS,
        fresh_for_ms,
        expires_at_ms,
        version: "0.7.0".to_string(),
    };
    let (exit_signing_data, exit_element, _) = sign_descriptor_at(exit_body, &alice);
    out.push((
        "descriptor/OnionExitDescriptor/signing_data".to_string(),
        exit_signing_data,
    ));
    out.push((
        "descriptor/OnionExitDescriptor/element".to_string(),
        exit_element.to_vec(),
    ));

    let service_body = ServiceDescriptorBody {
        did: alice.did,
        public_key: alice.public_key.clone(),
        network_id: GOLDEN_NETWORK_ID,
        ports: vec![
            ServedPort {
                port: 80,
                proxy_protocol: ProxyProtocol::Off,
            },
            ServedPort {
                port: 443,
                proxy_protocol: ProxyProtocol::V2,
            },
        ],
        txt: vec![ServedTxt {
            name: "_acme-challenge".to_string(),
            values: vec!["token-1".to_string(), "token-2".to_string()],
        }],
        fresh_for_ms,
        expires_at_ms,
    };
    let (service_signing_data, service_element, service_descriptor) =
        sign_descriptor_at(service_body, &alice);
    assert!(service_descriptor.announcement().is_ok());
    out.push((
        "descriptor/ServiceDescriptor/signing_data".to_string(),
        service_signing_data,
    ));
    out.push((
        "descriptor/ServiceDescriptor/element".to_string(),
        service_element.to_vec(),
    ));

    let mail_body = MailDescriptorBody {
        did: alice.did,
        public_key: alice.public_key.clone(),
        network_id: GOLDEN_NETWORK_ID,
        fresh_for_ms,
        expires_at_ms,
    };
    let (mail_signing_data, mail_element, mail_descriptor) = sign_descriptor_at(mail_body, &alice);
    assert_eq!(
        mail_descriptor.encryption_key_at(GOLDEN_NETWORK_ID, GOLDEN_EPOCH_MS),
        Some(alice.delegatee_key.delegatee_public_key())
    );
    out.push((
        "descriptor/MailDescriptor/signing_data".to_string(),
        mail_signing_data,
    ));
    out.push((
        "descriptor/MailDescriptor/element".to_string(),
        mail_element.to_vec(),
    ));

    out
}

/// Sealed mail (#1124): AEAD associated data, manifest element, chunk element, and mail digest.
fn mail_goldens() -> Vec<(String, Vec<u8>)> {
    let alice = GoldenActor::new(ALICE_ACCOUNT_SEED, ALICE_DELEGATEE_SEED);
    let recipient = MailRecipient {
        did: alice.did,
        encryption_key: alice.delegatee_key.delegatee_public_key(),
    };
    let message =
        b"Message-ID: <golden-1141@rings.example>\r\nSubject: golden\r\n\r\nhello from #1141\r\n";
    let mut rng = ChaCha20Rng::from_seed([0x55; 32]);
    let sealed = seal(message, &recipient, &mut rng).unwrap();
    assert_eq!(sealed.elements.len(), 2);

    let opened = open_mail(
        &sealed.elements,
        recipient.did,
        &alice.delegatee_key,
        |_| true,
    );
    assert_eq!(opened, vec![Opening::Opened(OpenedMail {
        mail_id: sealed.mail_id,
        message: message.to_vec(),
    })]);

    vec![
        ("mail/aad".to_string(), mail_aad(recipient.did)),
        (
            "mail/sealed/mail_id".to_string(),
            sealed.mail_id.to_string().into_bytes(),
        ),
        (
            "mail/sealed/manifest".to_string(),
            sealed.elements[0].to_vec(),
        ),
        (
            "mail/sealed/chunk_0".to_string(),
            sealed.elements[1].to_vec(),
        ),
    ]
}

/// Extension envelope, transport frames, and relay protocol commands.
fn extension_goldens() -> Vec<(String, Vec<u8>)> {
    let alice_did = Did::from(fixture_secret(ALICE_ACCOUNT_SEED).address());
    let bob_did = Did::from(fixture_secret(BOB_ACCOUNT_SEED).address());
    let session_id = RelaySessionId(42);
    let v4_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 10), 8080));
    let v6_addr = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
        8443,
        0,
        0,
    ));

    let envelope = Envelope {
        namespace: echo::NAMESPACE.to_string(),
        payload: Bytes::from_static(b"hello-extension"),
    };
    let encoded_envelope = envelope.encode().unwrap();
    let decoded_envelope = Envelope::decode(&encoded_envelope).unwrap();
    assert_eq!(decoded_envelope.namespace, envelope.namespace);
    assert_eq!(decoded_envelope.payload, envelope.payload);

    let mut out = vec![
        (
            "extension/namespace/onion_circuit".to_string(),
            ONION_CIRCUIT_NAMESPACE.as_bytes().to_vec(),
        ),
        (
            "extension/namespace/relay_tcp".to_string(),
            relay::TCP.as_bytes().to_vec(),
        ),
        (
            "extension/namespace/relay_udp".to_string(),
            relay::UDP.as_bytes().to_vec(),
        ),
        (
            "extension/namespace/echo".to_string(),
            echo::NAMESPACE.as_bytes().to_vec(),
        ),
        (
            "extension/service/dns_relay_443".to_string(),
            relay_service(443).into_bytes(),
        ),
        ("extension/Envelope".to_string(), encoded_envelope.to_vec()),
    ];

    let frames = vec![
        ("Open", Frame::Open {
            session: session_id,
            service: "http".to_string(),
        }),
        ("Data", Frame::Data {
            session: session_id,
            from_opener: true,
            seq: 7,
            bytes: Bytes::from_static(b"stream-bytes"),
        }),
        ("Datagram", Frame::Datagram {
            session: session_id,
            from_opener: false,
            bytes: Bytes::from_static(b"datagram-bytes"),
        }),
        ("Shutdown", Frame::Shutdown {
            session: session_id,
            from_opener: true,
            sent: 8,
        }),
        ("Close", Frame::Close {
            session: session_id,
            from_opener: false,
        }),
    ];
    let declared_frames = declared_variants::<Frame>();
    assert_eq!(
        frames.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_frames,
        "extension Frame goldens must cover every declared variant"
    );
    for (name, frame) in frames {
        let bytes = rings_codec::serialize(&frame).unwrap();
        let decoded: Frame = rings_codec::deserialize(&bytes).unwrap();
        assert_eq!(rings_codec::serialize(&decoded).unwrap(), bytes);
        out.push((format!("extension/Frame/{name}"), bytes));
    }

    let commands = vec![
        ("RegisterService", RelayCommand::RegisterService {
            name: "http".to_string(),
            target: v4_addr,
        }),
        ("Accepted", RelayCommand::Accepted {
            token: 99,
            peer: alice_did,
            service: "https".to_string(),
        }),
        ("Untrack", RelayCommand::Untrack {
            peer: alice_did,
            session: session_id,
            initiator: Initiator::Local,
        }),
        ("Abort", RelayCommand::Abort {
            peer: bob_did,
            session: session_id,
            initiator: Initiator::Remote,
        }),
    ];
    let declared_commands = declared_variants::<RelayCommand<SocketAddr>>();
    assert_eq!(
        commands.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_commands,
        "RelayCommand goldens must cover every declared variant"
    );
    for (name, command) in commands {
        let bytes = rings_codec::serialize(&command).unwrap();
        let decoded: RelayCommand<SocketAddr> = rings_codec::deserialize(&bytes).unwrap();
        assert_eq!(decoded, command);
        out.push((format!("extension/RelayCommand/{name}"), bytes));
    }
    let register_v6 = RelayCommand::RegisterService {
        name: "https".to_string(),
        target: v6_addr,
    };
    let register_v6_bytes = rings_codec::serialize(&register_v6).unwrap();
    assert_eq!(
        rings_codec::deserialize::<RelayCommand<SocketAddr>>(&register_v6_bytes).unwrap(),
        register_v6
    );
    out.push((
        "extension/RelayCommand/RegisterService_v6".to_string(),
        register_v6_bytes,
    ));

    out
}

fn fixture_hop(seed: u8, epoch: OnionProcessEpoch) -> OnionRouteHop {
    OnionRouteHop::new(
        Did::from(u32::from(seed)),
        fixture_secret(seed).pubkey(),
        epoch,
    )
}

fn fixture_loop_for_golden(epoch: OnionProcessEpoch) -> OnionLoop<OnionRouteHop> {
    session_loop([1, 2, 3, 4].map(|seed| fixture_hop(seed, epoch)))
}

/// Onion wire formats: KDF salts and info labels, derived carry seeds and AEZ carry slot, loop
/// classes, signature symbols, uniform layers, session arguments and frames, reply blocks
/// (`OnionSurb`), Sphinx header, seeded loop cell, `OnionLinkFact`, and HTTPS fetch codec.
fn onion_goldens() -> Vec<(String, Vec<u8>)> {
    let alice_did = Did::from(fixture_secret(ALICE_ACCOUNT_SEED).address());
    let bob_did = Did::from(fixture_secret(BOB_ACCOUNT_SEED).address());
    let fixture_epoch = OnionProcessEpoch::new([0x31; 16]);
    let fixture_expiry: OnionExpiry = grid_expiry(5);

    let mut out = vec![
        (
            "onion/kdf/header_salt".to_string(),
            header::HEADER_KDF_SALT.to_vec(),
        ),
        (
            "onion/kdf/header_stream_info".to_string(),
            header::STREAM_INFO.to_vec(),
        ),
        (
            "onion/kdf/header_mac_info".to_string(),
            header::MAC_INFO.to_vec(),
        ),
        (
            "onion/kdf/header_blind_info".to_string(),
            header::BLIND_INFO.to_vec(),
        ),
        ("onion/kdf/seed_salt".to_string(), seed::SEED_SALT.to_vec()),
        ("onion/kdf/key_salt".to_string(), seed::KEY_SALT.to_vec()),
        (
            "onion/kdf/relay_info".to_string(),
            seed::RELAY_INFO.to_vec(),
        ),
        (
            "onion/kdf/relay_indices".to_string(),
            seed::RELAY_INDICES.to_vec(),
        ),
        (
            "onion/kdf/consumer_info".to_string(),
            seed::CONSUMER_INFO.to_vec(),
        ),
        ("onion/kdf/key_info".to_string(), seed::KEY_INFO.to_vec()),
    ];

    // Carry seed schedule and AEZ carry slot.
    let segment_seed = OnionSegmentSeed::new([0x42; 32]);
    let derived_seeds = segment_seed.seeds();
    out.push((
        "onion/carry/seed/relay_1".to_string(),
        derived_seeds.relays[0].as_bytes().to_vec(),
    ));
    out.push((
        "onion/carry/seed/relay_2".to_string(),
        derived_seeds.relays[1].as_bytes().to_vec(),
    ));
    out.push((
        "onion/carry/seed/consumer".to_string(),
        derived_seeds.consumer.as_bytes().to_vec(),
    ));
    let segment_keys = segment_seed.keys().unwrap();
    let mut sealed_carry = vec![0_u8; OnionLoopClass::DEFAULT.carry_bytes()];
    carry::seal(
        OnionLoopClass::DEFAULT,
        &segment_keys,
        b"golden carry",
        &mut sealed_carry,
    )
    .unwrap();
    out.push(("onion/carry/slot/KiB16".to_string(), sealed_carry));

    // Loop classes and signature table.
    for class in OnionLoopClass::ALL {
        let spec = format!(
            "cell_bytes={} carry_bytes={} mac_label={}",
            class.cell_bytes(),
            class.carry_bytes(),
            to_hex(class.mac_label())
        );
        out.push((format!("onion/class/{class:?}"), spec.into_bytes()));
    }
    let symbols = std::iter::once((OnionSymbol::Relay, "relay"))
        .chain(
            OnionServiceName::world_facing()
                .map(|service| (OnionSymbol::WorldFacing(service.clone()), service.as_str())),
        )
        .collect::<Vec<_>>();
    for (symbol, name) in symbols {
        let spec = format!("code={} name={name}", symbol.code());
        out.push((format!("onion/symbol/{name}"), spec.into_bytes()));
    }

    // Session arguments (64 bytes) and uniform layers (205 bytes).
    let args_authority = OnionSessionArguments {
        session: OnionSessionId::new([0x51; 16]),
        digest: OnionTargetDigest::of(b"example.com:443"),
    };
    let args_ipv6 = OnionSessionArguments {
        session: OnionSessionId::new([0x52; 16]),
        digest: OnionTargetDigest::of(b"[2001:db8::1]:8443"),
    };
    let encoded_args_authority = args_authority.encode();
    let encoded_args_ipv6 = args_ipv6.encode();
    assert_eq!(
        OnionSessionArguments::decode(&encoded_args_authority),
        Some(args_authority)
    );
    assert_eq!(
        OnionSessionArguments::decode(&encoded_args_ipv6),
        Some(args_ipv6)
    );
    out.push((
        "onion/session_arguments/Authority".to_string(),
        encoded_args_authority.as_bytes().to_vec(),
    ));
    out.push((
        "onion/session_arguments/Ipv6".to_string(),
        encoded_args_ipv6.as_bytes().to_vec(),
    ));

    let mac = OnionHeaderMac::new([0x26; 16]);
    let make_layer = |application: OnionLayerApplication| OnionLayer {
        head: OnionLayerHead {
            application,
            next: Did::from(0x0102_0304_u32),
            epoch: fixture_epoch,
            expiry: fixture_expiry,
            nonce: OnionReplayNonce::new([0x23; 16]),
        },
        inbound: OnionCarrySeed::new([0x24; 32]),
        outbound: OnionSegmentSeed::new([0x25; 32]),
    };
    let relay_layer_bytes = make_layer(OnionLayerApplication::Relay).encode(&mac);
    let tcp_layer_bytes = make_layer(OnionLayerApplication::Apply(OnionApplication {
        symbol: OnionServiceName::tcp(),
        arguments: encoded_args_authority,
    }))
    .encode(&mac);
    let https_layer_bytes = make_layer(OnionLayerApplication::Apply(OnionApplication {
        symbol: OnionServiceName::https(),
        arguments: encoded_args_ipv6,
    }))
    .encode(&mac);
    let (decoded_layer, decoded_mac) = OnionLayer::decode(tcp_layer_bytes.as_slice()).unwrap();
    assert_eq!(
        decoded_layer.encode(&decoded_mac).as_slice(),
        tcp_layer_bytes.as_slice()
    );
    out.push(("onion/layer/Relay".to_string(), relay_layer_bytes.to_vec()));
    out.push((
        "onion/layer/Apply_tcp".to_string(),
        tcp_layer_bytes.to_vec(),
    ));
    out.push((
        "onion/layer/Apply_https".to_string(),
        https_layer_bytes.to_vec(),
    ));

    // Reply block (`OnionSurb`): pure reply cell production and seeded `build_surb` + `Credit`.
    let raw_surb_block = [
        [0x11_u8; 20].as_slice(),
        vec![0x22_u8; 2919].as_slice(),
        [0x33_u8; 32].as_slice(),
        fixture_expiry.to_wire_ms().to_be_bytes().as_slice(),
    ]
    .concat();
    let decoded_surb = OnionSurb::decode(OnionLoopClass::DEFAULT, &raw_surb_block).unwrap();
    let (_, produced_reply_cell) = decoded_surb.produce(b"golden reply").unwrap();
    out.push((
        "onion/surb/produced_cell".to_string(),
        produced_reply_cell.into_bytes(),
    ));

    let loop_hops = fixture_loop_for_golden(fixture_epoch);
    let (built_surb, built_surb_tag) = build_surb(
        loop_hops.return_path(),
        Did::from(99_u32),
        OnionLoopClass::DEFAULT,
        fixture_expiry,
        &mut ChaCha20Rng::from_seed([0x43; 32]),
    )
    .unwrap();
    out.push((
        "onion/surb/built_reply_tag".to_string(),
        built_surb_tag.tag.as_bytes().to_vec(),
    ));

    // Session frames (Data, Data with target, Fin, Fin with target, Credit, Abort).
    let frame_cases = [
        ("Data", OnionFrame::Data {
            sequence: OnionSequence::new(6),
            target: None,
            payload: Bytes::from_static(b"xyz"),
        }),
        ("Data_with_target", OnionFrame::Data {
            sequence: OnionSequence::new(5),
            target: Some(Bytes::from_static(b"example.com:443")),
            payload: Bytes::from_static(b"ab"),
        }),
        ("Fin", OnionFrame::Fin {
            sequence: OnionSequence::new(7),
            target: None,
        }),
        ("Fin_with_target", OnionFrame::Fin {
            sequence: OnionSequence::new(8),
            target: Some(Bytes::from_static(b"example.com:443")),
        }),
        ("Credit", OnionFrame::Credit {
            mark: OnionSequence::new(0x0102_0304),
            surbs: vec![built_surb],
        }),
        ("Abort", OnionFrame::Abort {
            sequence: OnionSequence::new(9),
        }),
    ];
    for (name, frame) in frame_cases {
        let encoded = frame.encode(OnionLoopClass::DEFAULT).unwrap();
        let decoded = OnionFrame::decode(OnionLoopClass::DEFAULT, encoded.as_slice()).unwrap();
        assert_eq!(
            decoded.encode(OnionLoopClass::DEFAULT).unwrap().as_slice(),
            encoded.as_slice()
        );
        out.push((format!("onion/frame/{name}"), encoded.to_vec()));
    }

    // Seeded full loop build (first cell χ₁ ‖ y₀ + reply tag t_⋄).
    let built_loop = build_loop(
        &loop_hops,
        &[OnionApplication {
            symbol: OnionServiceName::tcp(),
            arguments: encoded_args_authority,
        }],
        Did::from(99_u32),
        OnionLoopClass::DEFAULT,
        fixture_expiry,
        b"golden value",
        &mut ChaCha20Rng::from_seed([0x43; 32]),
    )
    .unwrap();
    out.push((
        "onion/loop/KiB16/first_cell".to_string(),
        built_loop.cell.into_bytes(),
    ));
    out.push((
        "onion/loop/KiB16/reply_tag".to_string(),
        built_loop.reply.tag.as_bytes().to_vec(),
    ));

    // Link facts (`OnionLinkFact`), checked against declared variants.
    let link_facts = vec![
        ("Opened", OnionLinkFact::Opened(alice_did)),
        ("Closed", OnionLinkFact::Closed(alice_did)),
        ("Dropped", OnionLinkFact::Dropped(bob_did)),
        ("Reconcile", OnionLinkFact::Reconcile),
    ];
    let declared_link_facts = declared_variants::<OnionLinkFact>();
    assert_eq!(
        link_facts.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_link_facts,
        "OnionLinkFact goldens must cover every declared variant"
    );
    for (name, fact) in link_facts {
        let encoded = fact.encode().unwrap();
        let decoded: OnionLinkFact = rings_codec::deserialize(&encoded).unwrap();
        assert_eq!(decoded, fact);
        out.push((format!("onion/link_fact/{name}"), encoded.to_vec()));
    }

    // HTTPS exit request and outcomes, checked against declared variants.
    let https_request = OnionHttpsRequest {
        method: "POST".to_string(),
        path: "/v1/submit?dry_run=false".to_string(),
        headers: vec![(
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        )],
        body: b"request-body".to_vec(),
    };
    let encoded_request = encode_request(&https_request).unwrap();
    let decoded_request: OnionHttpsRequest = rings_codec::deserialize(&encoded_request).unwrap();
    assert_eq!(decoded_request, https_request);
    out.push(("onion/https/Request".to_string(), encoded_request));

    let failures = vec![
        ("PermissionDenied", OnionExitFailure::PermissionDenied),
        ("MalformedRequest", OnionExitFailure::MalformedRequest),
        ("Internal", OnionExitFailure::Internal),
    ];
    let declared_failures = declared_variants::<OnionExitFailure>();
    assert_eq!(
        failures.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_failures,
        "OnionExitFailure goldens must cover every declared variant"
    );

    let outcomes = vec![
        (
            "Response",
            OnionHttpsOutcome::Response(OnionHttpsResponse {
                status: 200,
                headers: vec![("x-rings".to_string(), "ok".to_string())],
                body: b"response-body".to_vec(),
            }),
        ),
        (
            "Error",
            OnionHttpsOutcome::Error(OnionExitFailure::PermissionDenied),
        ),
    ];
    let declared_outcomes = declared_variants::<OnionHttpsOutcome>();
    assert_eq!(
        outcomes.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        declared_outcomes,
        "OnionHttpsOutcome goldens must cover every declared variant"
    );
    for (name, outcome) in outcomes {
        let encoded = encode_outcome(&outcome).unwrap();
        let decoded = decode_outcome(&encoded).unwrap();
        assert_eq!(decoded, outcome);
        out.push((format!("onion/https/Outcome/{name}"), encoded));
    }
    for (name, failure) in failures {
        let outcome = OnionHttpsOutcome::Error(failure);
        let encoded = encode_outcome(&outcome).unwrap();
        assert_eq!(decode_outcome(&encoded).unwrap(), outcome);
        out.push((format!("onion/https/Failure/{name}"), encoded));
    }

    out
}

/// Every golden vector, in a fixed order.
fn goldens() -> Vec<(String, Vec<u8>)> {
    let mut out = vec![format_goldens()];
    out.extend(descriptor_goldens());
    out.extend(mail_goldens());
    out.extend(extension_goldens());
    out.extend(onion_goldens());
    out
}

/// Hash of the golden table, injectively framed as `(len(label) ‖ label ‖ len(bytes) ‖ bytes)*`.
fn hash_goldens(entries: &[(String, Vec<u8>)]) -> String {
    let mut buf = Vec::new();
    for (label, bytes) in entries {
        buf.extend_from_slice(&(label.len() as u64).to_be_bytes());
        buf.extend_from_slice(label.as_bytes());
        buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        buf.extend_from_slice(bytes);
    }
    to_hex(keccak256(&buf))
}

/// The pin law: [`PINNED`] equals `(WIRE_VERSION, keccak256(goldens))`.
#[test]
fn test_wire_goldens_are_pinned_to_the_wire_version() {
    let entries = goldens();
    let digest = hash_goldens(&entries);
    let actual = (WIRE_VERSION, digest.as_str());

    if actual != PINNED {
        let per_entry = entries
            .iter()
            .map(|(label, bytes)| {
                format!(
                    "  {label:<48} {:>6} B  {}",
                    bytes.len(),
                    to_hex(keccak256(bytes))
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "rings-node wire goldens moved or WIRE_VERSION changed without updating PINNED (#1141).\n\
             \n\
              Bump rule (see crates/core/src/message/payload/frame.rs):\n\
             - If a wire byte moved since the last release, WIRE_VERSION must be one greater\n\
               than that release's, and PINNED below updated to match, in the same commit.\n\
             - If only format/registry moved, and only because serde-reflection or Cargo.lock\n\
               changed, update PINNED's hash without bumping WIRE_VERSION.\n\
             \n\
             pinned: ({}, {:?})\n\
             actual: ({}, {:?})\n\
             \n\
             per-entry digests ({} entries):\n\
             {per_entry}\n",
            PINNED.0,
            PINNED.1,
            actual.0,
            actual.1,
            entries.len()
        );
    }
}

/// Determinism law: two runs of [`goldens`] produce the same bytes entry for entry.
#[test]
fn test_wire_goldens_are_deterministic() {
    let first = goldens();
    let second = goldens();
    assert_eq!(first.len(), second.len());
    for ((label_a, bytes_a), (label_b, bytes_b)) in first.iter().zip(second.iter()) {
        assert_eq!(label_a, label_b);
        assert_eq!(
            bytes_a,
            bytes_b,
            "golden {label_a} is not deterministic: {} vs {}",
            to_hex(keccak256(bytes_a)),
            to_hex(keccak256(bytes_b))
        );
    }
}
