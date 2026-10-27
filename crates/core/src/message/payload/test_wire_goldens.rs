//! Golden vectors of the wire, pinned together with [`WIRE_VERSION`] (#1107): the bump rule of
//! `frame`, enforced.
//!
//! ```text
//!   goldens = formats          the serde format of every type  (traced: every variant)
//!           ∪ message frames   one per Message variant         (Message::test_variants)
//!           ∪ signaling frames the recorded path, both ways     (ConnectNodeSend)
//!           ∪ link control     one per LinkControl variant     (its declared variants)
//!           ∪ transcripts      one per signing domain tag      (every domain_tag! literal)
//!           ∪ handshake        the base58-check offer string   (createOffer's frame)
//!   PINNED  = (WIRE_VERSION, keccak256(goldens))
//! ```
//!
//! Every golden is built from fixed keys at a fixed instant, under a simulation runtime that
//! fixes the clock and the identifiers, so it moves only when the wire does. Coverage is
//! exhaustive by construction:
//!
//! - The formats are traced by serde reflection from the frames' roots (the payload frame, the
//!   link-control frame, the message and the probe completion its transaction data carries):
//!   every type they reach, every variant of every enum, every field, as `rings_codec` encodes
//!   it, an enum by its variant's index. So inserting a variant, or changing a field of a
//!   variant no sample takes, moves the `format/registry` golden.
//! - The frames add what a format does not show: the header, the bytes of custom
//!   `Serialize` impls, and the signed bytes. The messages come from the variant table that
//!   defines [`Message`]; the link-control frames are checked against the variants serde
//!   declares.
//! - The transcripts are those of every `domain_tag!("…")` label in the sources of the
//!   workspace's crates, read as text. Blind spots: a tag built by `DomainTag::new` from a
//!   runtime string, or one outside a crate's `src`, is not found, and a `domain_tag!("…")`
//!   inside a comment is (it then pins a label no domain uses). Tags are therefore declared
//!   with `domain_tag!` in `src`, and `DomainTag::new` is kept for labels known only at
//!   runtime, of which there are none on the wire.
//!
//! Scope: the core wire. The formats rings-node adds (onion cells, sealed mail, descriptors)
//! are pinned by a sibling test against the same version (#1141).
//!
//! Law (pin): `PINNED = (WIRE_VERSION, hash(goldens))`. A change that moves a byte fails here
//! until the same commit updates `PINNED`, and with it [`WIRE_VERSION`] by the bump rule; a
//! change that moves no byte never trips it.

use std::path::Path;
use std::path::PathBuf;

use serde::de::Visitor;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_reflection::Samples;
use serde_reflection::Tracer;
use serde_reflection::TracerConfig;

use super::LinkControl;
use super::MessagePayload;
use super::Transaction;
use super::WirePayload;
use super::WIRE_VERSION;
use crate::delegation::Account;
use crate::delegation::DelegateeKey;
use crate::dht::delivery::NextHop;
use crate::dht::entry::EntryVersion;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::ecc::PublicKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeSend;
use crate::message::DhtProtocolMode;
use crate::message::DomainTag;
use crate::message::Encoder;
use crate::message::HopBudget;
use crate::message::Message;
use crate::message::MessageRelay;
use crate::message::MessageSigner;
use crate::message::ProbeCompletion;
use crate::message::SignalingPath;
use crate::message::SigningDomain;
use crate::simulation::ProtectionProfile;
use crate::simulation::SimulationRuntimeGuard;

/// The wire version the goldens pin, and the Keccak-256 of the goldens, as hex.
///
/// Bump procedure: when this test fails, the change moved the wire. Bump [`WIRE_VERSION`] by
/// the rule of `frame` (once per release, relative to the last released version), then set
/// this pair to the new version and the hash the failure reports, in the same commit.
const PINNED: (u16, &str) = (
    2,
    "739213e7495519d29326482ab8d218d838178ec72af41dd04de294db0f943172",
);

/// The overlay every golden is signed in.
const NETWORK_ID: u32 = 1;
/// The instant the simulation clock holds while the goldens are built.
const GOLDEN_EPOCH_MS: u128 = 1_700_000_000_000;
/// The seed of the simulation's identifiers.
const GOLDEN_SEED: u64 = 1_107;
/// The transaction sequence of every golden payload.
const GOLDEN_SEQUENCE: u64 = 1;
/// The data every transcript golden signs.
const TRANSCRIPT_DATA: &[u8] = b"rings wire golden";
/// The lease left in the `Known` link-control golden.
const GOLDEN_LEASE_MS: u128 = 60_000;
/// The prefix of the labels only tests sign under, which no peer ever sees.
const TEST_LABEL_PREFIX: &str = "rings-core:test:";

