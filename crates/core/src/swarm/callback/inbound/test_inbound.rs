use futures::FutureExt;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

use super::capacity::InboundRefusal;
use super::*;
use crate::message::MessageCategory;

const DHT_CONTROL_LANE: InboundLane = InboundLane::from_class(MessageCategory::DhtControl);
const STORAGE_LANE: InboundLane = InboundLane::from_class(MessageCategory::Storage);
const E2E_LANE: InboundLane = InboundLane::from_class(MessageCategory::E2e);
const APPLICATION_LANE: InboundLane = InboundLane::from_class(MessageCategory::Application);
const REASSEMBLY_LANE: InboundLane = InboundLane::Reassembly;

#[test]
fn test_inbound_lane_mapping_is_total_and_reserves_one_extra_lane() {
    let lanes = [DHT_CONTROL_LANE, STORAGE_LANE, E2E_LANE, APPLICATION_LANE];

    assert_eq!(lanes.map(InboundLane::index), [0, 1, 2, 3]);
    assert_eq!(REASSEMBLY_LANE.index(), MessageCategory::COUNT);
    assert!(!DHT_CONTROL_LANE.is_logical_data());
    assert!(!REASSEMBLY_LANE.is_logical_data());
    assert!(lanes
        .iter()
        .skip(1)
        .copied()
        .all(InboundLane::is_logical_data));
}

#[test]
fn test_reassembly_handoff_blocks_reassembly_and_orders_only_data_lanes() {
    let barrier = ReassemblyHandoffBarrier::new(7);

    // The barrier itself gates only the reassembly lane; data lanes are ordered
    // against it by the actor's barrier sequence, control lanes never wait.
    assert!(barrier.blocks(REASSEMBLY_LANE));
    assert!(!barrier.blocks(STORAGE_LANE));
    assert!(!barrier.blocks(APPLICATION_LANE));
    assert!(!barrier.blocks(DHT_CONTROL_LANE));
    assert!(lane_waits_for_reassembly(STORAGE_LANE));
    assert!(lane_waits_for_reassembly(APPLICATION_LANE));
    assert!(!lane_waits_for_reassembly(DHT_CONTROL_LANE));
    assert!(!barrier.has_started());

    barrier.start_marker().store(true, Ordering::Release);
    assert!(barrier.has_started());
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[test]
fn test_barrier_exemption_ablation_blocks_real_control_lane() {
    let _runtime = crate::simulation::SimulationRuntimeGuard::enter(
        43,
        1_700_000_000_000,
        crate::simulation::ProtectionProfile::without_barrier_control_exemption(),
    )
    .expect("simulation runtime must install");
    assert!(lane_waits_for_reassembly(DHT_CONTROL_LANE));
}

#[test]
fn test_maximum_transport_frame_fits_every_lane_reservation() {
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
        // Each lane's reservation admits at once: the acquisition completes without waiting.
        capacity
            .acquire(
                (
                    Some(Did::from(u32::try_from(peer + 1).expect("test peer fits"))),
                    lane,
                    None,
                ),
                InboundCharge::Bytes(reserved),
            )
            .now_or_never()
            .unwrap_or(Err(Error::InboundMailboxClosed))
    })
    .collect::<Result<Vec<_>>>()
    .expect("every lane must admit one maximum transport frame from its reservation");

    assert_eq!(permits.len(), INBOUND_LANE_COUNT);
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_inbound_capacity_reserves_count_for_every_lane() {
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
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("application may borrow capacity not reserved for other lanes");

    assert!(matches!(
        capacity.try_acquire(Some(Did::from(u32::MAX)), APPLICATION_LANE, 1),
        Err(InboundRefusal::Mailbox)
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
        Err(InboundRefusal::Mailbox)
    ));
    drop(permits);
    assert!(capacity.try_acquire(None, APPLICATION_LANE, 1).is_ok());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_inbound_capacity_reserves_memory_for_every_lane() {
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
        Err(InboundRefusal::Mailbox)
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

/// A covered arrival, a later chunk of a message its first chunk was charged for, takes nothing
/// from the ledger: it is admitted at once while every budget is full (#932).
#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_a_covered_arrival_is_admitted_while_the_ledger_is_full() {
    let capacity = Arc::new(InboundCapacity::new());
    let peer = Some(Did::from(1_u32));
    let blocker = capacity
        .try_acquire(peer, REASSEMBLY_LANE, INBOUND_PEER_BYTE_CAPACITY)
        .expect("one peer may take its whole budget");
    assert_eq!(
        capacity.try_acquire(peer, REASSEMBLY_LANE, 1).err(),
        Some(InboundRefusal::Peer)
    );
    let covered = capacity
        .acquire((peer, REASSEMBLY_LANE, None), InboundCharge::Covered)
        .now_or_never()
        .expect("a covered arrival never waits for capacity")
        .expect("a covered arrival is never refused");
    drop(covered);
    drop(blocker);
}

/// No later chunk waits behind another class's message: arrivals are ordered per transport
/// carrier, so a covered chunk of one message is admitted while the first chunk of a message on
/// another carrier of the same peer waits for the capacity the first message holds. Ordered per
/// lane alone, the covered chunk would queue behind it, and neither message could complete.
#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_a_later_chunk_never_waits_behind_another_carriers_first_chunk() {
    let capacity = Arc::new(InboundCapacity::new());
    let peer = Some(Did::from(1_u32));
    let (storage, application) = (Some(ChannelLane::new(1)), Some(ChannelLane::new(3)));
    let first = capacity
        .acquire(
            (peer, REASSEMBLY_LANE, storage),
            InboundCharge::Bytes(INBOUND_PEER_BYTE_CAPACITY),
        )
        .now_or_never()
        .expect("the first message's first chunk fits the peer's budget")
        .expect("within the request bounds");
    let mut other = Box::pin(capacity.acquire(
        (peer, REASSEMBLY_LANE, application),
        InboundCharge::Bytes(1024),
    ));
    assert!(
        (&mut other).now_or_never().is_none(),
        "the other message's first chunk waits for the peer's budget"
    );
    let later = capacity
        .acquire((peer, REASSEMBLY_LANE, storage), InboundCharge::Covered)
        .now_or_never()
        .expect("the first message's later chunk never waits behind it")
        .expect("a covered arrival is never refused");
    drop(later);
    drop(first);
    assert!(
        other.now_or_never().is_some_and(|granted| granted.is_ok()),
        "the waiting first chunk is granted once the first message releases its charge"
    );
}

