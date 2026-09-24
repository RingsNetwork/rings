//! Laws of the L9 admission step, checked against injected time and a seeded RNG.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::ops::Range;

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

/// A connection-registry capacity `R ≥ 1`; zero saturates to one.
fn registry(r: usize) -> NonZeroUsize {
    NonZeroUsize::MIN.saturating_add(r.saturating_sub(1))
}

/// A fresh state for `epoch` with registry capacity `R`, a probe key drawn from `rng`, and links
/// opened at time zero from the DIDs in `linked`.
fn state_with(
    rng: &mut StdRng,
    epoch: OnionExitEpoch,
    r: usize,
    linked: Range<u32>,
) -> OnionAdmissionState {
    let mut admission =
        OnionAdmissionState::new(epoch, OnionReplayFilterKey::new(rng.gen()), registry(r));
    for sender in linked {
        assert_eq!(admission.link_opened(0, Did::from(sender)), Ok(()));
    }
    admission
}

/// A fresh state for [`EPOCH`] with `R = 64` and links from DIDs `0..=64`.
fn state(rng: &mut StdRng) -> OnionAdmissionState {
    state_with(rng, EPOCH, 64, 0..65)
}

/// The on-grid expiry `k · Q`.
fn expiry(quantum: u128) -> OnionExpiry {
    OnionExpiry(quantum)
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
    let mut admission = state(&mut rng);
    let mut admitted = HashSet::new();
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

/// Law (D2): a restarted process rejects every layer sealed for the previous epoch. Each such
/// cell is still charged, and none enters the replay store.
#[test]
fn test_new_epoch_rejects_every_layer_of_the_old_one() {
    let mut rng = StdRng::seed_from_u64(0x0841_0004);
    let x = latest_expiry(ORIGIN_MS);
    let mut before = state(&mut rng);
    assert_eq!(before.admit(ORIGIN_MS, request(1, x, 1, units(1))), Ok(()));

    let mut restarted = state_with(&mut rng, OnionExitEpoch::new([8; 16]), 64, 1..2);
    for tag in 0..64 {
        assert_eq!(
            restarted.admit(ORIGIN_MS, request(1, x, tag, units(1))),
            Err(OnionAdmissionRejection::StaleEpoch)
        );
    }
    assert_eq!(restarted.global.load(ORIGIN_MS / Q), 64);
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

/// Law (charging): every cell whose key was computed is charged exactly once. Replayed, expired
/// and `γ`-invalid cells pay; a `γ`-invalid cell leaves the replay store untouched. A cell without
/// headroom is rejected by the `headroom` query and by `admit` alike, and is charged nothing.
#[test]
fn test_every_computed_cell_is_charged_exactly_once() {
    let mut rng = StdRng::seed_from_u64(0x0841_000f);
    let mut admission = state(&mut rng);
    let quantum = ORIGIN_MS / Q;
    let x = latest_expiry(ORIGIN_MS);
    let sender = Did::from(1_u32);
    let load = |admission: &OnionAdmissionState| {
        admission
            .senders
            .get(&sender)
            .map(|ledger| ledger.ledger.load(quantum))
    };

    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, x, 1, units(2))),
        Ok(())
    );
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, x, 1, units(3))),
        Err(OnionAdmissionRejection::Replayed)
    );
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, expiry(quantum), 2, units(5))),
        Err(OnionAdmissionRejection::OutsideWindow)
    );
    assert_eq!(
        admission.charge_invalid(ORIGIN_MS, &sender, units(7)),
        Ok(())
    );
    assert_eq!(load(&admission), Some(2 + 3 + 5 + 7));
    assert_eq!(admission.global.load(quantum), 2 + 3 + 5 + 7);
    assert_eq!(
        admission
            .replay
            .filters()
            .get(&x)
            .map(|filter| filter.blocks().count()),
        Some(1)
    );

    let rest = units(ONION_ADMISSION_SENDER_UNITS - 17);
    assert_eq!(admission.headroom(ORIGIN_MS, &sender, rest), Ok(()));
    assert_eq!(
        admission.headroom(ORIGIN_MS, &sender, units(ONION_ADMISSION_SENDER_UNITS)),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    assert_eq!(load(&admission), Some(17));
    assert_eq!(
        admission.admit(
            ORIGIN_MS,
            request(1, x, 3, units(ONION_ADMISSION_SENDER_UNITS))
        ),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    assert_eq!(
        admission.charge_invalid(ORIGIN_MS, &sender, units(ONION_ADMISSION_SENDER_UNITS)),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    assert_eq!(load(&admission), Some(17));
    assert_eq!(
        admission.headroom(ORIGIN_MS, &Did::from(1_000_u32), units(1)),
        Err(OnionAdmissionRejection::UnlinkedSender)
    );
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1_000, x, 4, units(1))),
        Err(OnionAdmissionRejection::UnlinkedSender)
    );
    assert_eq!(admission.global.load(quantum), 17);
}

