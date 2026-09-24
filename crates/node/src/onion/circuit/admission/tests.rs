//! Laws of the L9 admission step, checked against injected time and a seeded RNG.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
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
use super::OnionAdmissionCharge;
use super::OnionAdmissionLayer;
use super::OnionAdmissionLink;
use super::OnionAdmissionRejection;
use super::OnionAdmissionState;
use super::OnionAdmissionUnits;
use super::OnionBudgetRejection;
use super::OnionExpiry;
use super::OnionLinkTableFull;
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

/// The verdict of one cell through the shell's pipeline: the charge, then the admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    /// The cell was admitted.
    Admitted,
    /// The charge was refused. Nothing was charged and the cell was not decrypted.
    Unpaid(OnionBudgetRejection),
    /// The cell was charged and then rejected.
    Rejected(OnionAdmissionRejection),
}

/// A connection-registry capacity `R ≥ 1`; zero saturates to one.
fn registry(r: usize) -> NonZeroUsize {
    NonZeroUsize::MIN.saturating_add(r.saturating_sub(1))
}

/// Generation `generation` of a link from DID `did`.
fn generation(did: u32, generation: u64) -> OnionAdmissionLink {
    OnionAdmissionLink {
        did: Did::from(did),
        generation,
    }
}

/// The first generation of a link from DID `did`.
fn link(did: u32) -> OnionAdmissionLink {
    generation(did, 0)
}

/// A fresh state for `epoch` with registry capacity `R`, a probe key drawn from `rng`, and the
/// first link generation of each DID in `linked` opened at time zero.
fn state_with(
    rng: &mut StdRng,
    epoch: OnionExitEpoch,
    r: usize,
    linked: Range<u32>,
) -> OnionAdmissionState {
    let mut admission =
        OnionAdmissionState::new(epoch, OnionReplayFilterKey::new(rng.gen()), registry(r));
    for sender in linked {
        open(&mut admission, 0, link(sender));
    }
    admission
}

/// A fresh state for [`EPOCH`] with `R = 64` and live links from DIDs `0..=64`.
fn state(rng: &mut StdRng) -> OnionAdmissionState {
    state_with(rng, EPOCH, 64, 0..65)
}

/// Open `link` at `now_ms`, which the test expects the table to accept.
fn open(admission: &mut OnionAdmissionState, now_ms: u128, link: OnionAdmissionLink) {
    admission
        .link_opened(now_ms, link)
        .expect("the table has room for this link");
}

/// The on-grid expiry `k · Q`, through the parser, the only way from a wire instant to an expiry.
fn expiry(quantum: u128) -> OnionExpiry {
    OnionExpiry::from_ms(quantum * Q).expect("a multiple of Q lies on the grid")
}

/// The latest admissible expiry at `now_ms`: the greatest grid point in `(now, now + V]`.
fn latest_expiry(now_ms: u128) -> OnionExpiry {
    expiry(now_ms / Q + ADMISSION_WINDOW_QUANTA_WIDE)
}

/// `n ≥ 1` units; zero saturates to one unit.
fn units(n: u32) -> OnionAdmissionUnits {
    OnionAdmissionUnits::new(NonZeroU32::MIN.saturating_add(n.saturating_sub(1)))
}

/// A layer for the current epoch.
fn layer(expiry: OnionExpiry, tag: u128) -> OnionAdmissionLayer {
    OnionAdmissionLayer {
        epoch: EPOCH,
        expiry,
        tag: OnionForwardNonce::new(tag.to_le_bytes()),
    }
}

/// One cell of `n` units on `link` with a `γ`-valid `layer`, through the shell's pipeline: charge,
/// then admit with the token.
fn send_on(
    admission: &mut OnionAdmissionState,
    now_ms: u128,
    link: OnionAdmissionLink,
    n: u32,
    layer: OnionAdmissionLayer,
) -> Verdict {
    match admission.charge(now_ms, &link, units(n)) {
        Err(rejection) => Verdict::Unpaid(rejection),
        Ok(token) => match admission.admit(now_ms, token, layer) {
            Ok(()) => Verdict::Admitted,
            Err(rejection) => Verdict::Rejected(rejection),
        },
    }
}

