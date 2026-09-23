//! Golden wire bytes of the onion data plane.
//!
//! Law (#834 L10, Phase 1): a refactor of the onion circuit preserves every encoding below byte for
//! byte. Writing `enc : T → Bytes` for the Rings codec, each test fixes one value `v₀` built only
//! from constants and asserts `enc(v₀) = golden` together with `dec ∘ enc (v₀) = v₀`. No RNG reaches
//! a pinned byte: ciphertexts are literal envelopes, and where a live seal is unavoidable only its
//! deterministic plaintext framing is compared.
//!
//! A failing test here is a wire cutover, never a fixture to refresh: Phase 2 replaces this set in
//! one commit together with the version bump.

use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::message::MessageSigner;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::super::cell::seal_message;
use super::super::cell::OnionWireCell;
use super::super::codec::OnionWireMessage;
use super::super::crypto::decrypt_forward_layer;
use super::super::OnionBackwardFrame;
use super::super::OnionCellBucket;
use super::super::OnionCircuitId;
use super::super::OnionCircuitPayload;
use super::super::OnionClientReturn;
use super::super::OnionForwardFrame;
use super::super::OnionForwardLayer;
use super::super::OnionForwardNonce;
use super::super::OnionForwardSequence;
use super::super::OnionReturnId;
use super::test_circuit_protocol::session;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitEpoch;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionServiceName;
use crate::onion::ONION_EXITS_TOPIC;
use crate::onion::ONION_RELAY_CAPABILITY;
use crate::online::OnlineNodeType;
use crate::tests::TEST_NETWORK_ID;

/// Pinned encoding of [`relay_layer`].
const GOLDEN_RELAY_LAYER: &str = "002a3078303030303030303030303030303030303030303030303030303030303030303030313032303330341111111111111111111111111111111133314c556b7a4d35714c7333314c556b7a4d35714c7333314c556b7a4d35714c7333314c556b7a4d35714c7333314c576f53425833315744647a323846576f34315744647a323846576f34315744647a323846576f34315744647a323846576f34315258557a694401023342763439646b693638593842763439646b693638593842763439646b693638593842763439646b6936385938384d36653272723343356f3264526b574a553943356f3264526b574a553943356f3264526b574a553943356f3264526b574a553938593357704331334346587564366e765551414346587564366e765551414346587564366e765551414346587564366e76555141385a5867444135334352476e636d714c654c424352476e636d714c654c424352476e636d714c654c424352476e636d714c654c42386a7a79486778414141414141414141414141054141414141";
/// Pinned encoding of [`exit_layer`].
const GOLDEN_EXIT_LAYER: &str = "0121212121212121212121212121212121333166785779684166676a353166785779684166676a353166785779684166676a353166785779684166676a3531557743527a55222222222222222222222222222222223331716850794e443572663631716850794e443572663631716850794e443572663631716850794e443572663631657242535972b0ba97ffbc31232323232323232323232323232323230703746370046f70656e";
/// Pinned encoding of `OnionWireMessage::Forward` over [`ciphertext`]`(0x51)`.
const GOLDEN_FORWARD_MESSAGE: &str = "00505050505050505050505050505050500102334562744459584e6b7353514562744459584e6b7353514562744459584e6b7353514562744459584e6b7353514143544a726e6733456d643659435242334e52456d643659435242334e52456d643659435242334e52456d643659435242334e52414d51756e414c3345774d7958735462444a5345774d7958735462444a5345774d7958735462444a5345774d7958735462444a5341546d54694d663346373672585957315045544637367258595731504554463736725859573150455446373672585957315045544162326d7a3355515151515151515151515151055151515151";
/// Pinned encoding of `OnionWireMessage::Backward` over [`ciphertext`]`(0x61)`.
const GOLDEN_BACKWARD_MESSAGE: &str = "016060606060606060606060606060606001023348486948544a3352634c6748486948544a3352634c6748486948544a3352634c6748486948544a3352634c67427843585738443348545441537935716e476848545441537935716e476848545441537935716e476848545441537935716e4768433755527370793348644333536538467843694864433353653846784369486443335365384678436948644333536538467843694345667359614633486e7676534b416738386a486e7676534b416738386a486e7676534b416738386a486e7676534b416738386a434a4d42557055616161616161616161616161056161616161";
/// Pinned encoding of `OnionWireMessage::Cover`.
const GOLDEN_COVER_MESSAGE: &str = "02";
/// Pinned encoding of one `OnionWireCell` in the `KiB16` class over [`ciphertext`]`(0x71)`.
const GOLDEN_WIRE_CELL: &str = "010102334b79594d4e3469364d45784b79594d4e3469364d45784b79594d4e3469364d45784b79594d4e3469364d4578446d517233586f334c3948454d6a6b575841794c3948454d6a6b575841794c3948454d6a6b575841794c3948454d6a6b5758417944795252584562334c4b32374d516e7668367a4c4b32374d516e7668367a4c4b32374d516e7668367a4c4b32374d516e7668367a45347a75763151334c556b7a4d35714c7333314c556b7a4d35714c7333314c556b7a4d35714c7333314c556b7a4d35714c733331453946484c4a50717171717171717171717171057171717171";
/// Pinned signing data of [`exit_descriptor_body`].
const GOLDEN_EXIT_DESCRIPTOR_SIGNING_DATA: &str = "2a30783030303030303030303030303030303030303030303030303030303030303030306130623063306400333231534779334657326237323153477933465732623732315347793346573262373231534779334657326237316a39514a674e33324242397869487643583832424239786948764358383242423978694876435838324242397869487643583831745a374d7676313131313131313131313131313131310101056874747073010f6578616d706c652e636f6d3a343433010b31302e302e302e313a3232100480804080d095ffbc31909e96ffbc31a0dd9bffbc310c302e302e302d676f6c64656e";

