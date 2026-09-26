#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
use num_bigint::BigUint;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use crate::delegation::DelegateeKey;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use crate::dht::entry::inbox::HeldMessage;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
use crate::dht::topology;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message::Encoded;
use crate::message::Encoder;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use crate::message::Message;
use crate::message::MessageCategory;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use crate::message::MessagePayload;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use crate::message::MessageSigner;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;
use crate::utils::get_epoch_ms;

/// Overlay every test fixture signs for and verifies against.
pub(crate) const TEST_NETWORK_ID: u32 = 0;

/// A delegatee key delegated for `ttl_ms` by a fresh account: with a small `ttl_ms`, a delegation
/// that expires under a clock advanced past it.
pub(crate) fn delegatee_key_with_ttl(ttl_ms: u64) -> Result<crate::delegation::DelegateeKey> {
    let account = crate::ecc::SecretKey::random();
    let delegator_did: Did = account.address().into();
    let builder = crate::delegation::DelegationBuilder::new(
        delegator_did.to_string(),
        "secp256k1".to_string(),
    )
    .set_ttl(ttl_ms);
    let sig = account.sign(&builder.unsigned_proof())?.to_vec();
    builder.set_delegator_signature(sig).build()
}

/// Retention bound far enough ahead that a fixture stays live for a whole test.
const FIXTURE_RETENTION_MS: u128 = 60 * 60 * 1_000;

/// An entry with a retention bound one hour ahead, inside the admission maximum, so a fixture
/// that is written to storage or carried in a sync message passes storage admission.
pub(crate) fn live_entry(did: Did, data: Vec<Encoded>, kind: EntryKind) -> Entry {
    live(Entry::new(did, data, kind))
}

/// Stamp an existing fixture with a live retention bound.
pub(crate) fn live(entry: Entry) -> Entry {
    with_retention(entry, get_epoch_ms() + FIXTURE_RETENTION_MS)
}

/// Stamp an existing fixture with the retention bound `expires_at_ms`.
pub(crate) fn with_retention(mut entry: Entry, expires_at_ms: u128) -> Entry {
    entry.expires_at_ms = Some(expires_at_ms);
    entry
}

/// Stamp an existing fixture with a retention bound that has already elapsed.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) fn expired(entry: Entry) -> Entry {
    with_retention(entry, 1)
}

/// A live inbox delta for `destination`: one custom message from a fresh sender, held now by
/// `holder` inside [`TEST_NETWORK_ID`].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) fn held_inbox_for(destination: Did, holder: &DelegateeKey) -> Result<Entry> {
    let sender = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let payload = MessagePayload::new_send(
        Message::custom(b"held")?,
        MessageSigner::new(&sender, TEST_NETWORK_ID),
        destination,
        destination,
    )?;
    let held = HeldMessage::hold(
        payload,
        MessageSigner::new(holder, TEST_NETWORK_ID),
        get_epoch_ms(),
    )?;
    Ok(live(Entry::inbox_delta(&held)?))
}

pub(crate) mod activity;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub mod wasm;

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub mod default;