/// One cell of `n` units on the first link generation of `from`.
fn send(
    admission: &mut OnionAdmissionState,
    now_ms: u128,
    from: u32,
    n: u32,
    layer: OnionAdmissionLayer,
) -> Verdict {
    send_on(admission, now_ms, link(from), n, layer)
}

/// The units charged to `from` in the window ending at the quantum of `now_ms`, if it has a
/// ledger.
fn sender_load(admission: &OnionAdmissionState, from: u32, now_ms: u128) -> Option<u32> {
    admission
        .senders
        .get(&Did::from(from))
        .map(|sender| sender.ledger.load(now_ms / Q))
}

/// The live filter keys in expiry order.
fn live(admission: &OnionAdmissionState) -> Vec<OnionExpiry> {
    admission.replay.filters().keys().copied().collect()
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
    let invalid = admission.charge(ORIGIN_MS, &sender, units(7));
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
    assert!(admission.charge(ORIGIN_MS, &sender, units(spent)).is_ok());
    let pipelined = (0..8)
        .map(|_| admission.charge(ORIGIN_MS, &sender, units(1)))
        .collect::<Vec<_>>();
    assert!(pipelined.first().is_some_and(Result::is_ok));
    assert!(pipelined
        .iter()
        .skip(1)
        .all(|charge| matches!(charge, Err(OnionBudgetRejection::SenderBudget))));
    assert_eq!(
        sender_load(&admission, 1, ORIGIN_MS),
        Some(ONION_ADMISSION_SENDER_UNITS)
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1_000, 1, layer(x, 4)),
        Verdict::Unpaid(OnionBudgetRejection::LinkNotLive)
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
    let over = Verdict::Unpaid(OnionBudgetRejection::SenderBudget);
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
        Verdict::Unpaid(OnionBudgetRejection::GlobalBudget)
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

/// Law: there is no fixed sender limit and no lockout. With `R = 100`, 150 live links, more than
/// the old 64 partitions, are all admitted.
#[test]
fn test_more_than_64_live_links_are_all_admitted() {
    let mut rng = StdRng::seed_from_u64(0x0841_000a);
    let mut admission = state_with(&mut rng, EPOCH, 100, 0..150);
    let x = latest_expiry(ORIGIN_MS);
    for sender in 0..150_u32 {
        assert_eq!(
            send(
                &mut admission,
                ORIGIN_MS,
                sender,
                1,
                layer(x, u128::from(sender))
            ),
            Verdict::Admitted
        );
    }
    assert_eq!(admission.senders.len(), 150);
}

/// Law: closing a link does not reset its ledger while it carries load, even under table pressure.
/// A DID spends `B` and closes its link. 38 churning DIDs then open, send and close, filling the
/// table: a refused churner never becomes live. When the DID reconnects within the window, as a new
/// generation, it finds its old ledger and is still over budget. Once the window passes, the sweep releases every drained churner.
#[test]
fn test_closed_link_keeps_its_ledger_until_it_drains() {
    const R: usize = 8;
    let mut rng = StdRng::seed_from_u64(0x0841_000b);
    let mut admission = state_with(&mut rng, EPOCH, R, 1..2);
    let x = latest_expiry(ORIGIN_MS);
    let spender = Did::from(1_u32);
    assert_eq!(
        send(
            &mut admission,
            ORIGIN_MS,
            1,
            ONION_ADMISSION_SENDER_UNITS,
            layer(x, 1)
        ),
        Verdict::Admitted
    );
    admission.link_closed(ORIGIN_MS, link(1));
    let mut refused = 0;
    for churner in 2..40_u32 {
        match admission.link_opened(ORIGIN_MS + 1, link(churner)) {
            Ok(()) => {
                assert_eq!(
                    send(
                        &mut admission,
                        ORIGIN_MS + 1,
                        churner,
                        1,
                        layer(x, u128::from(churner))
                    ),
                    Verdict::Admitted
                );
                admission.link_closed(ORIGIN_MS + 1, link(churner));
            }
            Err(OnionLinkTableFull) => refused += 1,
        }
        assert!(admission.senders.len() <= 2 * R);
    }
    assert!(refused > 0);
    assert!(admission.senders.contains_key(&spender));
    open(&mut admission, ORIGIN_MS + 2, generation(1, 1));
    assert_eq!(
        send_on(
            &mut admission,
            ORIGIN_MS + 2,
            generation(1, 1),
            1,
            layer(x, 100)
        ),
        Verdict::Unpaid(OnionBudgetRejection::SenderBudget)
    );
    let drained = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    assert_eq!(
        send_on(
            &mut admission,
            drained,
            generation(1, 1),
            1,
            layer(latest_expiry(drained), 101)
        ),
        Verdict::Admitted
    );
    assert_eq!(admission.senders.keys().collect::<Vec<_>>(), vec![&spender]);
}

/// Law: a full table refuses the link, and the link's cells are never charged or decrypted. The
/// shell's close of that refused link is a no-op. The peer's redial, a new generation, succeeds once
/// a closed ledger has drained and been swept.
#[test]
fn test_full_table_refuses_the_link_and_the_peer_redials() {
    let mut rng = StdRng::seed_from_u64(0x0841_000c);
    let mut admission = state_with(&mut rng, EPOCH, 1, 1..3);
    let x = latest_expiry(ORIGIN_MS);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Admitted
    );
    assert_eq!(
        admission.link_opened(ORIGIN_MS, link(3)),
        Err(OnionLinkTableFull)
    );
    let before = admission.senders.clone();
    admission.link_closed(ORIGIN_MS, link(3));
    assert_eq!(admission.senders, before);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 3, 1, layer(x, 3)),
        Verdict::Unpaid(OnionBudgetRejection::LinkNotLive)
    );

    admission.link_closed(ORIGIN_MS, link(1));
    assert_eq!(
        admission.link_opened(ORIGIN_MS + 1, generation(3, 1)),
        Err(OnionLinkTableFull)
    );
    let drained = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    open(&mut admission, drained, generation(3, 2));
    assert!(!admission.senders.contains_key(&Did::from(1_u32)));
    assert!(admission.senders.contains_key(&Did::from(2_u32)));
    assert_eq!(
        send_on(
            &mut admission,
            drained,
            generation(3, 2),
            1,
            layer(latest_expiry(drained), 3)
        ),
        Verdict::Admitted
    );
}