/// `take` moves a charge without releasing it: the taken charge holds it, the permit left in
/// its place holds nothing, and the charge is released once, when the taken charge drops. The
/// event ends with its permit, whoever holds the charge.
#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_taking_a_charge_moves_it_without_releasing_it() {
    let capacity = Arc::new(InboundCapacity::new());
    let peer = Some(Did::from(7_u32));
    let mut permit = capacity
        .try_acquire(peer, REASSEMBLY_LANE, INBOUND_PEER_BYTE_CAPACITY)
        .expect("one peer may take its whole budget");
    let taken = permit.take();
    assert_eq!(capacity.live_events_for_test(), 1);
    drop(permit);
    assert_eq!(capacity.live_events_for_test(), 0, "the event ended");
    assert_eq!(
        capacity.try_acquire(peer, REASSEMBLY_LANE, 1).err(),
        Some(InboundRefusal::Peer),
        "dropping the covered remainder releases nothing"
    );
    drop(taken);
    assert!(capacity.try_acquire(peer, REASSEMBLY_LANE, 1).is_ok());
}

#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_pending_ingress_ticket_blocks_later_same_lane_sequence() {
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
fn test_one_peer_cannot_exhaust_another_peers_inbound_allowance() {
    let capacity = Arc::new(InboundCapacity::new());
    let noisy_peer = Some(Did::from(1_u32));
    let control_peer = Some(Did::from(2_u32));
    let permits = (0..INBOUND_PEER_CAPACITY)
        .map(|_| capacity.try_acquire(noisy_peer, APPLICATION_LANE, 1))
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("one peer may use exactly its own count allowance");

    assert!(matches!(
        capacity.try_acquire(noisy_peer, APPLICATION_LANE, 1),
        Err(InboundRefusal::Peer)
    ));
    assert!(capacity
        .try_acquire(control_peer, DHT_CONTROL_LANE, 1)
        .is_ok());
    drop(permits);
}
