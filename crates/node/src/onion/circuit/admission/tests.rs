//! Laws of the L9 admission step, checked against injected time and a seeded RNG.

use std::collections::HashSet;
use std::num::NonZeroU32;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use rings_core::dht::Did;

use super::bloom::ReplayStore;
use super::bloom::REPLAY_BLOCK_HASHES;
use super::bloom::REPLAY_BLOCK_TAGS;
use super::bloom::REPLAY_SLICE_BITS;
use super::bloom::REPLAY_SLICE_WORDS;
use super::OnionAdmissionRejection;
use super::OnionAdmissionRequest;
use super::OnionAdmissionState;
use super::OnionAdmissionUnits;
use super::OnionExpiry;
use super::OnionReplayFilterKey;
use super::ADMISSION_WINDOW_QUANTA;
use super::ADMISSION_WINDOW_QUANTA_WIDE;
use super::ONION_ADMISSION_GLOBAL_UNITS;
use super::ONION_ADMISSION_SENDERS;
use super::ONION_ADMISSION_SENDER_UNITS;
use super::ONION_ADMISSION_WINDOW_MS;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::circuit::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use crate::onion::OnionExitEpoch;

/// `Q` in milliseconds.
const Q: u128 = ONION_FORWARD_EXPIRY_QUANTUM_MS;

/// An aligned origin far from zero, so no window arithmetic saturates.
const ORIGIN_MS: u128 = 1_000 * Q;

/// The epoch of the process under test.
const EPOCH: OnionExitEpoch = OnionExitEpoch::new([7; 16]);

/// A fresh state for [`EPOCH`] whose probe key is drawn from `rng`.
fn state(rng: &mut StdRng) -> OnionAdmissionState {
    OnionAdmissionState::new(EPOCH, OnionReplayFilterKey::new(rng.gen()))
}

/// The on-grid expiry `k · Q`.
fn expiry(quantum: u128) -> OnionExpiry {
    OnionExpiry(quantum * Q)
}

/// The latest admissible expiry at `now_ms`: the greatest grid point in `(now, now + V]`.
fn latest_expiry(now_ms: u128) -> OnionExpiry {
    expiry(now_ms / Q + ADMISSION_WINDOW_QUANTA_WIDE)
}

/// `n ≥ 1` units; zero saturates to one unit.
fn units(n: u32) -> OnionAdmissionUnits {
    OnionAdmissionUnits::new(NonZeroU32::MIN.saturating_add(n.saturating_sub(1)))
}

/// A request for the current epoch.
fn request(
    from: u32,
    expiry: OnionExpiry,
    tag: u128,
    units: OnionAdmissionUnits,
) -> OnionAdmissionRequest {
    OnionAdmissionRequest {
        from: Did::from(from),
        epoch: EPOCH,
        expiry,
        tag: OnionForwardNonce::new(tag.to_le_bytes()),
        units,
    }
}

/// The live filter keys in expiry order.
fn live(state: &OnionAdmissionState) -> Vec<OnionExpiry> {
    state.replay.filters().keys().copied().collect()
}

#[test]
fn test_expiry_is_constructible_only_on_the_quantum_grid() {
    assert_eq!(
        OnionExpiry::from_ms(ORIGIN_MS),
        Some(OnionExpiry(ORIGIN_MS))
    );
    assert_eq!(OnionExpiry::from_ms(ORIGIN_MS + 1), None);
    assert_eq!(OnionExpiry::from_ms(ORIGIN_MS + Q - 1), None);
}

