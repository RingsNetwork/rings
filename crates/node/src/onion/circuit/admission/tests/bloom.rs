//! The replay store: block geometry, the false-positive target, the memory bound and growth.

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;

use super::expiry;
use super::latest_expiry;
use super::layer;
use super::send;
use super::state;
use super::Verdict;
use super::ORIGIN_MS;
use super::Q;
use crate::onion::circuit::admission::bloom::ReplayStore;
use crate::onion::circuit::admission::bloom::REPLAY_BLOCK_HASHES;
use crate::onion::circuit::admission::bloom::REPLAY_BLOCK_TAGS;
use crate::onion::circuit::admission::bloom::REPLAY_SLICE_BITS;
use crate::onion::circuit::admission::bloom::REPLAY_SLICE_WORDS;
use crate::onion::circuit::admission::OnionAdmissionRejection;
use crate::onion::circuit::admission::OnionChargeRejection;
use crate::onion::circuit::admission::OnionReplayFilterKey;
use crate::onion::circuit::admission::ADMISSION_WINDOW_QUANTA;
use crate::onion::circuit::admission::ADMISSION_WINDOW_QUANTA_WIDE;
use crate::onion::circuit::admission::ONION_ADMISSION_GLOBAL_UNITS;
use crate::onion::circuit::OnionReplayNonce;

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

/// Property, seeded: a filter saturated through admission by `64` senders × `B` one-unit cells has
/// exactly `64` blocks. A false positive on a fresh tag is a drop, the safe direction, and it is
/// charged like every other cell. The exact measured rates (the product of slice fills) stay within
/// target: at most `1.25 · 2⁻²⁶` per block and `≤ 2⁻²⁰` for the filter. The global budget then
/// stops the filter from growing. A smoke check with `2¹⁶` fresh probes (`2⁻⁴` hits expected at the
/// target rate) guards the query path. The sample is far too small to estimate the rate itself.
#[test]
fn test_saturated_filter_meets_the_false_positive_target() {
    let mut rng = StdRng::seed_from_u64(0x0841_0008);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);
    let mut insertion_false_positives = 0_u32;
    for sender in 0..64_u32 {
        loop {
            match send(&mut admission, ORIGIN_MS, sender, 1, layer(x, rng.gen())) {
                Verdict::Admitted => {}
                Verdict::Rejected(OnionAdmissionRejection::Replayed) => {
                    insertion_false_positives += 1;
                }
                verdict => {
                    assert_eq!(verdict, Verdict::Unpaid(OnionChargeRejection::SenderBudget));
                    break;
                }
            }
        }
    }
    // A fresh tag is dropped at the filter's current rate, which averages `≤ 2⁻²¹` over the fill:
    // about `0.5` expected drops over `2²⁰` insertions.
    assert!(insertion_false_positives <= 4);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 64, 1, layer(x, rng.gen())),
        Verdict::Unpaid(OnionChargeRejection::GlobalBudget)
    );

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
            let probe = admission.replay.probe(OnionReplayNonce::new(rng.gen()));
            admission.replay.contains(x, &probe)
        })
        .count();
    assert_eq!(false_positives, 0);
}

/// Law (memory), seeded: under sustained saturation over ten quanta, the live filters together
/// never hold more than `64 + 5` blocks. Each quantum, every one of 64 DIDs sends one-unit cells
/// spread over the five admissible expiries until a charge is refused, so the global ledger is
/// saturated at every quantum: `G` whenever the window has room and nothing more when it is full.
/// Old filters expire while new ones fill.
#[test]
fn test_all_live_filters_stay_within_the_memory_bound() {
    let mut rng = StdRng::seed_from_u64(0x0841_000e);
    let mut admission = state(&mut rng);
    let mut refusals = 0_u32;
    for step in 0..10_u128 {
        let now_ms = ORIGIN_MS + step * Q;
        let expiries = (1..=ADMISSION_WINDOW_QUANTA_WIDE)
            .map(|offset| expiry(now_ms / Q + offset))
            .collect::<Vec<_>>();
        for sender in 0..64_u32 {
            for x in expiries.iter().cycle() {
                if let Verdict::Unpaid(_) = send(
                    &mut admission,
                    now_ms,
                    sender,
                    1,
                    layer(x.to_owned(), rng.gen()),
                ) {
                    refusals += 1;
                    break;
                }
            }
        }
        assert_eq!(
            admission.global.load(now_ms / Q),
            ONION_ADMISSION_GLOBAL_UNITS
        );
        let filters = admission.replay.filters();
        let blocks = filters
            .values()
            .map(|filter| filter.blocks().count())
            .sum::<usize>();
        assert!(filters.len() <= ADMISSION_WINDOW_QUANTA);
        assert!(blocks <= 64 + filters.len());
    }
    assert_eq!(refusals, 10 * 64);
}

/// Law: a filter grows by whole blocks, one per `B` tags, and no inserted tag is ever missed.
#[test]
fn test_filter_grows_by_blocks_without_false_negatives() {
    let mut rng = StdRng::seed_from_u64(0x0841_0009);
    let mut store = ReplayStore::new(OnionReplayFilterKey::new(rng.gen()));
    let x = latest_expiry(ORIGIN_MS);
    let tags = (0..=REPLAY_BLOCK_TAGS)
        .map(|_| OnionReplayNonce::new(rng.gen()))
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