/// Frozen AEAD namespace of forward layers (`ONION_AEAD_NAMESPACE`).
const FORWARD_LAYER_AEAD_NAMESPACE: &[u8] = b"rings-node:onion-circuit";
/// Frozen AEAD namespace of hop cells (`ONION_CELL_AEAD_NAMESPACE`).
const CELL_AEAD_NAMESPACE: &[u8] = b"rings-node:onion-cell";
/// Frozen signing domain of onion-exit descriptors.
const EXIT_DESCRIPTOR_DOMAIN_TAG: &[u8] = b"rings-node:onion-exit-descriptor";
/// Fixed circuit id bound into the forward-layer AEAD fixture.
const PINNED_CIRCUIT_ID: OnionCircuitId = OnionCircuitId::new([0x2a; 16]);

/// Render bytes as lowercase hex so a golden mismatch prints a readable diff.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Assert `enc(value) = golden` and `dec(enc(value)) = value` under the Rings codec.
fn assert_golden<T>(value: &T, golden: &str)
where T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug {
    let encoded = rings_codec::serialize(value).expect("encode golden fixture");
    assert_eq!(hex(encoded.as_slice()), golden);
    assert_eq!(
        rings_codec::deserialize::<T>(encoded.as_slice()).expect("decode golden fixture"),
        *value
    );
}

/// Constant compressed-point bytes; the codec carries curve elements opaquely.
const fn point(seed: u8) -> PublicKey<33> {
    PublicKey([seed; 33])
}

/// Literal AEAD envelope with the fixed two-block wrapped key the decoder admits.
fn ciphertext(seed: u8) -> AeadCiphertext {
    AeadCiphertext {
        version: 1,
        encrypted_key: vec![
            (point(seed), point(seed.wrapping_add(1))),
            (point(seed.wrapping_add(2)), point(seed.wrapping_add(3))),
        ],
        nonce: [seed; 12],
        ciphertext: vec![seed; 5],
    }
}

/// Relay layer whose every field is a distinct constant.
fn relay_layer() -> OnionForwardLayer {
    OnionForwardLayer::Relay {
        next_hop: Did::from(0x0102_0304_u32),
        next_circuit_id: OnionCircuitId::new([0x11; 16]),
        next_delegatee_public_key: point(0x02),
        return_delegatee_public_key: point(0x03),
        inner: ciphertext(0x41),
    }
}

/// Exit layer whose every field is a distinct constant.
fn exit_layer() -> OnionForwardLayer {
    OnionForwardLayer::Exit {
        process_epoch: OnionExitEpoch::new([0x21; 16]),
        client: OnionClientReturn {
            delegatee_public_key: point(0x04),
            return_id: OnionReturnId::new([0x22; 16]),
        },
        return_delegatee_public_key: point(0x05),
        expires_at_ms: 1_700_000_030_000,
        forward_nonce: OnionForwardNonce::new([0x23; 16]),
        forward_sequence: OnionForwardSequence::new(7),
        payload: OnionCircuitPayload::new(OnionServiceName::tcp(), b"open".as_slice()),
    }
}