/// Law: `(x, ν)` is admitted at most once, and a fresh in-window pair within budget is admitted.
/// The run covers rotation through many filters, every offset of `x` around the window
/// boundaries, and clock rollbacks. It checks the verdicts against the reference model
/// "window ∧ pair unseen".
#[test]
fn test_no_replay_is_admitted_across_rotation_and_boundaries() {
    let mut rng = StdRng::seed_from_u64(0x0841_0001);
    let mut admission = state(&mut rng);
    let mut admitted = HashSet::new();
    let mut clock_ms = 0;
    let mut now_ms = ORIGIN_MS;
    for _ in 0..20_000 {
        now_ms = if rng.gen_ratio(1, 16) {
            now_ms.saturating_sub(rng.gen_range(0..=2 * ONION_ADMISSION_WINDOW_MS))
        } else {
            now_ms + rng.gen_range(0..=Q / 4)
        };
        clock_ms = clock_ms.max(now_ms);
        let offset = rng.gen_range(0..=ADMISSION_WINDOW_QUANTA_WIDE + 2);
        let x = expiry(clock_ms / Q + offset - 1);
        let tag = rng.gen_range(0..32_u128);
        let verdict = admission.admit(now_ms, request(rng.gen_range(0..4), x, tag, units(1)));
        let in_window = clock_ms < x.as_ms() && x.as_ms() <= clock_ms + ONION_ADMISSION_WINDOW_MS;
        let expected = match (in_window, admitted.contains(&(x, tag))) {
            (false, _) => Err(OnionAdmissionRejection::OutsideWindow),
            (true, true) => Err(OnionAdmissionRejection::Replayed),
            (true, false) => Ok(()),
        };
        assert_eq!(
            verdict, expected,
            "now={now_ms} clock={clock_ms} x={x:?} tag={tag}"
        );
        if verdict.is_ok() {
            admitted.insert((x, tag));
        }
        assert!(live(&admission).len() <= ADMISSION_WINDOW_QUANTA);
        assert!(live(&admission).iter().all(|live| live.as_ms() > clock_ms));
    }
}

/// Law: at the window boundaries `arr = x − V` and `arr = x − 1` the pair is a replay, and at
/// `arr = x` the filter is dropped and the window rejects the pair.
#[test]
fn test_filter_is_dropped_exactly_at_its_expiry() {
    let mut rng = StdRng::seed_from_u64(0x0841_0002);
    let mut admission = state(&mut rng);
    let x = expiry(ORIGIN_MS / Q + 5);
    let arrival_ms = x.as_ms() - ONION_ADMISSION_WINDOW_MS;

    assert_eq!(
        admission.admit(arrival_ms - 1, request(1, x, 1, units(1))),
        Err(OnionAdmissionRejection::OutsideWindow)
    );
    assert_eq!(
        admission.admit(arrival_ms, request(1, x, 1, units(1))),
        Ok(())
    );
    assert_eq!(
        admission.admit(arrival_ms, request(2, x, 1, units(1))),
        Err(OnionAdmissionRejection::Replayed)
    );
    assert_eq!(
        admission.admit(x.as_ms() - 1, request(3, x, 1, units(1))),
        Err(OnionAdmissionRejection::Replayed)
    );
    assert_eq!(live(&admission), vec![x]);
    assert_eq!(
        admission.admit(x.as_ms(), request(1, x, 1, units(1))),
        Err(OnionAdmissionRejection::OutsideWindow)
    );
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
        admission.admit(ORIGIN_MS, request(1, x, 9, units(1))),
        Ok(())
    );
    assert_eq!(
        admission.admit(
            x.as_ms(),
            request(1, latest_expiry(x.as_ms()), 10, units(1))
        ),
        Ok(())
    );
    assert!(!live(&admission).contains(&x));
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, x, 9, units(1))),
        Err(OnionAdmissionRejection::OutsideWindow)
    );
}

/// Law (D2): a restarted process rejects every layer sealed for the previous epoch, and a
/// rejection commits nothing.
#[test]
fn test_new_epoch_rejects_every_layer_of_the_old_one() {
    let mut rng = StdRng::seed_from_u64(0x0841_0004);
    let x = latest_expiry(ORIGIN_MS);
    let mut before = state(&mut rng);
    assert_eq!(before.admit(ORIGIN_MS, request(1, x, 1, units(1))), Ok(()));

    let mut restarted = OnionAdmissionState::new(
        OnionExitEpoch::new([8; 16]),
        OnionReplayFilterKey::new(rng.gen()),
    );
    for tag in 0..64 {
        assert_eq!(
            restarted.admit(ORIGIN_MS, request(1, x, tag, units(1))),
            Err(OnionAdmissionRejection::StaleEpoch)
        );
    }
    assert!(restarted.senders.is_empty());
    assert_eq!(restarted.global.load(ORIGIN_MS / Q), 0);
    assert_eq!(live(&restarted), Vec::new());
    assert_eq!(
        restarted.admit(ORIGIN_MS, OnionAdmissionRequest {
            epoch: OnionExitEpoch::new([8; 16]),
            ..request(1, x, 1, units(1))
        }),
        Ok(())
    );
}

