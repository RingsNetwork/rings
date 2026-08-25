use super::*;
use crate::message::MessageClass;

const DHT_CONTROL_LANE: InboundLane = InboundLane::from_class(MessageClass::DhtControl);
const STORAGE_LANE: InboundLane = InboundLane::from_class(MessageClass::Storage);
const E2E_LANE: InboundLane = InboundLane::from_class(MessageClass::E2e);
const APPLICATION_LANE: InboundLane = InboundLane::from_class(MessageClass::Application);
const REASSEMBLY_LANE: InboundLane = InboundLane::REASSEMBLY;

#[test]
fn inbound_lane_mapping_is_total_and_reserves_one_extra_lane() {
    let lanes = [DHT_CONTROL_LANE, STORAGE_LANE, E2E_LANE, APPLICATION_LANE];

    assert_eq!(lanes.map(InboundLane::index), [0, 1, 2, 3]);
    assert_eq!(REASSEMBLY_LANE.index(), MessageClass::COUNT);
    assert!(!DHT_CONTROL_LANE.is_logical_data());
    assert!(!REASSEMBLY_LANE.is_logical_data());
    assert!(lanes
        .iter()
        .skip(1)
        .copied()
        .all(InboundLane::is_logical_data));
}

#[test]
fn reassembly_handoff_blocks_later_data_and_reassembly_until_first_poll() {
    let barrier = ReassemblyHandoffBarrier::new(7);

    assert!(!barrier.blocks(APPLICATION_LANE, 7));
    assert!(barrier.blocks(STORAGE_LANE, 8));
    assert!(barrier.blocks(REASSEMBLY_LANE, 9));
    assert!(!barrier.blocks(DHT_CONTROL_LANE, 10));
    assert!(!barrier.has_started());

    barrier.start_marker().store(true, Ordering::Release);
    assert!(barrier.has_started());
}

#[test]
fn maximum_transport_frame_fits_every_lane_reservation() {
    let reserved = memory_reservation(MAX_DATA_CHANNEL_MESSAGE_SIZE);
    assert!(reserved <= INBOUND_RESERVED_BYTES_PER_LANE);

    let capacity = Arc::new(InboundCapacity::new());
    let permits = [
        DHT_CONTROL_LANE,
        STORAGE_LANE,
        E2E_LANE,
        APPLICATION_LANE,
        REASSEMBLY_LANE,
    ]
    .into_iter()
    .enumerate()
    .map(|(peer, lane)| {
        capacity.acquire(
            Some(Did::from(u32::try_from(peer + 1).expect("test peer fits"))),
            lane,
            reserved,
        )
    })
    .collect::<Result<Vec<_>>>()
    .expect("every lane must admit one maximum transport frame from its reservation");

    assert_eq!(permits.len(), INBOUND_LANE_COUNT);
}