/// Exit-descriptor body whose every signed field is a distinct constant.
fn exit_descriptor_body() -> OnionExitDescriptorBody {
    OnionExitDescriptorBody {
        did: Did::from(0x0a0b_0c0d_u32),
        public_key: VerificationPublicKey::Secp256k1(point(0x06)),
        delegatee_public_key: point(0x07),
        process_epoch: OnionExitEpoch::new([0x31; 16]),
        node_type: OnlineNodeType::Native,
        network_id: TEST_NETWORK_ID,
        service: OnionServiceName::https(),
        policy: OnionExitPolicy {
            allowed_targets: vec![OnionExitTarget::parse("example.com:443").expect("target")],
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
fn test_relay_layer_wire_is_pinned() {
    assert_golden(&relay_layer(), GOLDEN_RELAY_LAYER);
}

#[test]
fn test_exit_layer_wire_is_pinned() {
    assert_golden(&exit_layer(), GOLDEN_EXIT_LAYER);
}

#[test]
fn test_wire_message_variants_are_pinned() {
    assert_golden(
        &OnionWireMessage::Forward(OnionForwardFrame {
            circuit_id: OnionCircuitId::new([0x50; 16]),
            layer: ciphertext(0x51),
        }),
        GOLDEN_FORWARD_MESSAGE,
    );
    assert_golden(
        &OnionWireMessage::Backward(OnionBackwardFrame {
            circuit_id: OnionCircuitId::new([0x60; 16]),
            payload: ciphertext(0x61),
        }),
        GOLDEN_BACKWARD_MESSAGE,
    );
    assert_golden(&OnionWireMessage::Cover, GOLDEN_COVER_MESSAGE);
}

#[test]
fn test_wire_cell_envelope_is_pinned() {
    assert_golden(
        &OnionWireCell {
            bucket: OnionCellBucket::KiB16,
            sealed: ciphertext(0x71),
        },
        GOLDEN_WIRE_CELL,
    );
}

/// Cell plaintext = `le32(|m|) ‖ m ‖ pad`, `|plaintext| = bucket.plaintext_len()`, sealed under
/// `AAD = enc(CELL_AEAD_NAMESPACE, bucket)`. The padding and the ciphertext are random; the
/// prefix, the message bytes, the length and the AAD are pinned.
#[test]
fn test_cell_plaintext_framing_is_pinned() {
    let recipient = session();
    let sealed = seal_message(
        &OnionWireMessage::Cover,
        recipient.delegatee_public_key(),
        Some(OnionCellBucket::KiB4),
    )
    .expect("seal cover cell");
    let cell = rings_codec::deserialize::<OnionWireCell>(sealed.as_ref()).expect("decode cell");
    let aad = [
        [u8::try_from(CELL_AEAD_NAMESPACE.len()).expect("short namespace")].as_slice(),
        CELL_AEAD_NAMESPACE,
        [0x00].as_slice(),
    ]
    .concat();
    let plaintext = recipient
        .decrypt_elgamal_aead(&cell.sealed, aad.as_slice())
        .expect("pinned cell AAD opens the cell");

    assert_eq!(cell.bucket, OnionCellBucket::KiB4);
    assert_eq!(plaintext.len(), 4 * 1024);
    assert_eq!(plaintext.get(..4), Some([1, 0, 0, 0].as_slice()));
    assert_eq!(
        plaintext.get(4..5).map(hex),
        Some(GOLDEN_COVER_MESSAGE.to_string())
    );
}

/// Forward-layer `AAD = enc(ONION_AEAD_NAMESPACE, Forward, circuit_id)`, written out byte by byte:
/// a layer sealed under this literal AAD opens through the production decryptor.
#[test]
fn test_forward_layer_aead_context_is_pinned() {
    let recipient = session();
    let aad = [
        [u8::try_from(FORWARD_LAYER_AEAD_NAMESPACE.len()).expect("short namespace")].as_slice(),
        FORWARD_LAYER_AEAD_NAMESPACE,
        [0x00].as_slice(),
        [0x2a; 16].as_slice(),
    ]
    .concat();
    let plaintext = rings_codec::serialize(&relay_layer()).expect("encode relay layer");
    let sealed = encrypt_aead_with_rng(
        plaintext.as_slice(),
        aad.as_slice(),
        recipient.delegatee_public_key(),
        &mut rand::thread_rng(),
    )
    .expect("seal relay layer");

    assert_eq!(
        decrypt_forward_layer(&recipient, PINNED_CIRCUIT_ID, &sealed).expect("open relay layer"),
        relay_layer()
    );
}

#[test]
fn test_exit_descriptor_signing_data_is_pinned() {
    let body = exit_descriptor_body();
    let signing_data = body.body_signing_data().expect("descriptor signing data");
    let signature = MessageSigner::new(&session(), TEST_NETWORK_ID)
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
    assert_eq!(ONION_RELAY_CAPABILITY, "onion-relay");
}
