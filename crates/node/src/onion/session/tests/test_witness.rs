//! Laws of the stall predicate and of the session witness (#950): what the client endpoint knows
//! of its session at an instant.

use bytes::Bytes;

use super::super::client::OnionClientSession;
use super::super::frame::OnionFrame;
use super::super::frame::OnionSequence;
use super::super::order::OnionReorder;
use super::super::witness::OnionSessionCondition;
use super::super::witness::OnionSessionWitness;
use super::NOW_MS;
use super::TARGET;
use crate::onion::circuit::ONION_ADMISSION_WINDOW_MS;

/// The reply `data(n, 0, w)`.
fn reply(sequence: u32, payload: &'static [u8]) -> OnionFrame {
    OnionFrame::Data {
        sequence: OnionSequence::new(sequence),
        target: None,
        payload: Bytes::from_static(payload),
    }
}

/// Law Stall on the reorder window: `stalled ⇔ P ≠ ∅ ∧ ¬failed`, over a hole that fills and a
/// hole that fails.
#[test]
fn test_reorder_is_stalled_exactly_while_a_frame_is_held_behind_a_hole() {
    let mut filled = OnionReorder::default();
    assert!(!filled.is_stalled());
    let held = filled
        .accept(NOW_MS, OnionSequence::new(1), 1)
        .expect("inside the window");
    assert!(held.is_empty());
    assert!(filled.is_stalled());
    let released = filled
        .accept(NOW_MS, OnionSequence::FIRST, 0)
        .expect("the hole fills");
    assert_eq!(released, vec![0, 1]);
    assert!(!filled.is_stalled());

    let mut failed = OnionReorder::default();
    let _ = failed
        .accept(NOW_MS, OnionSequence::new(1), ())
        .expect("inside the window");
    assert!(failed.is_stalled());
    assert!(failed.expire(NOW_MS + ONION_ADMISSION_WINDOW_MS).is_err());
    assert!(!failed.is_stalled());
}

/// The client machine is stalled while a reply is held behind a missing one: data past the
/// open ack, before the data it follows.
#[test]
fn test_client_session_is_stalled_while_a_reply_is_missing() {
    let mut session = OnionClientSession::new(Bytes::from_static(TARGET), NOW_MS);
    let _ = session.reply(NOW_MS, reply(0, b"")).expect("the open ack");
    assert!(!session.stalled());
    let held = session.reply(NOW_MS, reply(2, b"late")).expect("held");
    assert!(held.is_empty());
    assert!(session.stalled());
    let _ = session
        .reply(NOW_MS, reply(1, b"early"))
        .expect("the hole fills");
    assert!(!session.stalled());
}

/// Law Absorbing: a failed witness reads `Failed` whatever stall is recorded after it; before
/// it, the witness follows the recorded stall both ways.
#[test]
fn test_session_witness_failure_is_absorbing() {
    let witness = OnionSessionWitness::default();
    assert_eq!(witness.condition(), OnionSessionCondition::Live);
    witness.record_stalled(true);
    assert_eq!(witness.condition(), OnionSessionCondition::Stalled);
    witness.record_stalled(false);
    assert_eq!(witness.condition(), OnionSessionCondition::Live);
    witness.record_failed();
    for stalled in [true, false] {
        witness.record_stalled(stalled);
        assert_eq!(witness.condition(), OnionSessionCondition::Failed);
    }
}
