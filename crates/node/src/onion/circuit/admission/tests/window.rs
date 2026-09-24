//! The admission window, the epoch, and at-most-once admission across rotation and late admission.

use std::collections::BTreeSet;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;

use super::expiry;
use super::latest_expiry;
use super::layer;
use super::link;
use super::live;
use super::send;
use super::sender_load;
use super::state;
use super::state_with;
use super::units;
use super::Verdict;
use super::EPOCH;
use super::ORIGIN_MS;
use super::Q;
use crate::onion::circuit::admission::OnionAdmissionLayer;
use crate::onion::circuit::admission::OnionAdmissionRejection;
use crate::onion::circuit::admission::OnionExpiry;
use crate::onion::circuit::admission::ADMISSION_WINDOW_QUANTA;
use crate::onion::circuit::admission::ADMISSION_WINDOW_QUANTA_WIDE;
use crate::onion::circuit::admission::ONION_ADMISSION_GLOBAL_UNITS;
use crate::onion::circuit::admission::ONION_ADMISSION_SENDER_UNITS;
use crate::onion::circuit::admission::ONION_ADMISSION_WINDOW_MS;
use crate::onion::OnionExitEpoch;

/// Law: an expiry exists only on the grid `Q·ℕ`, and `from_ms ∘ as_ms = Some`.
#[test]
fn test_expiry_is_constructible_only_on_the_quantum_grid() {
    let x = expiry(ORIGIN_MS / Q);
    assert_eq!(OnionExpiry::from_ms(ORIGIN_MS), Some(x));
    assert_eq!(OnionExpiry::from_ms(x.as_ms()), Some(x));
    assert_eq!(OnionExpiry::from_ms(ORIGIN_MS + 1), None);
    assert_eq!(OnionExpiry::from_ms(ORIGIN_MS + Q - 1), None);
}

/// Law: the build quantiser is `x = ⌈t / Q⌉·Q + X₀`. It is constant on each quantum `((k − 1)Q, kQ]`,
/// and its expiry is admissible at the build instant itself, which a freshly built loop needs.
#[test]
fn test_build_quantiser_maps_each_quantum_to_one_admissible_expiry() {
    let quantum = ORIGIN_MS / Q;
    for built_at_ms in [ORIGIN_MS - Q + 1, ORIGIN_MS - Q / 2, ORIGIN_MS] {
        let x = OnionExpiry::of_build(built_at_ms);
        assert_eq!(x, expiry(quantum + ADMISSION_WINDOW_QUANTA_WIDE - 1));
        assert!(x.admissible_at(built_at_ms));
    }
    assert_eq!(
        OnionExpiry::of_build(ORIGIN_MS + 1),
        expiry(quantum + ADMISSION_WINDOW_QUANTA_WIDE)
    );
}

/// Law: `(x, ν)` is admitted at most once, and a fresh in-window pair within budget is admitted.
/// The run covers every offset of `x` around the window boundaries and clock rollbacks, and it
/// checks every verdict against the reference model "window ∧ pair unseen". Forward progress
/// dominates the walk (the forward step averages `Q / 8`; a rollback happens with probability
/// `1 / 64` and averages `V / 2`), and the final assertion checks that the clock rotated through
/// at least a hundred windows of filters.
#[test]
fn test_no_replay_is_admitted_across_rotation_and_boundaries() {
    let mut rng = StdRng::seed_from_u64(0x0841_0001);
    let mut admission = state_with(&mut rng, EPOCH, 4, 0..4);
    let mut admitted = BTreeSet::new();
    let mut clock_ms = 0;
    let mut now_ms = ORIGIN_MS;
    for _ in 0..20_000 {
        now_ms = if rng.gen_ratio(1, 64) {
            now_ms.saturating_sub(rng.gen_range(0..=ONION_ADMISSION_WINDOW_MS))
        } else {
            now_ms + rng.gen_range(0..=Q / 4)
        };
        clock_ms = clock_ms.max(now_ms);
        let offset = rng.gen_range(0..=ADMISSION_WINDOW_QUANTA_WIDE + 2);
        let x = expiry(clock_ms / Q + offset - 1);
        let tag = rng.gen_range(0..32_u128);
        let verdict = send(
            &mut admission,
            now_ms,
            rng.gen_range(0..4),
            1,
            layer(x, tag),
        );
        let in_window = clock_ms < x.as_ms() && x.as_ms() <= clock_ms + ONION_ADMISSION_WINDOW_MS;
        let expected = match (in_window, admitted.contains(&(x, tag))) {
            (false, _) => Verdict::Rejected(OnionAdmissionRejection::OutsideWindow),
            (true, true) => Verdict::Rejected(OnionAdmissionRejection::Replayed),
            (true, false) => Verdict::Admitted,
        };
        assert_eq!(
            verdict, expected,
            "now={now_ms} clock={clock_ms} x={x:?} tag={tag}"
        );
        if verdict == Verdict::Admitted {
            admitted.insert((x, tag));
        }
        assert!(live(&admission).len() <= ADMISSION_WINDOW_QUANTA);
        assert!(live(&admission).iter().all(|live| live.as_ms() > clock_ms));
    }
    assert!(clock_ms >= ORIGIN_MS + 100 * ONION_ADMISSION_WINDOW_MS);
}

