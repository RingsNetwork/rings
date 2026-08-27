use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;

use super::IrrevocableSendGuard;
use super::SendPermit;
use super::TransportMessage;

#[derive(Serialize)]
enum LegacyTransportMessage {
    Custom(Vec<u8>),
}

#[test]
fn test_send_permit_observes_revocation_at_evaluation_time() {
    let admitted = Arc::new(AtomicBool::new(true));
    admitted.store(false, Ordering::SeqCst);
    let permit = SendPermit::new({
        let admitted = Arc::clone(&admitted);
        move || admitted.load(Ordering::SeqCst)
    });

    assert!(!permit.allows());
}

#[test]
fn test_send_acceptance_observes_confirmed_queue_admission() {
    let permit = SendPermit::always();
    let acceptance = permit.acceptance();

    assert!(!acceptance.is_accepted());
    assert!(!acceptance.is_irrevocable());
    assert!(!acceptance.failed_after_irrevocable());
    assert!(permit.allows());
    let permit = permit
        .try_mark_irrevocable()
        .expect("live permit must become irrevocable");
    assert!(acceptance.is_irrevocable());
    assert!(!acceptance.is_accepted());
    assert!(acceptance.failed_after_irrevocable());
    permit.mark_accepted();
    assert!(acceptance.is_accepted());
    assert!(acceptance.is_irrevocable());
    assert!(!acceptance.failed_after_irrevocable());
}

#[test]
fn test_irrevocable_transition_is_one_shot_and_requires_a_live_predicate() {
    let denied = SendPermit::new(|| false);
    let denied_acceptance = denied.acceptance();
    assert!(denied.try_mark_irrevocable().is_none());
    assert!(!denied_acceptance.is_irrevocable());

    let admitted = SendPermit::always();
    let admitted_acceptance = admitted.acceptance();
    let _irrevocable = admitted
        .try_mark_irrevocable()
        .expect("live permit must become irrevocable");
    assert!(admitted_acceptance.is_irrevocable());
    assert!(!admitted_acceptance.is_accepted());
}

#[test]
fn test_cancellation_and_irrevocable_admission_are_mutually_exclusive() {
    let cancelled = SendPermit::always();
    let cancelled_acceptance = cancelled.acceptance();
    assert!(cancelled_acceptance.try_cancel());
    assert!(cancelled.try_mark_irrevocable().is_none());
    assert!(!cancelled_acceptance.is_irrevocable());

    let admitted = SendPermit::always();
    let admitted_acceptance = admitted.acceptance();
    let _permit = admitted
        .try_mark_irrevocable()
        .expect("irrevocable admission must win before cancellation");
    assert!(!admitted_acceptance.try_cancel());
    assert!(admitted_acceptance.is_irrevocable());
}

#[test]
fn test_irrevocable_guard_is_claimed_only_at_the_final_boundary() {
    let guard_open = Arc::new(AtomicBool::new(false));
    let permit = SendPermit::always().with_irrevocable_guard({
        let guard_open = Arc::clone(&guard_open);
        move |claim| {
            if guard_open.load(Ordering::SeqCst) {
                let _claimed = claim.try_claim();
            }
        }
    });
    let denied_acceptance = permit.acceptance();

    assert!(permit.allows());
    assert!(!denied_acceptance.is_irrevocable());
    assert!(permit.try_mark_irrevocable().is_none());
    let permit = SendPermit::always().with_irrevocable_guard({
        let guard_open = Arc::clone(&guard_open);
        move |claim| {
            if guard_open.load(Ordering::SeqCst) {
                let _claimed = claim.try_claim();
            }
        }
    });
    let admitted_acceptance = permit.acceptance();
    guard_open.store(true, Ordering::SeqCst);
    assert!(permit.try_mark_irrevocable().is_some());
    assert!(admitted_acceptance.is_irrevocable());
}

#[test]
fn test_irrevocable_guard_cannot_forge_a_proof_without_claiming_this_permit() {
    let permit = SendPermit::always().with_irrevocable_guard(|_claim| {});
    let acceptance = permit.acceptance();

    assert!(permit.try_mark_irrevocable().is_none());
    assert!(acceptance.try_cancel());
    assert!(!acceptance.is_irrevocable());
}

#[test]
fn test_claiming_in_a_guard_always_returns_the_matching_proof() {
    let permit = SendPermit::always().with_irrevocable_guard(|claim| {
        assert!(claim.try_claim());
    });
    let acceptance = permit.acceptance();

    let proof = permit
        .try_mark_irrevocable()
        .expect("a successful claim must return its proof");
    assert!(acceptance.is_irrevocable());
    proof.mark_accepted();
    assert!(acceptance.is_accepted());
}

#[test]
fn test_failed_irrevocable_send_retires_while_acceptance_disarms_retirement() {
    let failed = SendPermit::always();
    let failed_acceptance = failed.acceptance();
    let failed_retired = Arc::new(AtomicBool::new(false));
    {
        let retired = Arc::clone(&failed_retired);
        let mut guard = IrrevocableSendGuard::new(failed_acceptance.clone(), move || {
            retired.store(true, Ordering::Release)
        });
        guard.bind(
            failed
                .try_mark_irrevocable()
                .expect("live permit must become irrevocable"),
        );
    }
    assert!(failed_acceptance.is_irrevocable());
    assert!(!failed_acceptance.is_accepted());
    assert!(failed_retired.load(Ordering::Acquire));

    let accepted = SendPermit::always();
    let accepted_observer = accepted.acceptance();
    let accepted_retired = Arc::new(AtomicBool::new(false));
    let mut guard = IrrevocableSendGuard::new(accepted_observer.clone(), {
        let retired = Arc::clone(&accepted_retired);
        move || retired.store(true, Ordering::Release)
    });
    guard.bind(
        accepted
            .try_mark_irrevocable()
            .expect("live permit must become irrevocable"),
    );
    guard.mark_accepted();
    assert!(accepted_observer.is_accepted());
    assert!(!accepted_retired.load(Ordering::Acquire));
}

#[test]
fn test_claim_then_panic_retires_an_already_armed_send() {
    let permit = SendPermit::always().with_irrevocable_guard(|claim| {
        assert!(claim.try_claim());
        panic!("injected final-boundary guard panic");
    });
    let acceptance = permit.acceptance();
    let retired = Arc::new(AtomicBool::new(false));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
        let retired = Arc::clone(&retired);
        let guarded_acceptance = acceptance.clone();
        move || {
            let _retirement = IrrevocableSendGuard::new(guarded_acceptance, move || {
                retired.store(true, Ordering::Release);
            });
            let _proof = permit.try_mark_irrevocable();
        }
    }));

    assert!(outcome.is_err());
    assert!(acceptance.is_irrevocable());
    assert!(!acceptance.is_accepted());
    assert!(retired.load(Ordering::Acquire));
}

#[test]
fn test_bytes_transport_message_preserves_legacy_wire_encoding() {
    let body = vec![1, 2, 3, 4];
    let legacy = rings_codec::serialize(&LegacyTransportMessage::Custom(body.clone()))
        .expect("legacy message must serialize");
    let current = rings_codec::serialize(&TransportMessage::Custom(Bytes::from(body)))
        .expect("current message must serialize");

    assert_eq!(current, legacy);
}

#[test]
fn test_custom_message_accepts_the_documented_vec_migration() {
    let body = vec![1, 2, 3, 4];
    let message: TransportMessage = TransportMessage::Custom(body.into());

    assert!(matches!(message, TransportMessage::Custom(bytes) if bytes.len() == 4));
}