/// Law: the hop admits at most `G = 64·B` units per window, however many DIDs send. A rejected
/// cell leaves every ledger unchanged.
#[test]
fn test_global_budget_bounds_every_sender_together() {
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

/// Law: there is no fixed sender limit and no lockout. With `R = 100`, 150 live links, more than
/// the old 64 partitions, are all admitted.
#[test]
fn test_more_than_64_live_links_are_all_admitted() {
    let mut rng = StdRng::seed_from_u64(0x0841_000a);
    let mut admission = state_with(&mut rng, EPOCH, 100, 0..150);
    let x = latest_expiry(ORIGIN_MS);
    for sender in 0..150_u32 {
        assert_eq!(
            admission.admit(ORIGIN_MS, request(sender, x, u128::from(sender), units(1))),
            Ok(())
        );
    }
    assert_eq!(admission.senders.len(), 150);
}

/// Law: closing a link does not reset its ledger while it carries load. A DID that spends `B`,
/// closes its link while other DIDs churn, and reconnects within the window finds its old ledger
/// and is still over budget. Its budget returns only when the charge leaves the window.
#[test]
fn test_closed_link_keeps_its_ledger_until_it_drains() {
    let mut rng = StdRng::seed_from_u64(0x0841_000b);
    let mut admission = state_with(&mut rng, EPOCH, 4, 1..2);
    let x = latest_expiry(ORIGIN_MS);
    let spender = Did::from(1_u32);
    assert_eq!(
        admission.admit(
            ORIGIN_MS,
            request(1, x, 1, units(ONION_ADMISSION_SENDER_UNITS))
        ),
        Ok(())
    );
    admission.link_closed(ORIGIN_MS, &spender);
    for churner in 2..40_u32 {
        assert_eq!(
            admission.link_opened(ORIGIN_MS + 1, Did::from(churner)),
            Ok(())
        );
        admission.link_closed(ORIGIN_MS + 1, &Did::from(churner));
    }
    assert!(admission.senders.contains_key(&spender));
    assert_eq!(admission.link_opened(ORIGIN_MS + 2, spender), Ok(()));
    assert_eq!(
        admission.admit(ORIGIN_MS + 2, request(1, x, 100, units(1))),
        Err(OnionAdmissionRejection::SenderBudget)
    );
    let drained = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    assert_eq!(
        admission.admit(drained, request(1, latest_expiry(drained), 101, units(1))),
        Ok(())
    );
}

/// Law: a full ledger table rejects a new link (fail closed), and a closed ledger makes room only
/// once it has drained.
#[test]
fn test_full_table_rejects_a_new_link_until_a_closed_ledger_drains() {
    let mut rng = StdRng::seed_from_u64(0x0841_000c);
    let mut admission = state_with(&mut rng, EPOCH, 1, 1..3);
    let x = latest_expiry(ORIGIN_MS);
    let newcomer = Did::from(3_u32);
    assert_eq!(
        admission.admit(ORIGIN_MS, request(1, x, 1, units(1))),
        Ok(())
    );
    assert_eq!(
        admission.link_opened(ORIGIN_MS, newcomer),
        Err(OnionAdmissionRejection::SenderTableFull)
    );
    admission.link_closed(ORIGIN_MS, &Did::from(1_u32));
    assert_eq!(
        admission.link_opened(ORIGIN_MS + 1, newcomer),
        Err(OnionAdmissionRejection::SenderTableFull)
    );
    let drained = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    assert_eq!(admission.link_opened(drained, newcomer), Ok(()));
    assert!(!admission.senders.contains_key(&Did::from(1_u32)));
    assert!(admission.senders.contains_key(&Did::from(2_u32)));
    assert_eq!(
        admission.admit(drained, request(3, latest_expiry(drained), 3, units(1))),
        Ok(())
    );
}

/// Property, seeded: under random link churn (48 DIDs, `R = 16`, two of them heavy), no DID is
/// ever charged more than `B` units in any window of five consecutive quanta, and the table never
/// exceeds `2·R`. The run hits the sender budget, the full table and unlinked senders, so none of
/// the bounds holds vacuously.
#[test]
fn test_rotation_through_many_dids_never_exceeds_the_sender_budget() {
    let mut rng = StdRng::seed_from_u64(0x0841_000d);
    let mut admission = state_with(&mut rng, EPOCH, 16, 0..0);
    let mut charged = BTreeMap::<(u32, u128), u32>::new();
    let mut rejections = [0_u32; 3];
    let mut now_ms = ORIGIN_MS;
    for tag in 0..30_000_u128 {
        now_ms += rng.gen_range(0..=Q / 64);
        let sender = if rng.gen_bool(0.5) {
            rng.gen_range(0..2_u32)
        } else {
            rng.gen_range(2..48_u32)
        };
        let verdict = match rng.gen_range(0..16) {
            0 => {
                admission.link_closed(now_ms, &Did::from(sender));
                Ok(())
            }
            1 | 2 => admission.link_opened(now_ms, Did::from(sender)),
            _ => {
                let cost = rng.gen_range(1..=768_u32);
                let verdict = admission.admit(
                    now_ms,
                    request(sender, latest_expiry(now_ms), tag, units(cost)),
                );
                if verdict.is_ok() {
                    *charged.entry((sender, now_ms / Q)).or_default() += cost;
                }
                verdict
            }
        };
        match verdict {
            Ok(()) => {}
            Err(OnionAdmissionRejection::SenderBudget) => rejections[0] += 1,
            Err(OnionAdmissionRejection::UnlinkedSender) => rejections[1] += 1,
            Err(OnionAdmissionRejection::SenderTableFull) => rejections[2] += 1,
            unexpected => assert_eq!(unexpected, Ok(())),
        }
        assert!(admission.senders.len() <= 32);
    }
    assert!(rejections.iter().all(|count| *count > 0), "{rejections:?}");
    for (sender, quantum) in charged.keys() {
        let window = charged
            .range(
                (
                    *sender,
                    quantum.saturating_sub(ADMISSION_WINDOW_QUANTA_WIDE - 1),
                )..=(*sender, *quantum),
            )
            .map(|(_, units)| *units)
            .sum::<u32>();
        assert!(window <= ONION_ADMISSION_SENDER_UNITS);
    }
}

/// Geometry lemma: the exact expected fill of a slice after `B` insertions is
/// `p_B = 1 − (1 − 1/M)^B ≤ ½`, and slices are independent, so a full block's false-positive rate
/// is exactly `p_Bᴷ ≤ 2⁻²⁶`. It also checks that `M = 64·W`.
#[test]
fn test_block_geometry_meets_the_block_rate() {
    let slice_bits = f64::from(REPLAY_SLICE_BITS);
    let hashes = i32::try_from(REPLAY_BLOCK_HASHES).unwrap_or(0);
    let tags = i32::try_from(REPLAY_BLOCK_TAGS).unwrap_or(i32::MAX);
    assert_eq!(
        u32::try_from(REPLAY_SLICE_WORDS).map(|words| u64::BITS * words),
        Ok(REPLAY_SLICE_BITS)
    );
    let fill = 1.0 - (1.0 - 1.0 / slice_bits).powi(tags);
    assert!(fill <= 0.5);
    assert!(fill.powi(hashes) <= 2_f64.powi(-26));
}

/// Fill `admission` to the global budget with one-unit cells from DIDs `0..64` at `ORIGIN_MS`,
/// cycling each DID's cells over `expiries`. Each DID sends until its own budget is spent. Returns
/// the number of fresh tags dropped as false positives, which are charged like every other cell.
fn saturate(
    admission: &mut OnionAdmissionState,
    rng: &mut StdRng,
    expiries: &[OnionExpiry],
) -> u32 {
    let mut insertion_false_positives = 0;
    for sender in 0..64_u32 {
        for x in expiries.iter().cycle() {
            match admission.admit(
                ORIGIN_MS,
                request(sender, x.to_owned(), rng.gen(), units(1)),
            ) {
                Ok(()) => {}
                Err(OnionAdmissionRejection::Replayed) => insertion_false_positives += 1,
                verdict => {
                    assert_eq!(verdict, Err(OnionAdmissionRejection::SenderBudget));
                    break;
                }
            }
        }
    }
    assert_eq!(
        admission.admit(
            ORIGIN_MS,
            request(64, latest_expiry(ORIGIN_MS), 0, units(1))
        ),
        Err(OnionAdmissionRejection::GlobalBudget)
    );
    insertion_false_positives
}

/// Property, seeded: a filter saturated through admission by `64` senders × `B` one-unit cells has
/// exactly `64` blocks. A false positive on a fresh tag is a drop, the safe direction. The exact
/// measured rates (the product of slice fills) stay within target: at most `1.25 · 2⁻²⁶` per block
/// and `≤ 2⁻²⁰` for the filter. A smoke check with `2¹⁶` fresh probes (`2⁻⁴` hits expected at the
/// target rate) guards the query path. The sample is far too small to estimate the rate itself.
#[test]
fn test_saturated_filter_meets_the_false_positive_target() {
    let mut rng = StdRng::seed_from_u64(0x0841_0008);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);
    // A fresh tag is dropped at the filter's current rate, which averages `≤ 2⁻²¹` over the fill:
    // about `0.5` expected drops over `2²⁰` insertions.
    assert!(saturate(&mut admission, &mut rng, &[x]) <= 4);

    let rates = admission
        .replay
        .filters()
        .get(&x)
        .map(|filter| {
            filter
                .blocks()
                .map(|block| block.false_positive_rate())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(rates.len(), 64);
    assert!(rates.iter().all(|rate| *rate <= 1.25 * 2_f64.powi(-26)));
    assert!(rates.iter().sum::<f64>() <= 2_f64.powi(-20));

    let false_positives = (0..1_u32 << 16)
        .filter(|_| {
            let probe = admission.replay.probe(OnionForwardNonce::new(rng.gen()));
            admission.replay.contains(x, &probe)
        })
        .count();
    assert_eq!(false_positives, 0);
}

/// Law (memory): with the global budget saturated across all five admissible expiries at once,
/// the live filters together hold at most `64·B` tags in at most `64 + 5` blocks.
#[test]
fn test_all_live_filters_stay_within_the_memory_bound() {
    let mut rng = StdRng::seed_from_u64(0x0841_000e);
    let mut admission = state(&mut rng);
    let expiries = (1..=ADMISSION_WINDOW_QUANTA_WIDE)
        .map(|offset| expiry(ORIGIN_MS / Q + offset))
        .collect::<Vec<_>>();
    saturate(&mut admission, &mut rng, expiries.as_slice());
    let filters = admission.replay.filters();
    let blocks = filters
        .values()
        .map(|filter| filter.blocks().count())
        .sum::<usize>();
    assert_eq!(filters.len(), ADMISSION_WINDOW_QUANTA);
    assert!(blocks <= 64 + ADMISSION_WINDOW_QUANTA);
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