/// Law: at the window boundaries `arr = x − V` and `arr = x − 1` the pair is a replay, and at
/// `arr = x` the filter is dropped and the window rejects the pair.
#[test]
fn test_filter_is_dropped_exactly_at_its_expiry() {
    let mut rng = StdRng::seed_from_u64(0x0841_0002);
    let mut admission = state(&mut rng);
    let x = expiry(ORIGIN_MS / Q + 5);
    let arrival_ms = x.as_ms() - ONION_ADMISSION_WINDOW_MS;
    let outside = Verdict::Rejected(OnionAdmissionRejection::OutsideWindow);
    let replayed = Verdict::Rejected(OnionAdmissionRejection::Replayed);

    assert_eq!(
        send(&mut admission, arrival_ms - 1, 1, 1, layer(x, 1)),
        outside
    );
    assert_eq!(
        send(&mut admission, arrival_ms, 1, 1, layer(x, 1)),
        Verdict::Admitted
    );
    assert_eq!(
        send(&mut admission, arrival_ms, 2, 1, layer(x, 1)),
        replayed
    );
    assert_eq!(
        send(&mut admission, x.as_ms() - 1, 3, 1, layer(x, 1)),
        replayed
    );
    assert_eq!(live(&admission), vec![x]);
    assert_eq!(send(&mut admission, x.as_ms(), 1, 1, layer(x, 1)), outside);
    assert_eq!(live(&admission), Vec::new());
}

/// Law: a forward clock jump drops filters, and a later rollback cannot revive their pairs. The
/// monotone clock rejects them by the window.
#[test]
fn test_clock_rollback_after_a_jump_never_admits_a_replay() {
    let mut rng = StdRng::seed_from_u64(0x0841_0003);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);

    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 9)),
        Verdict::Admitted
    );
    assert_eq!(
        send(
            &mut admission,
            x.as_ms(),
            1,
            1,
            layer(latest_expiry(x.as_ms()), 10)
        ),
        Verdict::Admitted
    );
    assert!(!live(&admission).contains(&x));
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 9)),
        Verdict::Rejected(OnionAdmissionRejection::OutsideWindow)
    );
}