fn reserve_application_bytes(
    capacity: &Arc<InboundCapacity>,
    total: usize,
) -> Vec<InboundCapacityPermit> {
    let mut remaining = total;
    let mut peer = 1_u32;
    let mut permits = Vec::new();
    while remaining > 0 {
        let bytes = remaining.min(INBOUND_PEER_BYTE_CAPACITY);
        permits.push(
            capacity
                .try_acquire(Some(Did::from(peer)), APPLICATION_LANE, bytes)
                .expect("application blocker must fit within the declared limits"),
        );
        remaining -= bytes;
        peer = peer.saturating_add(1);
    }
    permits
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn inbound_capacity_reserves_count_for_every_lane() {
    let capacity = Arc::new(InboundCapacity::new());
    let application_limit =
        INBOUND_MAILBOX_CAPACITY - INBOUND_RESERVED_TRANSFERS_PER_LANE * (INBOUND_LANE_COUNT - 1);
    let mut permits = (0..application_limit)
        .map(|index| {
            capacity.try_acquire(
                Some(Did::from(
                    u32::try_from(index + 1).expect("test peer index fits"),
                )),
                APPLICATION_LANE,
                1,
            )
        })
        .collect::<Result<Vec<_>>>()
        .expect("application may borrow capacity not reserved for other lanes");

    assert!(matches!(
        capacity.try_acquire(Some(Did::from(u32::MAX)), APPLICATION_LANE, 1),
        Err(Error::InboundMailboxCapacityExceeded { .. })
    ));
    for (peer_index, lane) in [DHT_CONTROL_LANE, REASSEMBLY_LANE, STORAGE_LANE, E2E_LANE]
        .into_iter()
        .enumerate()
    {
        for _ in 0..INBOUND_RESERVED_TRANSFERS_PER_LANE {
            permits.push(
                capacity
                    .try_acquire(
                        Some(Did::from(
                            u32::try_from(application_limit + peer_index + 1)
                                .expect("test peer index fits"),
                        )),
                        lane,
                        1,
                    )
                    .expect("every lane retains its reserved admission capacity"),
            );
        }
    }
    assert!(matches!(
        capacity.try_acquire(Some(Did::from(u32::MAX)), DHT_CONTROL_LANE, 1),
        Err(Error::InboundMailboxCapacityExceeded { .. })
    ));
    drop(permits);
    assert!(capacity.try_acquire(None, APPLICATION_LANE, 1).is_ok());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn inbound_capacity_reserves_memory_for_every_lane() {
    let capacity = Arc::new(InboundCapacity::new());
    let application_limit =
        INBOUND_MAILBOX_BYTE_CAPACITY - INBOUND_RESERVED_BYTES_PER_LANE * (INBOUND_LANE_COUNT - 1);
    let first = capacity
        .try_acquire(
            Some(Did::from(1_u32)),
            APPLICATION_LANE,
            INBOUND_PEER_BYTE_CAPACITY,
        )
        .expect("one peer may retain one maximum-sized payload");
    let second = capacity
        .try_acquire(
            Some(Did::from(2_u32)),
            APPLICATION_LANE,
            application_limit - INBOUND_PEER_BYTE_CAPACITY,
        )
        .expect("application may borrow bytes not reserved for other lanes");

    assert!(matches!(
        capacity.try_acquire(Some(Did::from(3_u32)), APPLICATION_LANE, 1),
        Err(Error::InboundMailboxMemoryCapacityExceeded { .. })
    ));
    let control = capacity
        .try_acquire(
            Some(Did::from(3_u32)),
            DHT_CONTROL_LANE,
            INBOUND_RESERVED_BYTES_PER_LANE,
        )
        .expect("control retains its reserved byte budget");
    drop(first);
    drop(second);
    drop(control);
    assert!(capacity.try_acquire(None, APPLICATION_LANE, 1).is_ok());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn reassembly_handoff_rejects_capacity_pressure_without_waiting() {
    let capacity = Arc::new(InboundCapacity::new());
    let blockers = reserve_application_bytes(&capacity, 240 * 1024 * 1024);
    let mut handoff = capacity
        .try_acquire(Some(Did::from(2_u32)), REASSEMBLY_LANE, 1)
        .expect("reassembly retains its reserved byte budget");

    assert!(matches!(
        handoff.try_transition(APPLICATION_LANE, 16 * 1024 * 1024),
        Err(Error::InboundMailboxMemoryCapacityExceeded { .. })
    ));
    assert_eq!(handoff.lane, REASSEMBLY_LANE);
    assert_eq!(handoff.bytes, 1);

    drop(blockers);
    handoff
        .try_transition(APPLICATION_LANE, 16 * 1024 * 1024)
        .expect("failed transition must preserve the original reservation atomically");
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn reassembly_reservation_expands_before_logical_lane_transition() {
    let capacity = Arc::new(InboundCapacity::new());
    let peer = Some(Did::from(7_u32));
    let mut permit = capacity
        .try_acquire(peer, REASSEMBLY_LANE, 1)
        .expect("the raw chunk frame must fit in the reassembly lane");
    let decoded_bytes = INBOUND_PEER_BYTE_CAPACITY / 2;

    permit
        .try_transition(REASSEMBLY_LANE, decoded_bytes)
        .expect("the completed payload must replace the raw-frame reservation");

    assert!(matches!(
        capacity.try_acquire(
            peer,
            APPLICATION_LANE,
            INBOUND_PEER_BYTE_CAPACITY - decoded_bytes + 1,
        ),
        Err(Error::InboundPeerMemoryCapacityExceeded { .. })
    ));
    assert_eq!(permit.lane, REASSEMBLY_LANE);
    assert_eq!(permit.bytes, decoded_bytes);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn pending_ingress_ticket_blocks_later_same_lane_sequence() {
    let mut queues = InboundQueues::new();
    queues.push_pending(4, APPLICATION_LANE);
    queues.push_pending(5, APPLICATION_LANE);

    assert_eq!(queues.front_sequence(APPLICATION_LANE), Some(4));
    assert!(queues.pop(APPLICATION_LANE).is_none());

    queues.cancel(4, APPLICATION_LANE);
    assert_eq!(queues.front_sequence(APPLICATION_LANE), Some(5));
    assert!(queues.pop(APPLICATION_LANE).is_none());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn one_peer_cannot_exhaust_another_peers_inbound_allowance() {
    let capacity = Arc::new(InboundCapacity::new());
    let noisy_peer = Some(Did::from(1_u32));
    let control_peer = Some(Did::from(2_u32));
    let permits = (0..INBOUND_PEER_CAPACITY)
        .map(|_| capacity.try_acquire(noisy_peer, APPLICATION_LANE, 1))
        .collect::<Result<Vec<_>>>()
        .expect("one peer may use exactly its own count allowance");

    assert!(matches!(
        capacity.try_acquire(noisy_peer, APPLICATION_LANE, 1),
        Err(Error::InboundPeerCapacityExceeded {
            peer,
            capacity: INBOUND_PEER_CAPACITY,
        }) if peer == noisy_peer
    ));
    assert!(capacity
        .try_acquire(control_peer, DHT_CONTROL_LANE, 1)
        .is_ok());
    drop(permits);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn inbound_callback_failures_preserve_typed_sources() {
    let validation = inbound_failure_error(InboundFailure::Validation(Box::new(
        std::io::Error::other("validation source"),
    )));
    let callback = inbound_failure_error(InboundFailure::Callback(Box::new(
        std::io::Error::other("callback source"),
    )));

    assert!(std::error::Error::source(&validation)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some());
    assert!(std::error::Error::source(&callback)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some());
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[test]
fn native_callback_error_contract_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<CallbackError>();
    let _: CallbackError = Box::new(std::io::Error::other("native callback error"));
}