/// Law: liveness is per generation, so interleaved generations of one DID
/// (`open(g₀) open(g₁) close(g₀)`) leave `g₁` live and `g₀` not. Only the last close, together with
/// a drained load, releases the DID's ledger.
#[test]
fn test_ledger_counts_interleaved_link_generations() {
    let mut rng = StdRng::seed_from_u64(0x0841_0010);
    let mut admission = state_with(&mut rng, EPOCH, 4, 0..0);
    let peer = Did::from(1_u32);
    open(&mut admission, ORIGIN_MS, generation(1, 0));
    open(&mut admission, ORIGIN_MS, generation(1, 1));
    admission.link_closed(ORIGIN_MS, generation(1, 0));
    let x = latest_expiry(ORIGIN_MS);
    assert_eq!(
        send_on(&mut admission, ORIGIN_MS, generation(1, 0), 1, layer(x, 1)),
        Verdict::Unpaid(OnionBudgetRejection::LinkNotLive)
    );
    assert_eq!(
        send_on(&mut admission, ORIGIN_MS, generation(1, 1), 1, layer(x, 1)),
        Verdict::Admitted
    );
    admission.link_closed(ORIGIN_MS, generation(1, 1));
    assert!(admission.senders.contains_key(&peer));
    let drained = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    assert_eq!(
        send_on(
            &mut admission,
            drained,
            generation(1, 1),
            1,
            layer(latest_expiry(drained), 2)
        ),
        Verdict::Unpaid(OnionBudgetRejection::LinkNotLive)
    );
    assert!(admission.senders.is_empty());
}