/// One golden: what it pins, and its bytes.
type Golden = (String, Vec<u8>);

/// The fixed key of the 64-digit hex scalar `n`.
fn key(n: u8) -> Result<SecretKey> {
    SecretKey::try_from(format!("{n:064x}").as_str())
}

/// The session every golden is signed with: account key 0x11 over delegatee key 0x12.
fn session() -> Result<DelegateeKey> {
    DelegateeKey::from_test_keys(&key(0x11)?, key(0x12)?, GOLDEN_EPOCH_MS, 86_400_000)
}

/// The payload of `message` from `session` to the DID of key 0x21, carried along `relay` or a
/// fresh carrier toward it.
fn payload(
    session: &DelegateeKey,
    message: Message,
    relay: impl FnOnce(MessageRelay) -> Result<MessageRelay>,
) -> Result<MessagePayload> {
    let signer = MessageSigner::new(session, NETWORK_ID);
    let destination: Did = key(0x21)?.address().into();
    let transaction = Transaction::new(
        destination,
        uuid::Uuid::from_u128(0x1107),
        GOLDEN_SEQUENCE,
        None,
        message,
        signer,
    )?;
    let carrier = MessageRelay::new(NextHop::toward(destination), destination, HopBudget::MAX);
    MessagePayload::new(transaction, signer, relay(carrier)?)
}

/// One frame per [`Message`] variant: exhaustive, since the variants come from the table that
/// defines the enum.
fn message_goldens(session: &DelegateeKey) -> Result<Vec<Golden>> {
    Message::test_variants()?
        .into_iter()
        .map(|message| {
            let name = format!("message/{}", message.kind().as_str());
            Ok((name, payload(session, message, Ok)?.to_wire()?.to_vec()))
        })
        .collect()
}

/// A connect offer with no signaling.
fn connect_offer() -> Message {
    Message::ConnectNodeSend(ConnectNodeSend {
        sdp: "v=0".to_owned(),
        dht_protocol_mode: DhtProtocolMode::new(1, 1, 1),
        relay_custody: crate::dht::RelayCustody::Holds,
    })
}

/// The signaling path, recorded outbound through two hops and retraced on the reply.
fn signaling_goldens(session: &DelegateeKey) -> Result<Vec<Golden>> {
    let crossed = SignalingPath::outbound()
        .forwarded(key(0x31)?.address().into())?
        .forwarded(key(0x32)?.address().into())?;
    let reply = SignalingPath::Return(crossed.reply());
    [("signaling/outbound", crossed), ("signaling/return", reply)]
        .into_iter()
        .map(|(name, path)| {
            let framed = payload(session, connect_offer(), |relay| {
                Ok(relay.with_signaling_path(path))
            })?;
            Ok((name.to_owned(), framed.to_wire()?.to_vec()))
        })
        .collect()
}

/// One frame per [`LinkControl`] variant, checked against the variants serde declares.
fn link_control_goldens(session: &DelegateeKey) -> Result<Vec<Golden>> {
    let digest = session.delegation().digest()?;
    let controls = [
        LinkControl::Known {
            digest,
            remaining_ms: GOLDEN_LEASE_MS,
        },
        LinkControl::Unknown(digest),
    ];
    let named = controls
        .iter()
        .map(|control| Ok((variant_name(control)?, control.to_wire()?.to_vec())))
        .collect::<Result<Vec<_>>>()?;
    let covered = named
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        covered,
        declared_variants::<LinkControl>(),
        "every LinkControl variant needs exactly one golden, in declaration order"
    );
    Ok(named
        .into_iter()
        .map(|(name, bytes)| (format!("link-control/{name}"), bytes))
        .collect())
}

/// The transcript of every signing domain tag the workspace declares.
fn transcript_goldens() -> Result<Vec<Golden>> {
    let mut labels = domain_tag_labels()?;
    labels.retain(|label| !label.starts_with(TEST_LABEL_PREFIX));
    labels.sort();
    labels.dedup();
    assert!(!labels.is_empty(), "the workspace declares signing domains");
    Ok(labels
        .into_iter()
        .map(|label| {
            let name = format!("transcript/{label}");
            let tag = DomainTag::new(Box::leak(label.into_boxed_str()));
            let transcript = SigningDomain::new(tag, NETWORK_ID).transcript(
                TRANSCRIPT_DATA,
                GOLDEN_EPOCH_MS,
                86_400_000,
            );
            (name, transcript)
        })
        .collect())
}

/// The handshake string: the base58-check offer a node hands out of band.
fn handshake_goldens(session: &DelegateeKey) -> Result<Vec<Golden>> {
    let offer = payload(session, connect_offer(), Ok)?.encode()?;
    Ok(vec![(
        "handshake/offer".to_owned(),
        offer.to_string().into_bytes(),
    )])
}

