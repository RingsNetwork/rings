//! Golden wire bytes of the loop data plane (#843).
//!
//! Law (#834 L10): every encoding below is fixed byte for byte. Writing `enc : T → Bytes`, each
//! test fixes one value `v₀` built only from constants and seeded generators and asserts
//! `enc(v₀) = golden`, with `dec ∘ enc (v₀) = v₀` where a decoder exists. A cell is pinned by its
//! SHA-256 digest `H(enc(v₀))` and its width, since it is `b` bytes.
//!
//! A failing test here is a wire cutover, never a fixture to refresh: there is no protocol
//! versioning, so a change to any of these bytes is a total cutover of the network.

use rand::rngs::StdRng;
use rand::SeedableRng;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::ecc::SecretKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::message::MessageSigner;
use sha2::Digest;
use sha2::Sha256;

use super::super::OnionExpiry;
use super::super::ONION_CIRCUIT_NAMESPACE;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;
use crate::onion::session::frame::OnionFrame;
use crate::onion::session::frame::OnionSequence;
use crate::onion::session::OnionSessionArguments;
use crate::onion::session::OnionSessionId;
use crate::onion::session::OnionTargetDigest;
use crate::onion::sphinx::builder::build_loop;
use crate::onion::sphinx::builder::build_surb;
use crate::onion::sphinx::builder::OnionApplication;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionLoop;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;
use crate::onion::ONION_EXITS_TOPIC;
use crate::online::OnlineNodeType;
use crate::tests::TEST_NETWORK_ID;

/// Pinned `enc(data(5, T, t = example.com:443, w′ = "ab"))`.
const GOLDEN_DATA_WITH_TARGET: &str = "000000000501000f6578616d706c652e636f6d3a3434336162";
/// Pinned `enc(data(6, 0, "xyz"))`.
const GOLDEN_DATA: &str = "00000000060078797a";
/// Pinned `enc(fin(7))`.
const GOLDEN_FIN: &str = "0100000007";
/// Pinned `H(enc(credit(υ)))` of the fixture reply block.
const GOLDEN_CREDIT_DIGEST: &str =
    "c6f5a52476199519d2ad18dcdb02116cd0d9b1b705734fe03ea7ad82b54bf68c";
/// Pinned `ā = ς ‖ SHA-256(t) ‖ 0^16` of the fixture session.
const GOLDEN_SESSION_ARGUMENTS: &str =
    "515151515151515151515151515151512d92752e69614799ea8467c10d252c76f79c85845cf23d54406ef49ab463ff0c00000000000000000000000000000000";
/// Pinned `H(χ₁ ‖ y₀)` of the fixture loop's first cell.
const GOLDEN_LOOP_CELL_DIGEST: &str =
    "4a135348f22224f7f2caf3cc469f5bb8f6fcd84adcc85691086e1c70b353f756";
/// Pinned `t_⋄` of the fixture loop.
const GOLDEN_LOOP_REPLY_TAG: &str = "3e5eef7ce8cf481099959ffd78fbcf1a";
/// Pinned signing data of [`exit_descriptor_body`].
const GOLDEN_EXIT_DESCRIPTOR_SIGNING_DATA: &str =
    "2a30783030303030303030303030303030303030303030303030303030303030303030306130623063306400333231534779334657326237323153477933465732623732315347793346573262373231534779334657326237316a39514a674e33324242397869487643583832424239786948764358383242423978694876435838324242397869487643583831745a374d7676313131313131313131313131313131310101056874747073020f6578616d706c652e636f6d3a343433052a3a343433010b31302e302e302e313a3232100480804080d095ffbc31909e96ffbc31a0dd9bffbc310c302e302e302d676f6c64656e";

/// Frozen signing domain of onion-exit descriptors.
const EXIT_DESCRIPTOR_DOMAIN_TAG: &[u8] = b"rings-node:onion-exit-descriptor";
/// The fixture session's target.
const FIXTURE_TARGET: &[u8] = b"example.com:443";
/// The fixture loop's process epoch.
const FIXTURE_EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([0x31; 16]);

