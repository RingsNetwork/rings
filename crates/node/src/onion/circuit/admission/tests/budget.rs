//! The unit budgets: charge once on receipt, `B` per DID and `G` per hop over the sliding window.

use rand::rngs::StdRng;
use rand::SeedableRng;

use super::expiry;
use super::latest_expiry;
use super::layer;
use super::link;
use super::send;
use super::sender_load;
use super::state;
use super::units;
use super::Verdict;
use super::ORIGIN_MS;
use super::Q;
use crate::onion::circuit::admission::OnionAdmissionRejection;
use crate::onion::circuit::admission::OnionChargeRejection;
use crate::onion::circuit::admission::ADMISSION_WINDOW_QUANTA_WIDE;
use crate::onion::circuit::admission::ONION_ADMISSION_GLOBAL_UNITS;
use crate::onion::circuit::admission::ONION_ADMISSION_SENDER_UNITS;
use crate::onion::circuit::admission::ONION_ADMISSION_WINDOW_MS;

/// Law (charging): a cell is charged exactly once, before its key is computed. A `γ`-failing cell
/// drops its token and stays charged without touching the replay store. Replayed and expired
/// cells stay charged. A refused charge charges nothing. Pipelined cells cannot overdraw the
/// budget, because each ECDH needs a token that has already been paid for.
#[test]
fn test_every_computed_cell_is_charged_exactly_once() {
    let mut rng = StdRng::seed_from_u64(0x0841_000f);
    let mut admission = state(&mut rng);
    let quantum = ORIGIN_MS / Q;
    let x = latest_expiry(ORIGIN_MS);
    let sender = link(1);

    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 2, layer(x, 1)),
        Verdict::Admitted
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 3, layer(x, 1)),
        Verdict::Rejected(OnionAdmissionRejection::Replayed)
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 5, layer(expiry(quantum), 2)),
        Verdict::Rejected(OnionAdmissionRejection::OutsideWindow)
    );
    let invalid = admission.charge(ORIGIN_MS, sender, units(7));
    assert!(invalid.is_ok());
    drop(invalid);
    assert_eq!(sender_load(&admission, 1, ORIGIN_MS), Some(2 + 3 + 5 + 7));
    assert_eq!(admission.global.load(quantum), 2 + 3 + 5 + 7);
    assert_eq!(
        admission
            .replay
            .filters()
            .get(&x)
            .map(|filter| filter.blocks().count()),
        Some(1)
    );

    let spent = ONION_ADMISSION_SENDER_UNITS - 17 - 1;
    assert!(admission.charge(ORIGIN_MS, sender, units(spent)).is_ok());
    let pipelined = (0..8)
        .map(|_| admission.charge(ORIGIN_MS, sender, units(1)))
        .collect::<Vec<_>>();
    assert!(pipelined.first().is_some_and(Result::is_ok));
    assert!(pipelined
        .iter()
        .skip(1)
        .all(|charge| matches!(charge, Err(OnionChargeRejection::SenderBudget))));
    assert_eq!(
        sender_load(&admission, 1, ORIGIN_MS),
        Some(ONION_ADMISSION_SENDER_UNITS)
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1_000, 1, layer(x, 4)),
        Verdict::Unpaid(OnionChargeRejection::LinkNotLive)
    );
    assert_eq!(admission.global.load(quantum), ONION_ADMISSION_SENDER_UNITS);
}

/// Law: one sender is charged at most `B` units over five aligned quanta. The budget returns
/// exactly when the charging quantum leaves the window, and other senders are unaffected.
#[test]
fn test_sender_budget_is_bounded_over_the_sliding_window() {
    let mut rng = StdRng::seed_from_u64(0x0841_0005);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);
    let over = Verdict::Unpaid(OnionChargeRejection::SenderBudget);
    for tag in 0..u128::from(ONION_ADMISSION_SENDER_UNITS / 768) {
        assert_eq!(
            send(&mut admission, ORIGIN_MS, 1, 768, layer(x, tag)),
            Verdict::Admitted
        );
    }
    assert_eq!(send(&mut admission, ORIGIN_MS, 1, 768, layer(x, 100)), over);
    assert_eq!(
        send(
            &mut admission,
            ORIGIN_MS,
            1,
            ONION_ADMISSION_SENDER_UNITS % 768,
            layer(x, 101)
        ),
        Verdict::Admitted
    );
    assert_eq!(send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 102)), over);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 2, 1, layer(x, 103)),
        Verdict::Admitted
    );

    let last_in_window = ORIGIN_MS + ADMISSION_WINDOW_QUANTA_WIDE * Q - 1;
    assert_eq!(
        send(
            &mut admission,
            last_in_window,
            1,
            1,
            layer(latest_expiry(last_in_window), 104)
        ),
        over
    );
    let refilled = last_in_window + 1;
    assert_eq!(
        send(
            &mut admission,
            refilled,
            1,
            1,
            layer(latest_expiry(refilled), 105)
        ),
        Verdict::Admitted
    );
}

/// Law: the hop admits at most `G = 64·B` units per window, however many DIDs send. A refused
/// charge leaves every ledger unchanged.
#[test]
fn test_global_budget_bounds_every_sender_together() {
    let mut rng = StdRng::seed_from_u64(0x0841_0006);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);
    for sender in 0..64_u32 {
        assert_eq!(
            send(
                &mut admission,
                ORIGIN_MS,
                sender,
                ONION_ADMISSION_SENDER_UNITS,
                layer(x, u128::from(sender))
            ),
            Verdict::Admitted
        );
    }
    assert_eq!(
        admission.global.load(ORIGIN_MS / Q),
        ONION_ADMISSION_GLOBAL_UNITS
    );
    let before = admission.senders.clone();
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 64, 1, layer(x, 64)),
        Verdict::Unpaid(OnionChargeRejection::GlobalBudget)
    );
    assert_eq!(admission.senders, before);
    let next_window = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    assert_eq!(
        send(
            &mut admission,
            next_window,
            64,
            1,
            layer(latest_expiry(next_window), 64)
        ),
        Verdict::Admitted
    );
}