/// Law: one sender is charged at most `B` units over five aligned quanta. The budget returns
/// exactly when the charging quantum leaves the window, and other senders are unaffected.
#[test]
fn test_sender_budget_is_bounded_over_the_sliding_window() {
    let mut rng = StdRng::seed_from_u64(0x0841_0005);
    let mut admission = state(&mut rng);
    let largest_class = units(768);
    let fills = ONION_ADMISSION_SENDER_UNITS / 768;
    let remainder = units(ONION_ADMISSION_SENDER_UNITS % 768);
    for tag in 0..u128::from(fills) {
        let verdict = admission.admit(
            ORIGIN_MS,
            request(1, latest_expiry(ORIGIN_MS), tag, largest_class),
        );
        assert_eq!(verdict, Ok(()));
    }
    let x = latest_expiry(ORIGIN_MS);
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, x, 100, largest_class)),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, x, 101, remainder)),
        Ok(())
    );
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, x, 102, units(1))),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    assert_eq!(
        admission.admit(ORIGIN_MS, request(2, x, 103, units(1))),
        Ok(())
    );

    let last_in_window = ORIGIN_MS + (ADMISSION_WINDOW_QUANTA_WIDE - 1) * Q + Q - 1;
    assert_eq!(
        admission.admit(
            last_in_window,
            request(1, latest_expiry(last_in_window), 104, units(1))
        ),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    let refilled = last_in_window + 1;
    assert_eq!(
        admission.admit(refilled, request(1, latest_expiry(refilled), 105, units(1))),
        Ok(())
    );
}

/// Law: the hop admits at most `64·B` units per window whatever the number of identities. A
/// rejection recycles no partition.
#[test]
fn test_global_budget_bounds_identity_rotation() {
    let mut rng = StdRng::seed_from_u64(0x0841_0006);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);
    let whole_budget = units(ONION_ADMISSION_SENDER_UNITS);
    for sender in 0..64_u32 {
        assert_eq!(
            admission.admit(
                ORIGIN_MS,
                request(sender, x, u128::from(sender), whole_budget)
            ),
            Ok(())
        );
    }
    assert_eq!(
        admission.global.load(ORIGIN_MS / Q),
        ONION_ADMISSION_GLOBAL_UNITS
    );
    let before = admission.senders.clone();
    assert_eq!(
        admission.admit(ORIGIN_MS, request(64, x, 64, units(1))),
        Err(OnionAdmissionRejection::GlobalBudget)
    );
    assert_eq!(admission.senders, before);
    let next_window = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    assert_eq!(
        admission.admit(
            next_window,
            request(64, latest_expiry(next_window), 64, units(1))
        ),
        Ok(())
    );
}

/// Law: a sender beyond the 64th takes the slot of the least recently active partition, with
/// ties broken by the least DID, and gets a fresh ledger.
#[test]
fn test_least_recently_active_partition_is_recycled() {
    let mut rng = StdRng::seed_from_u64(0x0841_0007);
    let mut admission = state(&mut rng);
    let mut tag = 0_u128;
    let mut admit = |admission: &mut OnionAdmissionState, now_ms: u128, from: u32, n: u32| {
        tag += 1;
        admission.admit(now_ms, request(from, latest_expiry(now_ms), tag, units(n)))
    };
    for sender in 0..64_u32 {
        assert_eq!(
            admit(&mut admission, ORIGIN_MS + u128::from(sender), sender, 1),
            Ok(())
        );
    }
    assert_eq!(admit(&mut admission, ORIGIN_MS + 64, 0, 1), Ok(()));
    assert_eq!(
        admit(
            &mut admission,
            ORIGIN_MS + 65,
            64,
            ONION_ADMISSION_SENDER_UNITS
        ),
        Ok(())
    );
    assert_eq!(admission.senders.len(), ONION_ADMISSION_SENDERS);
    assert!(admission.senders.contains_key(&Did::from(0_u32)));
    assert!(!admission.senders.contains_key(&Did::from(1_u32)));

    let mut tied = state(&mut rng);
    for sender in (0..64_u32).rev() {
        assert_eq!(admit(&mut tied, ORIGIN_MS, sender, 1), Ok(()));
    }
    assert_eq!(admit(&mut tied, ORIGIN_MS, 64, 1), Ok(()));
    assert!(!tied.senders.contains_key(&Did::from(0_u32)));
    assert!(tied.senders.contains_key(&Did::from(63_u32)));
}

