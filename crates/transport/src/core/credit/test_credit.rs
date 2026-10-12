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