#[allow(dead_code)]
pub fn setup_tracing() {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

pub async fn manually_establish_connection(swarm1: &Swarm, swarm2: &Swarm) {
    assert!(swarm1.transport.get_connection(swarm2.did()).is_none());
    assert!(swarm2.transport.get_connection(swarm1.did()).is_none());

    let offer = swarm1.create_offer(swarm2.did()).await.unwrap();
    let answer = swarm2.answer_offer(offer).await.unwrap();
    swarm1.accept_answer(answer).await.unwrap();
}

#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
pub fn ring_topology_converged(nodes: &[&Swarm]) -> Result<bool> {
    let members: Vec<Did> = nodes.iter().map(|node| node.did()).collect();
    for node in nodes {
        let expected_successor = topology::successors(&members, node.did(), 1)
            .into_iter()
            .next()
            .unwrap_or(node.did());
        let expected_predecessor = topology::predecessor(&members, node.did());
        let observed_predecessor = node.dht().topology_state()?.predecessor;
        if node.dht().successors().min()? != expected_successor
            || observed_predecessor != expected_predecessor
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn replace_observed_fingers(swarm: &Swarm, fingers: &[(usize, Did)]) -> Result<()> {
    swarm.dht().replace_fingers_for_test(fingers)
}

#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
pub(crate) fn midpoint_storage_key(local: Did, lower: Did, upper: Did) -> Did {
    let midpoint =
        (topology::dist(local, lower) + topology::dist(local, upper)) / BigUint::from(2_u8);
    local + Did::from(midpoint)
}

#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
pub(crate) fn tail_storage_key(local: Did, lower: Did) -> Did {
    let ring_size = BigUint::from(1_u8) << topology::RING_BITS;
    let midpoint = (topology::dist(local, lower) + ring_size) / BigUint::from(2_u8);
    local + Did::from(midpoint)
}

pub fn multi_frame_storage_sync_entries() -> Result<Vec<PlacedEntry>> {
    let topic = "shared multi-frame storage contention";
    let entry_did = Entry::gen_did(topic)?;
    let payload = vec![0xcd; 1024 * 1024].encode()?;
    let entry = live_entry(entry_did, vec![payload], EntryKind::Data);
    Ok(vec![PlacedEntry::new(entry_did, entry)])
}

pub fn control_interleaves_transfer(
    trace: &[(MessageCategory, u64, usize)],
    data_class: MessageCategory,
) -> bool {
    trace.iter().enumerate().any(|(first_index, first)| {
        first.0 == data_class
            && trace
                .iter()
                .enumerate()
                .skip(first_index.saturating_add(1))
                .any(|(later_index, later)| {
                    later.0 == data_class
                        && later.1 == first.1
                        && later.2 > first.2
                        && trace[first_index.saturating_add(1)..later_index]
                            .iter()
                            .any(|event| event.0 == MessageCategory::DhtControl)
                })
    })
}

/// Frames of `category` admitted in `trace`.
#[cfg(any(
    all(feature = "std", not(feature = "dummy")),
    all(feature = "wasm", target_family = "wasm")
))]
pub fn frame_count(trace: &[(MessageCategory, u64, usize)], category: MessageCategory) -> usize {
    trace.iter().filter(|event| event.0 == category).count()
}

/// Whether a control round may send its next control: the `data_class` transfer moved on past
/// the control just sent, which is traced once `trace` holds more than `controls_before`
/// control frames.
///
/// ```text
/// progressed ≡ (#control(trace) > controls_before ∧ ∃ data frame after the last control)
///              ∨ interleaves(trace) ∨ transfers_in_flight = 0
/// ```
///
/// Position, not a count snapshot, decides the first disjunct, so a data frame admitted before
/// the control never satisfies it. The trace does not tell the test's controls from other
/// `DhtControl` traffic on the link: when the test's controls are the only such traffic, as in
/// the native fixture, consecutive controls always have a data frame between them; when other
/// control traffic shares the link, as in the browser soak's maintenance, the data frame follows
/// the latest control of any origin, which paces the rounds but no longer guarantees it.
/// The control's own activity cannot satisfy it either, and evaluating it only reads the trace,
/// so it sends nothing. The last disjunct ends the rounds once the transfer is done and nothing
/// more can interleave.
#[cfg(any(
    all(feature = "std", not(feature = "dummy")),
    all(feature = "wasm", target_family = "wasm")
))]
pub fn data_transfer_progressed(
    trace: &[(MessageCategory, u64, usize)],
    data_class: MessageCategory,
    controls_before: usize,
    transfers_in_flight: usize,
) -> bool {
    let data_follows_last_control = trace
        .iter()
        .rposition(|event| event.0 == MessageCategory::DhtControl)
        .is_some_and(|last_control| {
            trace
                .iter()
                .skip(last_control.saturating_add(1))
                .any(|event| event.0 == data_class)
        });
    (frame_count(trace, MessageCategory::DhtControl) > controls_before && data_follows_last_control)
        || control_interleaves_transfer(trace, data_class)
        || transfers_in_flight == 0
}