/// Law (D2): a restarted process rejects every layer sealed for the previous epoch. Each such
/// cell is still charged, and none enters the replay store.
#[test]
fn test_new_epoch_rejects_every_layer_of_the_old_one() {
    let mut rng = StdRng::seed_from_u64(0x0841_0004);
    let x = latest_expiry(ORIGIN_MS);
    let mut before = state(&mut rng);
    assert_eq!(
        send(&mut before, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Admitted
    );

    let restarted_epoch = OnionExitEpoch::new([8; 16]);
    let mut restarted = state_with(&mut rng, restarted_epoch, 64, 1..2);
    for tag in 0..64 {
        assert_eq!(
            send(&mut restarted, ORIGIN_MS, 1, 1, layer(x, tag)),
            Verdict::Rejected(OnionAdmissionRejection::StaleEpoch)
        );
    }
    assert_eq!(restarted.global.load(ORIGIN_MS / Q), 64);
    assert_eq!(live(&restarted), Vec::new());
    assert_eq!(
        send(&mut restarted, ORIGIN_MS, 1, 1, OnionAdmissionLayer {
            epoch: restarted_epoch,
            ..layer(x, 1)
        }),
        Verdict::Admitted
    );
}

/// Law (H1 of the round-3 review): the window is judged at the charge's arrival, not at the
/// admission's instant. A cell charged at `arr` with `x = arr + V + Q` (outside the window) is
/// still outside when its admission completes later, once `x ≤ clock + V`. A cell charged inside
/// the window whose admission completes after `x` has passed is rejected too, because its filter
/// is gone. So a late admission neither widens the window nor writes a dropped filter.
#[test]
fn test_the_window_is_judged_at_the_charge_instant() {
    let mut rng = StdRng::seed_from_u64(0x0841_0013);
    let mut admission = state(&mut rng);
    let sender = link(1);
    let outside = Err(OnionAdmissionRejection::OutsideWindow);

    let early = admission
        .charge(ORIGIN_MS, sender, units(1))
        .expect("the sender has headroom");
    let too_far = expiry(ORIGIN_MS / Q + ADMISSION_WINDOW_QUANTA_WIDE + 1);
    assert!(too_far.admissible_at(ORIGIN_MS + Q));
    assert_eq!(
        admission.admit(ORIGIN_MS + Q, early, layer(too_far, 1)),
        outside
    );
    assert!(live(&admission).is_empty());

    let x = latest_expiry(ORIGIN_MS);
    let in_time = admission
        .charge(ORIGIN_MS, sender, units(1))
        .expect("the sender has headroom");
    assert_eq!(admission.admit(x.as_ms(), in_time, layer(x, 2)), outside);
    assert!(live(&admission).is_empty());
    assert_eq!(sender_load(&admission, 1, ORIGIN_MS), Some(2));
}

/// Law: a token binds the epoch of the state that charged it, so a token from another process
/// (another epoch) admits nothing here, even for a layer sealed for this epoch.
#[test]
fn test_a_token_from_another_epoch_admits_nothing() {
    let mut rng = StdRng::seed_from_u64(0x0841_0014);
    let mut here = state(&mut rng);
    let mut elsewhere = state_with(&mut rng, OnionExitEpoch::new([9; 16]), 4, 1..2);
    let x = latest_expiry(ORIGIN_MS);
    let foreign = elsewhere
        .charge(ORIGIN_MS, link(1), units(1))
        .expect("the other state has headroom");
    assert_eq!(
        here.admit(ORIGIN_MS, foreign, layer(x, 1)),
        Err(OnionAdmissionRejection::StaleEpoch)
    );
    assert!(live(&here).is_empty());
}

/// Property, seeded (H1 of the round-3 review): with tokens held across quanta and admitted late,
/// no filter ever holds more than `G / u` tags of `u`-unit cells, and the bound is tight.
///
/// Each of four rounds targets one expiry `x = kQ` with 64 DIDs and `u = 1024`, so that
/// `64·B / u = G / u` cells saturate a window:
/// * Phase A: in quantum `k − 6`, every DID spends its whole budget on cells that name `x`. At that
///   arrival `x` lies outside the window. The tokens are held and admitted at random instants in
///   `[(k − 5)Q, kQ)`, when `x ≤ clock + V`.
/// * Phase B: in quantum `k − 1`, after phase A has left the ledger window, every DID spends its
///   budget again on cells that name `x`, and they are admitted at once.
///
/// Judging the window at the charge instant rejects all of phase A and admits all of phase B, so
/// `R_i[x]` holds exactly `G / u` tags. Judging it at the admission instant would admit both
/// phases, `2·G / u` tags.
#[test]
fn test_late_admissions_never_exceed_the_global_bound_per_filter() {
    const UNITS: u32 = 1024;
    let per_sender = ONION_ADMISSION_SENDER_UNITS / UNITS;
    let bound = ONION_ADMISSION_GLOBAL_UNITS / UNITS;
    let mut rng = StdRng::seed_from_u64(0x0841_0016);
    let mut admission = state(&mut rng);
    let mut tag = 0_u128;
    for round in 0..4_u128 {
        let k = ORIGIN_MS / Q + 20 + 10 * round;
        let x = expiry(k);
        let mut events = Vec::new();
        let cells = (0..64_u32).flat_map(|sender| (0..per_sender).map(move |_| sender));
        for (sender, offset) in cells.clone().zip(0_u128..) {
            let arrival_ms = (k - 6) * Q + offset;
            let token = admission
                .charge(arrival_ms, link(sender), units(UNITS))
                .expect("phase A fits every budget");
            tag += 1;
            let release_ms = (k - 5) * Q + rng.gen_range(0..5 * Q);
            events.push((release_ms, token, layer(x, tag)));
        }
        events.sort_by_key(|(release_ms, _, _)| *release_ms);
        let mut admitted = 0_u32;
        let mut late = events.into_iter().peekable();
        while let Some((release_ms, token, layer)) =
            late.next_if(|(release_ms, _, _)| *release_ms < (k - 1) * Q)
        {
            assert_eq!(
                admission.admit(release_ms, token, layer),
                Err(OnionAdmissionRejection::OutsideWindow)
            );
        }
        for (sender, offset) in cells.zip(0_u128..) {
            let arrival_ms = (k - 1) * Q + offset;
            while let Some((release_ms, token, layer)) =
                late.next_if(|(release_ms, _, _)| *release_ms <= arrival_ms)
            {
                assert_eq!(
                    admission.admit(release_ms, token, layer),
                    Err(OnionAdmissionRejection::OutsideWindow)
                );
            }
            tag += 1;
            if send(&mut admission, arrival_ms, sender, UNITS, layer(x, tag)) == Verdict::Admitted {
                admitted += 1;
            }
        }
        for (release_ms, token, layer) in late {
            assert_eq!(
                admission.admit(release_ms, token, layer),
                Err(OnionAdmissionRejection::OutsideWindow)
            );
        }
        assert_eq!(admitted, bound);
    }
}

/// Law: the shell's renewal trigger is exactly `now < clock − X₀`.
#[test]
fn test_rollback_is_detected_beyond_the_build_offset() {
    let mut rng = StdRng::seed_from_u64(0x0841_001a);
    let mut admission = state(&mut rng);
    assert_eq!(
        send(
            &mut admission,
            ORIGIN_MS,
            1,
            1,
            layer(latest_expiry(ORIGIN_MS), 1)
        ),
        Verdict::Admitted
    );
    let offset = (ADMISSION_WINDOW_QUANTA_WIDE - 1) * Q;
    assert!(!admission.is_rolled_back_at(ORIGIN_MS));
    assert!(!admission.is_rolled_back_at(ORIGIN_MS - offset));
    assert!(admission.is_rolled_back_at(ORIGIN_MS - offset - 1));
}