/// Law: a close is idempotent and specific to its generation. A late close of a refused `g₀`, a
/// repeated close, and a close of an unknown generation all leave the live `g₁` live, across
/// later windows, so no stale close can lock a link out.
#[test]
fn test_a_refused_generation_cannot_close_a_live_one() {
    let mut rng = StdRng::seed_from_u64(0x0841_0012);
    let mut admission = state_with(&mut rng, EPOCH, 1, 1..3);
    let x = latest_expiry(ORIGIN_MS);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Admitted
    );
    assert_eq!(
        admission.link_opened(ORIGIN_MS, generation(3, 0)),
        Err(OnionLinkTableFull)
    );
    admission.link_closed(ORIGIN_MS, link(1));
    let drained = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    open(&mut admission, drained, generation(3, 1));
    admission.link_closed(drained, generation(3, 0));
    admission.link_closed(drained, generation(3, 0));
    admission.link_closed(drained, generation(3, 7));
    for window in 1..4_u128 {
        let now_ms = drained + window * ONION_ADMISSION_WINDOW_MS;
        assert_eq!(
            send_on(
                &mut admission,
                now_ms,
                generation(3, 1),
                1,
                layer(latest_expiry(now_ms), window)
            ),
            Verdict::Admitted
        );
    }
}

/// Law: the epoch reset keeps every live link live with a zero ledger, so every live link still has
/// a ledger. It clears the loads, the clock and the replay store, and drops the ledgers of closed
/// links. Every layer of the old epoch is then rejected, and so is every token charged before the
/// reset, because its epoch differs.
#[test]
fn test_renewal_keeps_live_links_and_clears_the_rest() {
    let mut rng = StdRng::seed_from_u64(0x0841_0015);
    let mut admission = state_with(&mut rng, EPOCH, 4, 1..3);
    let x = latest_expiry(ORIGIN_MS);
    assert_eq!(
        send(
            &mut admission,
            ORIGIN_MS,
            1,
            ONION_ADMISSION_SENDER_UNITS,
            layer(x, 1)
        ),
        Verdict::Admitted
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 2, 1, layer(x, 2)),
        Verdict::Admitted
    );
    let held = admission
        .charge(ORIGIN_MS, &link(2), units(1))
        .expect("DID 2 has headroom");
    admission.link_closed(ORIGIN_MS, link(2));
    let renewed_epoch = OnionExitEpoch::new([8; 16]);
    let mut renewed = admission.renewed(renewed_epoch, OnionReplayFilterKey::new(rng.gen()));
    assert_eq!(renewed.clock_ms, 0);
    assert_eq!(live(&renewed), Vec::new());
    assert_eq!(renewed.senders.keys().collect::<Vec<_>>(), vec![
        &Did::from(1_u32)
    ]);
    assert_eq!(sender_load(&renewed, 1, ORIGIN_MS), Some(0));
    assert_eq!(
        renewed.admit(ORIGIN_MS, held, OnionAdmissionLayer {
            epoch: renewed_epoch,
            ..layer(x, 4)
        }),
        Err(OnionAdmissionRejection::StaleEpoch)
    );
    assert_eq!(
        send(&mut renewed, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Rejected(OnionAdmissionRejection::StaleEpoch)
    );
    assert_eq!(
        send(&mut renewed, ORIGIN_MS, 1, 1, OnionAdmissionLayer {
            epoch: renewed_epoch,
            ..layer(x, 1)
        }),
        Verdict::Admitted
    );
    assert_eq!(
        send(&mut renewed, ORIGIN_MS, 2, 1, layer(x, 3)),
        Verdict::Unpaid(OnionBudgetRejection::LinkNotLive)
    );
}

