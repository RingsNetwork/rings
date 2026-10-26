//! The open-message table on the inbound ledger (load-bounds paper, Def. 6, Law 4, Prop. 5):
//! first chunks charged to their slots under Law 1, waiting in resource order, and released when
//! their message completes or expires.

use futures::FutureExt;
use rings_transport::core::pool::ChannelLane;

use super::capacity::InboundRefusal;
use super::*;

/// The retained bytes of the largest honest message, `r(n·d₀)` for 60 MB.
const LARGEST: usize = 120_000_000;

/// A first chunk's charge on the application lane, introduced by slot `slot`.
const fn opening(bytes: usize, slot: u16) -> InboundCharge {
    InboundCharge::Message {
        bytes,
        lane: InboundLane::Application,
        introducer: Principal::Slot(slot),
    }
}

/// Acquire `charge` for `peer` on the reassembly lane through the application carrier, at
/// once, or `None` while it waits.
fn try_now(
    capacity: &Arc<InboundCapacity>,
    peer: u32,
    charge: InboundCharge,
) -> Option<InboundCapacityPermit> {
    capacity
        .acquire(
            (
                Some(Did::from(peer)),
                InboundLane::Reassembly,
                Some(ChannelLane::new(3)),
            ),
            charge,
        )
        .now_or_never()
        .map(|granted| granted.expect("within the request bounds"))
}

/// An attacker that re-reserves after its open message expired queues behind the honest claim
/// already waiting: the pool of `O` serves its claims in resource order, so the honest message
/// waits at most for the attacker's one open message (Prop. 5(2)).
#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_a_re_reservation_after_expiry_queues_behind_a_waiting_honest_claim() {
    let capacity = Arc::new(InboundCapacity::new());
    let (attacker, honest) = ((1, 1_u16), (2, 2_u16));
    let open = try_now(&capacity, attacker.0, opening(LARGEST, attacker.1))
        .expect("alone, the attacker's claim is the whole pool");

    let mut waiting = Box::pin(capacity.acquire(
        (
            Some(Did::from(honest.0)),
            InboundLane::Reassembly,
            Some(ChannelLane::new(3)),
        ),
        opening(LARGEST, honest.1),
    ));
    assert!(
        (&mut waiting).now_or_never().is_none(),
        "beyond its fair claim, the honest message waits"
    );
    let mut again = Box::pin(capacity.acquire(
        (
            Some(Did::from(attacker.0)),
            InboundLane::Reassembly,
            Some(ChannelLane::new(1)),
        ),
        opening(LARGEST, attacker.1),
    ));
    assert!((&mut again).now_or_never().is_none());

    // The attacker's message expires: its charge is dropped.
    drop(open);
    assert_eq!(capacity.open_bytes_for_test(Principal::Slot(attacker.1)), 0);
    let honest_permit = waiting
        .now_or_never()
        .expect("the waiting honest claim is served first")
        .expect("within the request bounds");
    assert_eq!(
        capacity.open_bytes_for_test(Principal::Slot(honest.1)),
        LARGEST
    );
    assert!(
        (&mut again).now_or_never().is_none(),
        "the re-reservation waits behind the honest message"
    );
    drop(honest_permit);
    assert!(again.now_or_never().is_some_and(|granted| granted.is_ok()));
}

/// A first chunk within its slot's floor is admitted at once while another slot holds the whole
/// pool of `O`, and every arrival other than a first chunk never waits for `O` (Prop. 5(1)).
#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_a_first_chunk_within_its_floor_never_waits_for_the_pool() {
    let capacity = Arc::new(InboundCapacity::new());
    let _attacker = try_now(&capacity, 1, opening(LARGEST, 1)).expect("the pool, alone");
    assert!(
        try_now(&capacity, 2, opening(LARGEST, 2)).is_none(),
        "a large message of another slot waits for the pool"
    );
    assert!(
        try_now(&capacity, 3, opening(200 * 1024, 3)).is_some(),
        "a message within its slot's floor does not"
    );
    assert!(try_now(&capacity, 4, InboundCharge::Bytes(1024)).is_some());
}

/// At completion a message leaves `O`, and its mailbox reservation moves with it onto the event
/// that dispatches it: a move, not a release and a new charge (Law 4).
#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_completion_moves_the_charge_out_of_the_open_message_table() {
    let capacity = Arc::new(InboundCapacity::new());
    let slot = Principal::Slot(5);
    let mut event = try_now(&capacity, 5, opening(LARGEST, 5)).expect("the pool, alone");
    let message = event.take();
    assert_eq!(capacity.open_bytes_for_test(slot), LARGEST);

    event.restore(message);
    assert_eq!(capacity.open_bytes_for_test(slot), 0, "the message closed");
    assert_eq!(
        capacity
            .try_acquire(Some(Did::from(5_u32)), InboundLane::Application, LARGEST)
            .err(),
        Some(InboundRefusal::Peer),
        "its mailbox reservation is still held, by the dispatching event"
    );
    drop(event);
    assert!(capacity
        .try_acquire(Some(Did::from(5_u32)), InboundLane::Application, LARGEST)
        .is_ok());
}

/// A first chunk is charged on its carrier's class lane, where its message is dispatched once
/// complete; without a carrier, on the reassembly lane.
#[cfg_attr(
    all(feature = "wasm", target_family = "wasm"),
    wasm_bindgen_test::wasm_bindgen_test
)]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), test)]
fn test_a_first_chunk_is_charged_on_its_carriers_class_lane() {
    for lane in InboundLane::ALL {
        let Some(class) = lane.class() else {
            continue;
        };
        assert_eq!(InboundLane::of_carrier(Some(class.channel_lane())), lane);
    }
    assert_eq!(InboundLane::of_carrier(None), InboundLane::Reassembly);
}