/// The serde format of every type the frames reach from their roots, traced with every
/// variant of every enum: values sampled first for the types whose own `Deserialize` refuses
/// what the tracer would invent, then each root traced in full.
fn format_goldens(session: &DelegateeKey) -> Result<Vec<Golden>> {
    let reflect = |error: serde_reflection::Error| Error::InvalidMessage(error.to_string());
    // Invented values that every type's own `Deserialize` accepts: a byte string a UUID's
    // width, a string a valid public key (which `PublicKey` reads from its base58-check text,
    // and any `String` field takes), and a `u64` that is nonzero.
    let public_key =
        match serde_json::to_value(session.delegatee_public_key()).map_err(Error::Serialize)? {
            serde_json::Value::String(text) => text,
            other => return Err(Error::InvalidMessage(format!("a public key as {other}"))),
        };
    let config = TracerConfig::default()
        .default_borrowed_bytes_value(&[0; 16])
        .default_byte_buf_value(vec![0; 16])
        .default_u64_value(1)
        .default_string_value(public_key.clone())
        .default_borrowed_str_value(Box::leak(public_key.into_boxed_str()));
    let mut tracer = Tracer::new(config.record_samples_for_structs(true));
    // Replayed samples: a DID reads only its 20 bytes, and an entry version's serde alias
    // names a field its binary encoding does not have, which the tracer would read as a fifth
    // field; each is traced from a value instead.
    let did = session.delegator_did();
    let mut samples = Samples::new();
    tracer.trace_value(&mut samples, &did).map_err(reflect)?;
    let version = EntryVersion {
        logical_time_ms: GOLDEN_EPOCH_MS,
        counter: 1,
        actor: did,
        operation: did,
    };
    tracer
        .trace_value(&mut samples, &version)
        .map_err(reflect)?;
    // An account's keys differ in width, so no one invented string reads as all of them: the
    // delegation that holds the account is replayed from a sample, and the account is traced
    // from one value per variant, checked against the variants serde declares.
    tracer
        .trace_value(&mut samples, &session.delegation())
        .map_err(reflect)?;
    let accounts = account_samples(session)?;
    for account in &accounts {
        tracer.trace_value(&mut samples, account).map_err(reflect)?;
    }
    let sampled = accounts
        .iter()
        .map(variant_name)
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        sampled,
        declared_variants::<Account>(),
        "every Account variant needs one sample, in declaration order"
    );
    // The roots, then every enum they nest: the tracer completes only the enum it is asked
    // for, and the registry refuses an enum with a variant left untraced, so an enum nested
    // anew fails here until it is listed.
    macro_rules! trace {
        ($($ty:ty),+ $(,)?) => {
            $(tracer.trace_type::<$ty>(&samples).map_err(reflect)?;)+
        };
    }
    trace!(
        WirePayload<'_>,
        LinkControl,
        Message,
        ProbeCompletion,
        super::DelegationRef<'_>,
        crate::dht::entry::EntryKind,
        crate::dht::entry::EntryOperation,
        crate::message::FindSuccessorReportHandler,
        crate::message::FindSuccessorThen,
        crate::message::QueryFor,
        crate::dht::RelayCustody,
        crate::message::ServiceKind,
        SignalingPath,
        crate::dht::StorageSyncDestination,
        crate::dht::StorageSyncPurpose,
        crate::message::StoreOutcome,
    );
    let registry = tracer.registry().map_err(reflect)?;
    let bytes = serde_json::to_vec_pretty(&registry).map_err(Error::Serialize)?;
    Ok(vec![("format/registry".to_owned(), bytes)])
}

/// One account of each kind, keyed by `session`'s keys: their bytes need not verify, only
/// encode.
fn account_samples(session: &DelegateeKey) -> Result<Vec<Account>> {
    let did = session.delegator_did();
    let key = session.delegatee_public_key();
    let wide = PublicKey::<48>::from_exact_u8(&[0x48; 48])?;
    Ok(vec![
        Account::Secp256k1(did),
        Account::Secp256r1(key),
        Account::EIP191(did),
        Account::BIP137(did),
        Account::Ed25519(key),
        Account::Bls12381(wide),
    ])
}

/// Every golden, in a fixed order.
fn goldens() -> Result<Vec<Golden>> {
    let session = session()?;
    Ok([
        format_goldens(&session)?,
        message_goldens(&session)?,
        signaling_goldens(&session)?,
        link_control_goldens(&session)?,
        transcript_goldens()?,
        handshake_goldens(&session)?,
    ]
    .concat())
}

