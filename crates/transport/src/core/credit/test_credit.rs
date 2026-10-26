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
    assert_eq!(receiver.advertise(window), Some(6));

    sender.grant(6);
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
    assert_eq!(receiver.advertise(window), Some(6));
    assert_eq!(receiver.unused(), 2);
}