pub fn assert_control_interleaves_transfer(
    trace: &[(MessageCategory, u64, usize)],
    data_class: MessageCategory,
) {
    assert!(
        control_interleaves_transfer(trace, data_class),
        "control must run between frames of one data transfer: {trace:?}"
    );
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn byte_debug_fragment(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn structured_log_fields<'line>(
    line: &'line str,
    event_marker: &str,
) -> Option<Vec<(&'line str, &'line str)>> {
    let (_, fields) = line.rsplit_once(event_marker)?;
    fields
        .split_ascii_whitespace()
        .map(|field| field.split_once('='))
        .collect()
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn field_occurs_once(fields: &[(&str, &str)], expected: (&str, &str)) -> bool {
    let mut matching = fields.iter().filter(|(key, _)| *key == expected.0);
    matches!(matching.next(), Some((_, value)) if *value == expected.1) && matching.next().is_none()
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn assert_single_structured_log_event(
    lines: &[&str],
    target: &str,
    event_message: &str,
    unique_field: (&str, &str),
    expected_fields: &[(&str, String)],
    forbidden_fragments: &[&str],
) -> std::result::Result<(), String> {
    let event_marker = format!(" {target}: {event_message} ");
    let matching = lines
        .iter()
        .filter(|line| line.contains(&event_marker))
        .copied()
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(format!(
            "expected one `{target}: {event_message}` event, found {}",
            matching.len()
        ));
    };
    let Some(fields) = structured_log_fields(event, &event_marker) else {
        return Err(format!(
            "structured event fields could not be parsed: {event}"
        ));
    };
    if !field_occurs_once(&fields, unique_field) {
        return Err(format!(
            "structured event omitted unique `{}={}`: {event}",
            unique_field.0, unique_field.1
        ));
    }
    let expected_field_count = expected_fields.len().saturating_add(1);
    if fields.len() != expected_field_count {
        return Err(format!(
            "structured event contained {} fields, expected {expected_field_count}: {event}",
            fields.len()
        ));
    }
    for (expected_key, expected_value) in expected_fields {
        if !field_occurs_once(&fields, (*expected_key, expected_value.as_str())) {
            return Err(format!(
                "structured event omitted `{expected_key}={expected_value}`: {event}"
            ));
        }
    }
    for fragment in forbidden_fragments {
        if event.contains(fragment) {
            return Err(format!(
                "structured event contained forbidden `{fragment}`: {event}"
            ));
        }
        if let Some(leaking_line) = lines.iter().find(|line| line.contains(fragment)) {
            return Err(format!(
                "log scope contained forbidden `{fragment}`: {leaking_line}"
            ));
        }
    }
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn outbound_capacity_released(transport: &SwarmTransport, peer: Did) -> bool {
    matches!(
        transport.outbound_admitted_transfer_count_for_test(peer),
        None | Some(0)
    )
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod test_structured_log_assertion;

/// Hex secrets of [`fixed_secret_keys`]; a request for `N` keys takes the first `N`.
const FIXED_SECRET_KEY_HEX: [&str; 4] = [
    "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0",
    "1f9275dbafdfba81942eb3330b07f38cbee4ebb86bdc2174af9648d5f5509a54",
    "27b2fe8ceaf3a6a720f12658301351960b128672e9da4d6f4dead366af3fd834",
    "4a1c8e3f0b7d2965e8a13c57f09b4d26e7c1a85f3b0d9e624c7a18f5d03b6e92",
];

/// The first `N ≤ 4` fixed identities, in ascending address order, so fixtures that depend
/// on ring placement are the same on every run.
pub fn fixed_secret_keys<const N: usize>() -> Result<[SecretKey; N]> {
    let mut keys = FIXED_SECRET_KEY_HEX
        .iter()
        .take(N)
        .map(|hex| SecretKey::try_from(*hex))
        .collect::<Result<Vec<_>>>()?;
    keys.sort_by_key(|key| key.address());
    keys.try_into().map_err(|_| {
        crate::error::Error::InvalidMessage(format!("at most 4 fixed keys, {N} requested"))
    })
}
