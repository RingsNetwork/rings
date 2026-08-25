use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
use crate::dht::successor::SuccessorReader;
#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
use crate::dht::topology;
#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "wasm", target_family = "wasm")
))]
use crate::dht::Did;
use crate::error::Result;
use crate::message::Encoder;
use crate::message::MessageClass;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::SwarmTransport;
use crate::swarm::Swarm;

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

pub fn multi_frame_storage_sync_entries() -> Result<Vec<PlacedEntry>> {
    let topic = "shared multi-frame storage contention";
    let entry_did = Entry::gen_did(topic)?;
    let payload = vec![0xcd; 1024 * 1024].encode()?;
    let entry = Entry::new(entry_did, vec![payload], EntryKind::Data);
    Ok(vec![PlacedEntry::new(entry_did, entry)])
}

pub fn control_interleaves_transfer(
    trace: &[(MessageClass, u64, usize)],
    data_class: MessageClass,
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
                            .any(|event| event.0 == MessageClass::DhtControl)
                })
    })
}

pub fn assert_control_interleaves_transfer(
    trace: &[(MessageClass, u64, usize)],
    data_class: MessageClass,
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

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod structured_log_assertion_tests {
    use super::assert_single_structured_log_event;

    const TARGET: &str = "rings_core::tests::structured";
    const EVENT: &str = "expected event";

    fn assert_event(lines: &[&str]) -> std::result::Result<(), String> {
        assert_single_structured_log_event(
            lines,
            TARGET,
            EVENT,
            ("tx_id", "wanted"),
            &[("value", "1".to_owned())],
            &[],
        )
    }

    #[test]
    fn different_event_with_matching_suffix_is_not_selected() {
        let impostor =
            "scope: rings_core::tests::structured: different: expected event tx_id=wanted value=1";

        assert!(assert_event(&[impostor]).is_err());
    }

    #[test]
    fn duplicate_event_with_wrong_transaction_is_rejected_before_field_validation() {
        let valid = "scope: rings_core::tests::structured: expected event tx_id=wanted value=1";
        let duplicate = "scope: rings_core::tests::structured: expected event tx_id=other value=1";

        assert!(assert_event(&[valid, duplicate]).is_err());
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub fn outbound_capacity_released(transport: &SwarmTransport, peer: Did) -> bool {
    matches!(
        transport.outbound_admitted_transfer_count_for_test(peer),
        None | Some(0)
    )
}