/// Geometry lemma: `M = 64·W ≥ B / ln 2`, so a full block's fill ratio is at most `½`, and its
/// false-positive rate `(1 − e^{−B/M})ᴷ` is at most `2⁻²⁶`.
#[test]
fn test_block_geometry_meets_the_block_rate() {
    let slice_bits = f64::from(REPLAY_SLICE_BITS);
    let hashes = i32::try_from(REPLAY_BLOCK_HASHES).unwrap_or(0);
    assert_eq!(
        u32::try_from(REPLAY_SLICE_WORDS).map(|words| u64::BITS * words),
        Ok(REPLAY_SLICE_BITS)
    );
    assert!(slice_bits >= f64::from(REPLAY_BLOCK_TAGS) / std::f64::consts::LN_2);
    let fill = 1.0 - (-f64::from(REPLAY_BLOCK_TAGS) / slice_bits).exp();
    assert!(fill.powi(hashes) <= 2_f64.powi(-26));
}

/// Property, seeded: a filter saturated through admission by `64` senders × `B` one-unit
/// cells has exactly `64` full blocks. A false positive on a fresh tag is a drop, the safe
/// direction. Their measured rates stay within target: at most
/// `1.25 · 2⁻²⁶` per block and `≤ 2⁻²⁰` for the filter, and fresh probes confirm it. The global
/// cap then stops the filter from growing.
#[test]
fn test_saturated_filter_meets_false_positive_target_and_memory_bound() {
    let mut rng = StdRng::seed_from_u64(0x0841_0008);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);
    let mut insertion_false_positives = 0_u32;
    for sender in 0..64_u32 {
        let mut admitted = 0;
        while admitted < ONION_ADMISSION_SENDER_UNITS {
            match admission.admit(ORIGIN_MS, request(sender, x, rng.gen(), units(1))) {
                Ok(()) => admitted += 1,
                verdict => {
                    assert_eq!(verdict, Err(OnionAdmissionRejection::Replayed));
                    insertion_false_positives += 1;
                }
            }
        }
    }
    // A fresh tag is dropped at the filter's current rate, which averages `≤ 2⁻²¹` over the fill:
    // about `0.5` expected drops over `2²⁰` insertions.
    assert!(insertion_false_positives <= 4);
    assert_eq!(
        admission.admit(ORIGIN_MS, request(0, x, rng.gen(), units(1))),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    assert_eq!(
        admission.admit(ORIGIN_MS, request(64, x, rng.gen(), units(1))),
        Err(OnionAdmissionRejection::GlobalBudget)
    );

    let filter = admission.replay.filters().get(&x);
    let rates = filter
        .map(|filter| {
            filter
                .blocks()
                .map(|block| block.false_positive_rate())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(rates.len(), ONION_ADMISSION_SENDERS);
    assert!(rates.iter().all(|rate| *rate <= 1.25 * 2_f64.powi(-26)));
    assert!(rates.iter().sum::<f64>() <= 2_f64.powi(-20));

    let false_positives = (0..1_u32 << 16)
        .filter(|_| {
            let probe = admission.replay.probe(OnionForwardNonce::new(rng.gen()));
            admission.replay.contains(x, &probe)
        })
        .count();
    assert!(false_positives <= 2);
}

/// Law: a filter grows by whole blocks, one per `B` tags, and no inserted tag is ever missed.
#[test]
fn test_filter_grows_by_blocks_without_false_negatives() {
    let mut rng = StdRng::seed_from_u64(0x0841_0009);
    let mut store = ReplayStore::new(OnionReplayFilterKey::new(rng.gen()));
    let x = latest_expiry(ORIGIN_MS);
    let tags = (0..=REPLAY_BLOCK_TAGS)
        .map(|_| OnionForwardNonce::new(rng.gen()))
        .collect::<Vec<_>>();
    for tag in tags.iter() {
        store.insert(x, &store.probe(tag.to_owned()));
    }
    let blocks = store
        .filters()
        .get(&x)
        .map(|filter| filter.blocks().count())
        .unwrap_or_default();
    assert_eq!(blocks, 2);
    assert!(tags
        .iter()
        .all(|tag| store.contains(x, &store.probe(tag.to_owned()))));
    store.forget_through(x.as_ms());
    assert!(store.filters().is_empty());
}
