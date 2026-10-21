//! Law tests of the forward high-water mark (#981; paper, D2′ Order): the `credit(m, υ…)`
//! encoding, the reorder window's marks (`session::order`, Laws Gap, Mark, Stall), and the
//! exit's Law Mark.
//!
//! Every instant is an argument of a pure machine, so the clock is the test's own.

use super::acked;
use super::credit;
use super::kinds;
use super::round_trip;
use super::surbs;
use super::CLASS;
use super::NOW_MS;
use crate::onion::circuit::ONION_ADMISSION_WINDOW_MS;
use crate::onion::session::exit::ONION_SESSION_ECHO_HORIZON_MS;
use crate::onion::session::frame::OnionFrame;
use crate::onion::session::frame::OnionSequence;
use crate::onion::session::order::OnionReorder;
use crate::onion::session::order::OnionSequenceGap;
use crate::onion::sphinx::cell::ONION_SURB_BYTES;

/// `V`, the forward validity.
const V: u128 = ONION_ADMISSION_WINDOW_MS;

// ---- frame -------------------------------------------------------------------------------------

/// Round trip and Width: `enc(credit(m, υ)) = 02 ‖ m:u32 ‖ υ`, decoded back to the same `m`, and
/// the capacity is `k = ⌊(C₀ − 6) / |υ|⌋`, the most blocks whose frame fits `C₀ − 1` bytes.
#[test]
fn test_credit_frame_carries_its_mark() {
    let bytes = round_trip(&credit(0x0102_0304, surbs(110, 1, 0)));
    assert_eq!(bytes.len(), 5 + ONION_SURB_BYTES);
    assert_eq!(bytes[..5], [0x02, 0x01, 0x02, 0x03, 0x04], "02 ‖ m:u32");
    assert!(matches!(
        OnionFrame::decode(CLASS, &bytes),
        Ok(OnionFrame::Credit { mark, surbs }) if mark.value() == 0x0102_0304 && surbs.len() == 1
    ));

    let k = OnionFrame::credit_capacity(CLASS);
    assert_eq!(k, (CLASS.value_capacity() - 5) / ONION_SURB_BYTES);
    assert!(5 + k * ONION_SURB_BYTES <= CLASS.value_capacity());
    assert!(5 + (k + 1) * ONION_SURB_BYTES > CLASS.value_capacity());
}

// ---- order -------------------------------------------------------------------------------------

/// Law Gap: a mark above `next` makes it known missing from the mark's arrival, exactly as a
/// later frame would, so the direction fails `V` after that arrival.
#[test]
fn test_reorder_mark_exposes_a_missing_last_frame() {
    let mut reorder = OnionReorder::default();
    assert_eq!(reorder.accept(NOW_MS, OnionSequence::FIRST, 0), Ok(vec![0]));
    assert!(!reorder.is_stalled());

    let marked = NOW_MS + 10;
    assert_eq!(reorder.mark(marked, OnionSequence::new(2)), Ok(()));
    assert!(reorder.is_stalled(), "frame 1 is known missing");
    assert_eq!(reorder.expire(marked + V - 1), Ok(()));
    assert_eq!(
        reorder.expire(marked + V),
        Err(OnionSequenceGap { missing: 1 })
    );
    assert!(!reorder.is_stalled(), "a failed direction is not stalled");
}

/// Law Mark: a mark at or below `next` changes nothing, and a mark never releases a frame nor
/// occupies the window, however far beyond it.
#[test]
fn test_reorder_mark_at_or_below_next_is_inert() {
    let mut reorder = OnionReorder::default();
    assert_eq!(reorder.mark(NOW_MS, OnionSequence::FIRST), Ok(()));
    assert!(reorder.accept(NOW_MS, OnionSequence::FIRST, 0).is_ok());
    for mark in [0, 1] {
        assert_eq!(reorder.mark(NOW_MS, OnionSequence::new(mark)), Ok(()));
    }
    assert!(!reorder.is_stalled());
    assert_eq!(reorder.expire(NOW_MS + 10 * V), Ok(()));

    let mut far = OnionReorder::default();
    assert_eq!(far.mark(NOW_MS, OnionSequence::new(u32::MAX)), Ok(()));
    assert_eq!(
        far.accept(NOW_MS, OnionSequence::FIRST, 0),
        Ok(vec![0]),
        "the mark invents no frame and leaves the window whole"
    );
    assert!(far.is_stalled());
}

/// Law Gap, soundness: the missing instant is the arrival of the earliest mark above the
/// current `next`, so a mark the released frames reached no longer counts, and a later mark
/// starts its own `V`; a lower mark after a higher one moves nothing.
#[test]
fn test_reorder_mark_counts_from_the_first_mark_above_next() {
    let mut reorder = OnionReorder::default();
    assert_eq!(reorder.mark(NOW_MS, OnionSequence::new(2)), Ok(()));
    for n in [1, 0] {
        assert!(reorder.accept(NOW_MS + 1, OnionSequence::new(n), n).is_ok());
    }
    assert!(!reorder.is_stalled(), "the frames reached the mark");

    let later = NOW_MS + V / 2;
    assert_eq!(reorder.mark(later, OnionSequence::new(4)), Ok(()));
    assert_eq!(reorder.mark(later + 1, OnionSequence::new(3)), Ok(()));
    assert_eq!(
        reorder.expire(NOW_MS + V),
        Ok(()),
        "the first mark no longer counts"
    );
    assert_eq!(reorder.expire(later + V - 1), Ok(()));
    assert_eq!(
        reorder.expire(later + V),
        Err(OnionSequenceGap { missing: 2 })
    );
}

// ---- exit --------------------------------------------------------------------------------------

/// Law Mark at `h`: the client's last `data(1)` is lost and only credit loops follow it, each
/// with `m = 2`. `h` echoes as usual, then fails `ς` exactly `V` after the first of them and
/// spends a block they left on `abort(n)`.
#[test]
fn test_exit_aborts_a_lost_last_frame_exposed_by_a_mark() {
    let mut blocks = surbs(120, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    let first = NOW_MS + 1;
    let mut early = surbs(121, 2, 0).into_iter();
    let refill = credit(2, vec![early.next().expect("a block")]);
    assert!(kinds(&session.forward(first, refill, early.next().expect("a block"))).is_empty());

    // The keep-alive half a window later: it is echoed, and its blocks outlive `first + V`.
    let keep_alive = first + ONION_SESSION_ECHO_HORIZON_MS;
    let mut late = surbs(122, 2, 5).into_iter();
    let refill = credit(2, vec![late.next().expect("a block")]);
    assert_eq!(
        kinds(&session.forward(keep_alive, refill, late.next().expect("a block"))),
        ["data 1 0"]
    );

    assert!(kinds(&session.tick(first + V - 1)).is_empty());
    assert_eq!(kinds(&session.tick(first + V)), ["abort 2", "close"]);
    assert!(session.is_closed());
}

/// Law Mark at `h`: credit loops whose `m` is at or below the next expected forward sequence
/// change nothing, so a session they keep alive is neither stalled nor failed.
#[test]
fn test_exit_ignores_a_mark_at_or_below_the_next_sequence() {
    let mut blocks = surbs(130, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    let mut fresh = surbs(131, 4, 2).into_iter();
    let mut next = || fresh.next().expect("a block");
    for (at, mark) in [(NOW_MS + 1, 1), (NOW_MS + V / 2, 0)] {
        session.forward(at, credit(mark, vec![next()]), next());
    }
    assert!(kinds(&session.tick(NOW_MS + 1 + V)).is_empty());
    assert!(!session.is_closed());
}