/// Law (invariant): no ledger is releasable after any step. A closed ledger with load is released
/// by the first step in the quantum in which it drains, whatever that step is, and not only under
/// table pressure.
#[test]
fn test_drained_closed_ledgers_are_swept_on_the_next_quantum() {
    let mut rng = StdRng::seed_from_u64(0x0841_0011);
    let mut admission = state_with(&mut rng, EPOCH, 64, 1..3);
    let x = latest_expiry(ORIGIN_MS);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Admitted
    );
    admission.link_closed(ORIGIN_MS, link(1));
    let still_loaded = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS - 1;
    assert_eq!(
        send(
            &mut admission,
            still_loaded,
            2,
            1,
            layer(latest_expiry(still_loaded), 2)
        ),
        Verdict::Admitted
    );
    assert!(admission.senders.contains_key(&Did::from(1_u32)));
    let drained = ORIGIN_MS + ONION_ADMISSION_WINDOW_MS;
    assert_eq!(
        send(
            &mut admission,
            drained,
            2,
            1,
            layer(latest_expiry(drained), 3)
        ),
        Verdict::Admitted
    );
    assert!(!admission.senders.contains_key(&Did::from(1_u32)));
    assert!(admission
        .senders
        .values()
        .all(|sender| !sender.is_releasable_at(drained / Q)));
}

/// Property, seeded: under random link churn (48 DIDs, `R = 16`, two of them heavy), no DID is
/// ever charged more than `B` units in any window of five consecutive quanta, the table never
/// exceeds `2·R`, and no ledger is ever releasable between steps. The run hits the sender budget,
/// unlinked senders and the full table, so none of the bounds holds vacuously.
#[test]
fn test_rotation_through_many_dids_never_exceeds_the_sender_budget() {
    const R: usize = 16;
    let mut rng = StdRng::seed_from_u64(0x0841_000d);
    let mut admission = state_with(&mut rng, EPOCH, R, 0..0);
    let mut live_generations = BTreeMap::<u32, Vec<u64>>::new();
    let mut next_generation = 0_u64;
    let mut charged = BTreeMap::<(u32, u128), u32>::new();
    let mut outcomes = [0_u32; 3];
    let mut now_ms = ORIGIN_MS;
    for tag in 0..30_000_u128 {
        now_ms += rng.gen_range(0..=Q / 64);
        let sender = if rng.gen_bool(0.5) {
            rng.gen_range(0..2_u32)
        } else {
            rng.gen_range(2..48_u32)
        };
        match rng.gen_range(0..16) {
            0 => {
                let closed = live_generations
                    .get_mut(&sender)
                    .and_then(Vec::pop)
                    .unwrap_or(u64::MAX);
                admission.link_closed(now_ms, generation(sender, closed));
            }
            1 | 2 => {
                next_generation += 1;
                match admission.link_opened(now_ms, generation(sender, next_generation)) {
                    Ok(()) => live_generations
                        .entry(sender)
                        .or_default()
                        .push(next_generation),
                    Err(OnionLinkTableFull) => outcomes[2] += 1,
                }
            }
            _ => {
                let cost = rng.gen_range(1..=768_u32);
                let on = live_generations
                    .get(&sender)
                    .and_then(|generations| generations.last().copied())
                    .unwrap_or(0);
                match send_on(
                    &mut admission,
                    now_ms,
                    generation(sender, on),
                    cost,
                    layer(latest_expiry(now_ms), tag),
                ) {
                    Verdict::Admitted => {
                        *charged.entry((sender, now_ms / Q)).or_default() += cost;
                    }
                    Verdict::Unpaid(OnionBudgetRejection::SenderBudget) => outcomes[0] += 1,
                    Verdict::Unpaid(OnionBudgetRejection::LinkNotLive) => outcomes[1] += 1,
                    unexpected => assert_eq!(unexpected, Verdict::Admitted),
                }
            }
        }
        assert!(admission.senders.len() <= 2 * R);
        assert!(admission
            .senders
            .values()
            .all(|sender| !sender.is_releasable_at(now_ms / Q)));
    }
    assert!(outcomes.iter().all(|count| *count > 0), "{outcomes:?}");
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

/// Property, seeded: a filter saturated through admission by `64` senders × `B` one-unit cells has
/// exactly `64` blocks. A false positive on a fresh tag is a drop, the safe direction, and it is
/// charged like every other cell. The exact measured rates (the product of slice fills) stay
/// within target: at most `1.25 · 2⁻²⁶` per block and `≤ 2⁻²⁰` for the filter. The global budget
/// then stops the filter from growing. A smoke check with `2¹⁶` fresh probes (`2⁻⁴` hits expected at
/// the target rate) guards the query path. The sample is far too small to estimate the rate itself.
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
                    assert_eq!(verdict, Verdict::Unpaid(OnionBudgetRejection::SenderBudget));
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
        Verdict::Unpaid(OnionBudgetRejection::GlobalBudget)
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
            let probe = admission.replay.probe(OnionForwardNonce::new(rng.gen()));
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
        .charge(ORIGIN_MS, &sender, units(1))
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
        .charge(ORIGIN_MS, &sender, units(1))
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
        .charge(ORIGIN_MS, &link(1), units(1))
        .expect("the other state has headroom");
    assert_eq!(
        here.admit(ORIGIN_MS, foreign, layer(x, 1)),
        Err(OnionAdmissionRejection::StaleEpoch)
    );
    assert!(live(&here).is_empty());
}

