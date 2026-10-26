//! A directed witness of the batch advertisement; `test_credit_model` checks the credit laws
//! over every interleaving.

use super::CreditWindow;
use super::ReceiveWindow;
use super::SendCredit;

/// Releasing a batch advertises `released + W`, and granting it unblocks the sender for exactly
/// the released frames.
#[test]
fn test_releasing_a_batch_advertises_released_plus_window() {
    let window = CreditWindow::new(4, 2);
    let mut sender = SendCredit::new(window);
    let mut receiver = ReceiveWindow::new(window);
    for _ in 0..4 {
        assert!(sender.try_reserve());
        sender.commit();
        assert_eq!(receiver.admit(), Ok(()));
    }
    receiver.release();
    assert_eq!(receiver.advertise(window), None);
    receiver.release();
    assert_eq!(receiver.advertise(window), Some((6, 0)));

    sender.grant(6, 0);
    assert!(sender.try_reserve());
    assert!(sender.try_reserve());
    assert!(!sender.try_reserve());
}

/// The credit a sender holds unused, as its receiver counts it, leaves out what the receiver
/// holds back: a frame it still holds, and a release awaiting its batch. A receiver's rate floor
/// reads it, so its clock pauses while the receiver throttles the sender.
#[test]
fn test_unused_credit_leaves_out_what_the_receiver_holds_back() {
    let window = CreditWindow::new(4, 2);
    let mut receiver = ReceiveWindow::new(window);
    assert_eq!(receiver.unused(), 4);
    for _ in 0..4 {
        assert_eq!(receiver.admit(), Ok(()));
    }
    assert_eq!(receiver.unused(), 0, "the receiver holds every frame");
    receiver.release();
    assert_eq!(receiver.advertise(window), None);
    assert_eq!(receiver.unused(), 0, "a release awaiting its batch");
    receiver.release();
    assert_eq!(receiver.advertise(window), Some((6, 0)));
    assert_eq!(receiver.unused(), 2);
}

/// Def. Reclaim, directed: a shrink to `L′` is answered with `B = max(c, L′)`, the sender's new
/// limit even above a credit in transit, which belongs to the older generation and is ignored;
/// the receiver counts `B` once the return arrives, and grants its floor again from there.
#[test]
fn test_a_shrink_is_answered_with_a_bound_both_ends_keep() {
    let window = CreditWindow::pooled(4, 1, 2);
    let mut sender = SendCredit::new(window);
    let mut receiver = ReceiveWindow::new(window);
    assert_eq!(receiver.lend(window, 3), 3);
    assert_eq!(receiver.advertise(window), Some((4, 0)));
    sender.grant(4, 0);
    assert!(sender.try_reserve());
    sender.commit();
    assert_eq!(receiver.admit(), Ok(()));
    receiver.release();

    // Two idle checks: the first sees the admission, the second shrinks.
    assert_eq!(receiver.idle_shrink(window), None);
    let Some(super::LaneSignal::Shrink { limit, generation }) = receiver.idle_shrink(window) else {
        panic!("an idle lane holding a share is shrunk");
    };
    assert_eq!((limit, generation), (2, 1));
    assert_eq!(
        receiver.advertise(window),
        None,
        "nothing while the shrink is pending"
    );

    let bound = sender
        .shrink(limit, generation)
        .expect("a newer shrink is answered");
    assert_eq!(bound, 2, "max(c = 1, L′ = 2)");
    assert_eq!(
        sender.shrink(limit, generation),
        Some(bound),
        "a duplicate is answered alike"
    );
    sender.grant(4, 0);
    assert_eq!(
        sender.free(),
        1,
        "a credit of the older generation is ignored"
    );

    assert!(receiver.returned(bound, generation));
    assert!(
        !receiver.returned(bound, generation),
        "the shrink ended once"
    );
    assert!(sender.try_reserve());
    sender.commit();
    assert_eq!(receiver.admit(), Ok(()));
    receiver.release();
    assert_eq!(receiver.advertise(window), Some((3, 1)));
}