/// `keccak256` over every golden, each name and bytes length-prefixed, so no two lists hash
/// alike.
fn hash(goldens: &[Golden]) -> [u8; 32] {
    let mut transcript = Vec::new();
    for (name, bytes) in goldens {
        for part in [name.as_bytes(), bytes.as_slice()] {
            transcript.extend_from_slice(&(part.len() as u64).to_be_bytes());
            transcript.extend_from_slice(part);
        }
    }
    keccak256(&transcript)
}

/// Every `domain_tag!` label in the sources of the workspace's crates.
fn domain_tag_labels() -> Result<Vec<String>> {
    const MACRO: &str = "domain_tag!(\"";
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| Error::InvalidMessage("the core crate has a parent".to_owned()))?;
    let mut labels = Vec::new();
    for source in rust_sources(crates)? {
        let text = std::fs::read_to_string(&source)
            .map_err(|error| Error::InvalidMessage(format!("{}: {error}", source.display())))?;
        let mut rest = text.as_str();
        while let Some(start) = rest.find(MACRO) {
            rest = &rest[start + MACRO.len()..];
            let end = rest.find('"').ok_or_else(|| {
                Error::InvalidMessage(format!("unterminated label in {}", source.display()))
            })?;
            labels.push(rest[..end].to_owned());
        }
    }
    Ok(labels)
}

/// The `.rs` files under the `src` of every crate in `crates`.
fn rust_sources(crates: &Path) -> Result<Vec<PathBuf>> {
    let io = |error: std::io::Error| Error::InvalidMessage(error.to_string());
    let mut pending = std::fs::read_dir(crates)
        .map_err(io)?
        .map(|entry| entry.map(|entry| entry.path().join("src")).map_err(io))
        .collect::<Result<Vec<_>>>()?;
    let mut sources = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(io)? {
                pending.push(entry.map_err(io)?.path());
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
    Ok(sources)
}

/// The externally tagged name of `value`'s variant, as serde writes it.
fn variant_name(value: &impl Serialize) -> Result<String> {
    match serde_json::to_value(value).map_err(Error::Serialize)? {
        serde_json::Value::String(name) => Ok(name),
        serde_json::Value::Object(map) if map.len() == 1 => map
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| Error::InvalidMessage("an empty variant".to_owned())),
        other => Err(Error::InvalidMessage(format!(
            "not an enum variant: {other}"
        ))),
    }
}

/// The variants serde declares for the enum `T`, in declaration order: a deserializer that
/// records what `deserialize_enum` is handed and decodes nothing.
fn declared_variants<'de, T: Deserialize<'de>>() -> Vec<&'static str> {
    /// The probe, holding where the declared variants go.
    struct Probe<'a>(&'a mut &'static [&'static str]);

    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = serde::de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(
            self,
            _: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom(
                "the probe reads only an enum's variants",
            ))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            *self.0 = variants;
            Err(serde::de::Error::custom("the probe decodes nothing"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
            option unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier
            ignored_any
        }
    }

    let mut variants: &'static [&'static str] = &[];
    let _ = T::deserialize(Probe(&mut variants));
    variants.to_vec()
}

/// The wire, as the goldens see it, is what `PINNED` names for this [`WIRE_VERSION`].
#[test]
fn test_wire_goldens_are_pinned_to_the_wire_version() -> Result<()> {
    let _runtime =
        SimulationRuntimeGuard::enter(GOLDEN_SEED, GOLDEN_EPOCH_MS, ProtectionProfile::ALL_ENABLED)
            .map_err(|error| Error::InvalidMessage(format!("{error:?}")))?;
    let goldens = goldens()?;
    let observed = (WIRE_VERSION, hex::encode(hash(&goldens)));
    if observed != (PINNED.0, PINNED.1.to_owned()) {
        for (name, bytes) in &goldens {
            println!("{name}: {}", hex::encode(keccak256(bytes)));
        }
    }
    assert_eq!(
        observed,
        (PINNED.0, PINNED.1.to_owned()),
        "the wire moved: bump WIRE_VERSION by the rule of `frame` and pin the new hash in the \
         same commit (each golden's digest is printed above)"
    );
    Ok(())
}

/// The goldens are a function of the code alone: built twice, they hash alike.
#[test]
fn test_wire_goldens_are_deterministic() -> Result<()> {
    let build = || -> Result<[u8; 32]> {
        let _runtime = SimulationRuntimeGuard::enter(
            GOLDEN_SEED,
            GOLDEN_EPOCH_MS,
            ProtectionProfile::ALL_ENABLED,
        )
        .map_err(|error| Error::InvalidMessage(format!("{error:?}")))?;
        Ok(hash(&goldens()?))
    };
    assert_eq!(build()?, build()?);
    Ok(())
}