/// Render bytes as lowercase hex so a golden mismatch prints a readable diff.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `H(bytes)` in hex.
fn digest(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

/// The fixed secret `(seed)·0x0101…01`.
fn secret(seed: u8) -> SecretKey {
    SecretKey::try_from(format!("{seed:02x}").repeat(32).as_str()).expect("fixture scalar")
}

/// The route hop `seed`: DID and session key both fixed by `seed`.
fn hop(seed: u8) -> OnionRouteHop {
    OnionRouteHop::new(
        Did::from(u32::from(seed)),
        secret(seed).pubkey(),
        FIXTURE_EPOCH,
    )
}

/// The fixture loop `g = 1, r₀₂ = 2, h = 3, r₁₁ = 4, g = 1`.
fn fixture_loop() -> OnionLoop<OnionRouteHop> {
    let mut relays = [1, 2, 4].into_iter();
    OnionLoop::try_unfold(Vec::new(), hop(3), |_| {
        relays
            .next()
            .map(hop)
            .ok_or(crate::error::Error::InvalidData)
    })
    .expect("the fixture loop")
}

/// The fixture expiry `x = 5Q`.
fn fixture_expiry() -> OnionExpiry {
    OnionExpiry::from_ms(150_000).expect("on the grid")
}

/// The fixture session's arguments.
fn session_arguments() -> OnionSessionArguments {
    OnionSessionArguments {
        session: OnionSessionId::new([0x51; 16]),
        digest: OnionTargetDigest::of(FIXTURE_TARGET),
    }
}

/// Assert `enc(frame) = golden` and that the encoding decodes to a frame encoding the same.
fn assert_frame(frame: &OnionFrame, golden: &str) {
    let encoded = frame
        .encode(OnionLoopClass::DEFAULT)
        .expect("the frame fits");
    assert_eq!(hex(encoded.as_slice()), golden);
    let decoded = OnionFrame::decode(OnionLoopClass::DEFAULT, encoded.as_slice()).expect("decode");
    assert_eq!(
        decoded.encode(OnionLoopClass::DEFAULT).expect("fits"),
        encoded
    );
}

#[test]
fn test_session_frames_are_pinned() {
    assert_frame(
        &OnionFrame::Data {
            sequence: OnionSequence::new(5),
            target: Some(bytes::Bytes::from_static(FIXTURE_TARGET)),
            payload: bytes::Bytes::from_static(b"ab"),
        },
        GOLDEN_DATA_WITH_TARGET,
    );
    assert_frame(
        &OnionFrame::Data {
            sequence: OnionSequence::new(6),
            target: None,
            payload: bytes::Bytes::from_static(b"xyz"),
        },
        GOLDEN_DATA,
    );
    assert_frame(
        &OnionFrame::Fin {
            sequence: OnionSequence::new(7),
        },
        GOLDEN_FIN,
    );
}

#[test]
fn test_credit_frame_is_pinned() {
    let (surb, _) = build_surb(
        fixture_loop().return_path(),
        Did::from(99_u32),
        OnionLoopClass::DEFAULT,
        fixture_expiry(),
        &mut StdRng::seed_from_u64(0x843),
    )
    .expect("build the reply block");
    let encoded = OnionFrame::Credit(vec![surb])
        .encode(OnionLoopClass::DEFAULT)
        .expect("one block fits");

    assert_eq!(digest(encoded.as_slice()), GOLDEN_CREDIT_DIGEST);
    assert!(matches!(
        OnionFrame::decode(OnionLoopClass::DEFAULT, encoded.as_slice()),
        Ok(OnionFrame::Credit(blocks)) if blocks.len() == 1
    ));
}

#[test]
fn test_session_arguments_are_pinned() {
    let arguments = session_arguments().encode();

    assert_eq!(hex(arguments.as_bytes()), GOLDEN_SESSION_ARGUMENTS);
    assert_eq!(
        OnionSessionArguments::decode(&arguments),
        Some(session_arguments())
    );
}

/// The whole first cell of a seeded loop: every header, carry and padding byte is a function
/// of the hops' keys, the applications, the value and the seed.
#[test]
fn test_loop_cell_is_pinned() {
    let built = build_loop(
        &fixture_loop(),
        &[OnionApplication {
            symbol: OnionServiceName::tcp(),
            arguments: session_arguments().encode(),
        }],
        Did::from(99_u32),
        OnionLoopClass::DEFAULT,
        fixture_expiry(),
        b"golden value",
        &mut StdRng::seed_from_u64(0x843),
    )
    .expect("build the loop");

    assert_eq!(built.guard, Did::from(1_u32));
    let cell = built.cell.into_bytes();
    assert_eq!(cell.len(), OnionLoopClass::DEFAULT.cell_bytes());
    assert_eq!(digest(cell.as_slice()), GOLDEN_LOOP_CELL_DIGEST);
    assert_eq!(hex(built.reply.tag.as_bytes()), GOLDEN_LOOP_REPLY_TAG);
}

/// Exit-descriptor body whose every signed field is a distinct constant.
fn exit_descriptor_body() -> OnionExitDescriptorBody {
    OnionExitDescriptorBody {
        did: Did::from(0x0a0b_0c0d_u32),
        public_key: VerificationPublicKey::Secp256k1(PublicKey([0x06; 33])),
        delegatee_public_key: PublicKey([0x07; 33]),
        process_epoch: FIXTURE_EPOCH,
        node_type: OnlineNodeType::Native,
        network_id: TEST_NETWORK_ID,
        service: OnionServiceName::https(),
        policy: OnionExitPolicy {
            allowed_targets: vec![
                OnionExitTarget::parse("example.com:443").expect("target"),
                OnionExitTarget::parse("*:443").expect("target"),
            ],
            denied_targets: vec![OnionExitTarget::parse("10.0.0.1:22").expect("target")],
            max_circuits: 16,
            max_streams_per_circuit: 4,
            max_bytes_per_minute: 1_048_576,
        },
        started_at_ms: 1_700_000_000_000,
        heartbeat_at_ms: 1_700_000_010_000,
        expires_at_ms: 1_700_000_100_000,
        version: "0.0.0-golden".to_string(),
    }
}

#[test]
fn test_exit_descriptor_signing_data_is_pinned() {
    let body = exit_descriptor_body();
    let signing_data = body.body_signing_data().expect("descriptor signing data");
    let session = DelegateeKey::new_with_seckey(&secret(9)).expect("fixture delegation");
    let signature = MessageSigner::new(&session, TEST_NETWORK_ID)
        .sign(OnionExitDescriptorBody::DOMAIN_TAG, signing_data.as_slice())
        .expect("sign descriptor");
    let descriptor = body.into_signed_descriptor(signature);

    assert_eq!(
        hex(signing_data.as_slice()),
        GOLDEN_EXIT_DESCRIPTOR_SIGNING_DATA
    );
    assert_eq!(
        descriptor
            .descriptor_signing_data()
            .expect("descriptor signing data"),
        signing_data
    );
}

#[test]
fn test_frozen_onion_labels() {
    assert_eq!(
        OnionExitDescriptorBody::DOMAIN_TAG.as_bytes(),
        EXIT_DESCRIPTOR_DOMAIN_TAG
    );
    assert_eq!(ONION_EXITS_TOPIC, "onion_exits");
    assert_eq!(ONION_CIRCUIT_NAMESPACE, "onion-circuit");
}