/// Property, seeded (H1 of the round-3 review): with tokens held across quanta and admitted late,
/// no filter ever holds more tags than the units charged in its five arrival quanta, and so never
/// more than `G / u` tags of `u`-unit cells. 64 DIDs send 768-unit cells as fast as their budgets
/// allow, for twenty quanta. Each cell names a random expiry around its arrival window, some just
/// outside it, and its admission completes after a random delay of up to `V`. The run checks that
/// filters come within a factor of two of the bound, so the bound is not vacuous.
#[test]
fn test_late_admissions_never_exceed_the_global_bound_per_filter() {
    const UNITS: u32 = 768;
    let mut rng = StdRng::seed_from_u64(0x0841_0016);
    let mut admission = state(&mut rng);
    let mut pending = Vec::new();
    let mut tags = BTreeMap::<OnionExpiry, u32>::new();
    let mut settle = |admission: &mut OnionAdmissionState,
                      pending: &mut Vec<(u128, OnionAdmissionCharge, OnionAdmissionLayer)>,
                      now_ms: u128| {
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(pending)
            .into_iter()
            .partition(|(release_ms, _, _)| *release_ms <= now_ms);
        *pending = waiting;
        for (_, token, layer) in due {
            if admission.admit(now_ms, token, layer).is_ok() {
                *tags.entry(layer.expiry).or_default() += 1;
            }
        }
    };
    let mut tag = 0_u128;
    for step in 0..(20 * 32_u128) {
        let now_ms = ORIGIN_MS + step * (Q / 32);
        for _ in 0..16 {
            let sender = rng.gen_range(0..64_u32);
            if let Ok(token) = admission.charge(now_ms, &link(sender), units(UNITS)) {
                tag += 1;
                let x = expiry(now_ms / Q + rng.gen_range(1..=ADMISSION_WINDOW_QUANTA_WIDE + 1));
                let release_ms = now_ms + rng.gen_range(0..=ONION_ADMISSION_WINDOW_MS);
                pending.push((release_ms, token, layer(x, tag)));
            }
        }
        settle(&mut admission, &mut pending, now_ms);
    }
    settle(&mut admission, &mut pending, u128::MAX);
    let fullest = tags.values().copied().max().unwrap_or(0);
    assert!(fullest * UNITS <= ONION_ADMISSION_GLOBAL_UNITS);
    assert!(2 * fullest * UNITS >= ONION_ADMISSION_GLOBAL_UNITS / 5);
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
