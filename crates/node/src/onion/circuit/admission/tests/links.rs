//! Links as core generations, their ledgers, the table bound, reconciliation, and the epoch reset.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::Rng;
use rand::SeedableRng;
use rings_core::dht::Did;

use super::generation;
use super::latest_expiry;
use super::layer;
use super::link;
use super::live_filters;
use super::open;
use super::registry;
use super::send;
use super::send_on;
use super::sender_load;
use super::state;
use super::state_with;
use super::units;
use super::Verdict;
use super::EPOCH;
use super::ORIGIN_MS;
use super::Q;
use crate::onion::circuit::admission::OnionAdmissionLayer;
use crate::onion::circuit::admission::OnionAdmissionLink;
use crate::onion::circuit::admission::OnionAdmissionRejection;
use crate::onion::circuit::admission::OnionAdmissionState;
use crate::onion::circuit::admission::OnionChargeRejection;
use crate::onion::circuit::admission::OnionEpochNotFresh;
use crate::onion::circuit::admission::OnionLinkTableFull;
use crate::onion::circuit::admission::OnionRefusedLinks;
use crate::onion::circuit::admission::OnionReplayFilterKey;
use crate::onion::circuit::admission::ADMISSION_WINDOW_QUANTA_WIDE;
use crate::onion::circuit::admission::ONION_ADMISSION_SENDER_UNITS;
use crate::onion::circuit::admission::ONION_ADMISSION_WINDOW_MS;
use crate::onion::OnionProcessEpoch;

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
/// generation, it finds its old ledger and is still over budget. Once the window passes, the sweep
/// releases every drained churner.
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
        Verdict::Unpaid(OnionChargeRejection::SenderBudget)
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
/// shell's close of that refused link is a no-op. The peer's redial, a new generation, succeeds
/// once a closed ledger has drained and been swept.
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
        Verdict::Unpaid(OnionChargeRejection::LinkNotLive)
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
        Verdict::Unpaid(OnionChargeRejection::LinkNotLive)
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
        Verdict::Unpaid(OnionChargeRejection::LinkNotLive)
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

/// Law: the epoch reset rebuilds the live set from core's snapshot `L`, not from the old live set.
/// The snapshot here differs from the live set: it drops the live DID 2 and adds the non-live DID
/// 3. Every link of `L` starts with a zero ledger, so every live link still has a ledger. The reset
/// clears the loads, the clock and the replay store, and drops every other ledger. Every layer of
/// the old epoch is then rejected, and so is every token charged before the reset, because its
/// epoch differs.
#[test]
fn test_renewal_rebuilds_live_links_from_the_snapshot() {
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
        .charge(ORIGIN_MS, link(2), units(1))
        .expect("DID 2 has headroom");
    let renewed_epoch = OnionProcessEpoch::new([8; 16]);
    assert_eq!(
        admission.renew(renewed_epoch, OnionReplayFilterKey::new(rng.gen()), [
            link(1),
            link(3)
        ]),
        Ok(OnionRefusedLinks::default())
    );
    assert_eq!(admission.clock_ms, 0);
    assert_eq!(live_filters(&admission), Vec::new());
    assert_eq!(admission.senders.keys().collect::<Vec<_>>(), vec![
        &Did::from(1_u32),
        &Did::from(3_u32)
    ]);
    assert_eq!(sender_load(&admission, 1, ORIGIN_MS), Some(0));
    assert_eq!(sender_load(&admission, 3, ORIGIN_MS), Some(0));
    assert_eq!(
        admission.admit(ORIGIN_MS, held, OnionAdmissionLayer {
            epoch: renewed_epoch,
            ..layer(x, 4)
        }),
        Err(OnionAdmissionRejection::StaleEpoch)
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Rejected(OnionAdmissionRejection::StaleEpoch)
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, OnionAdmissionLayer {
            epoch: renewed_epoch,
            ..layer(x, 1)
        }),
        Verdict::Admitted
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 2, 1, layer(x, 3)),
        Verdict::Unpaid(OnionChargeRejection::LinkNotLive)
    );
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 3, 1, OnionAdmissionLayer {
            epoch: renewed_epoch,
            ..layer(x, 5)
        }),
        Verdict::Admitted
    );
}

/// Law (M1 of the round-5 review): renewing into the current epoch is refused and changes nothing,
/// so the replay store survives and an admitted `(x, ν)` cannot be admitted again.
#[test]
fn test_a_same_epoch_renewal_is_refused_and_cannot_readmit_a_replay() {
    let mut rng = StdRng::seed_from_u64(0x0841_0017);
    let mut admission = state(&mut rng);
    let x = latest_expiry(ORIGIN_MS);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Admitted
    );
    assert_eq!(
        admission.renew(EPOCH, OnionReplayFilterKey::new(rng.gen()), [link(1)]),
        Err(OnionEpochNotFresh)
    );
    assert_eq!(admission.clock_ms, ORIGIN_MS);
    assert_eq!(live_filters(&admission), vec![x]);
    assert_eq!(
        send(&mut admission, ORIGIN_MS, 1, 1, layer(x, 1)),
        Verdict::Rejected(OnionAdmissionRejection::Replayed)
    );
}

/// Law (M2 of the round-5 review): lost `Retired` events pin live links until the shell
/// reconciles with core's snapshot. Here every `Retired` is lost, so the table fills and refuses a
/// newcomer. `reconcile` then closes every generation absent from the snapshot, opens the snapshot
/// generation that was never reported (a lost `Admitted`), and returns the links it refuses.
#[test]
fn test_reconcile_recovers_lost_link_events() {
    let mut rng = StdRng::seed_from_u64(0x0841_0018);
    let mut admission = state_with(&mut rng, EPOCH, 2, 1..5);
    assert_eq!(
        admission.link_opened(ORIGIN_MS, link(5)),
        Err(OnionLinkTableFull)
    );
    let refused = admission.reconcile(ORIGIN_MS, [link(1), generation(6, 3)]);
    assert!(refused.links().is_empty());
    assert_eq!(admission.senders.keys().collect::<Vec<_>>(), vec![
        &Did::from(1_u32),
        &Did::from(6_u32)
    ]);
    open(&mut admission, ORIGIN_MS, link(5));
    assert_eq!(
        send_on(
            &mut admission,
            ORIGIN_MS,
            generation(6, 3),
            1,
            layer(latest_expiry(ORIGIN_MS), 1)
        ),
        Verdict::Admitted
    );
    assert_eq!(
        send(
            &mut admission,
            ORIGIN_MS,
            2,
            1,
            layer(latest_expiry(ORIGIN_MS), 2)
        ),
        Verdict::Unpaid(OnionChargeRejection::LinkNotLive)
    );

    // DIDs 1 and 5 are absent and unloaded, so they are released. DID 6 is absent but still
    // loaded, so its closed ledger keeps a slot until it drains. Three of the ten new links fit,
    // and reconcile opens in DID order, so DIDs 10 to 12 win and 13 to 19 are refused.
    let crowded = (10..20).map(link).collect::<Vec<_>>();
    let refused = admission.reconcile(ORIGIN_MS, crowded);
    assert_eq!(
        refused.links(),
        (13..20).map(link).collect::<Vec<_>>().as_slice()
    );
    assert_eq!(admission.senders.len(), 4);
    assert!(admission.senders.contains_key(&Did::from(6_u32)));
}

/// Law (memory bound): the live-link set is capped at `2·R` even for one DID. That can only be
/// reached if the event obligation is broken, so that many generations of one DID stay live.
#[test]
fn test_live_links_are_capped_as_a_memory_bound() {
    let mut rng = StdRng::seed_from_u64(0x0841_0019);
    let mut admission = state_with(&mut rng, EPOCH, 2, 0..0);
    for generation_index in 0..4 {
        open(&mut admission, ORIGIN_MS, generation(1, generation_index));
    }
    assert_eq!(
        admission.link_opened(ORIGIN_MS, generation(1, 4)),
        Err(OnionLinkTableFull)
    );
    assert_eq!(admission.live_link_count(), 4);
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
/// exceeds `2·R`, the live-link count equals the model's, and no ledger is ever releasable between
/// steps. Closes and charges pick a random live generation, or with probability ¼ a random
/// generation that may be stale, refused or unknown. The run hits the sender budget, links that
/// are not live, and the full table, so none of the bounds holds vacuously.
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
                let generations = live_generations.entry(sender).or_default();
                let closed = if generations.is_empty() || rng.gen_ratio(1, 4) {
                    rng.gen_range(0..=next_generation)
                } else {
                    generations.swap_remove(rng.gen_range(0..generations.len()))
                };
                generations.retain(|live| *live != closed);
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
                let on = match live_generations.get(&sender) {
                    Some(generations) if !generations.is_empty() && !rng.gen_ratio(1, 4) => {
                        generations
                            .get(rng.gen_range(0..generations.len()))
                            .copied()
                            .unwrap_or(0)
                    }
                    _ => rng.gen_range(0..=next_generation),
                };
                let modelled_live = live_generations
                    .get(&sender)
                    .is_some_and(|generations| generations.contains(&on));
                let verdict = send_on(
                    &mut admission,
                    now_ms,
                    generation(sender, on),
                    cost,
                    layer(latest_expiry(now_ms), tag),
                );
                assert_eq!(
                    verdict == Verdict::Unpaid(OnionChargeRejection::LinkNotLive),
                    !modelled_live
                );
                match verdict {
                    Verdict::Admitted => {
                        *charged.entry((sender, now_ms / Q)).or_default() += cost;
                    }
                    Verdict::Unpaid(OnionChargeRejection::SenderBudget) => outcomes[0] += 1,
                    Verdict::Unpaid(OnionChargeRejection::LinkNotLive) => outcomes[1] += 1,
                    unexpected => assert_eq!(unexpected, Verdict::Admitted),
                }
            }
        }
        assert!(admission.senders.len() <= 2 * R);
        assert_eq!(
            admission.live_link_count(),
            live_generations.values().map(Vec::len).sum::<usize>()
        );
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

/// Whether the table has no room for `link`, given the modelled number of live links: the live-link
/// set is full, or `link`'s DID has no ledger and the ledger table is full. This is the only
/// justification for a refusal.
fn is_full_for(
    admission: &OnionAdmissionState,
    live_links: usize,
    link: &OnionAdmissionLink,
) -> bool {
    live_links >= admission.capacity
        || (!admission.senders.contains_key(&link.did)
            && admission.senders.len() >= admission.capacity)
}

/// The live links of `admission`, as `(did, generation)` pairs.
fn live_links(admission: &OnionAdmissionState) -> BTreeSet<(Did, u64)> {
    admission
        .senders
        .iter()
        .flat_map(|(&did, sender)| sender.live.iter().map(move |&generation| (did, generation)))
        .collect()
}

/// The model of the lost-events property: core's truth `L`, the live set admission should hold,
/// and two states driven in lockstep, one reconciled with sorted snapshots and one with shuffled
/// snapshots.
struct Lockstep {
    /// The seeded source of every choice.
    rng: StdRng,
    /// The state reconciled with sorted snapshots.
    sorted: OnionAdmissionState,
    /// The state reconciled with shuffled snapshots.
    shuffled: OnionAdmissionState,
    /// Core's registry of live links, `L`.
    truth: BTreeSet<(Did, u64)>,
    /// The live set admission should hold, given the events it received.
    modelled: BTreeSet<(Did, u64)>,
    /// The last generation core issued.
    next_generation: u64,
    /// Reconciliations run.
    reconciliations: u32,
    /// Events lost before reaching admission.
    lost: u32,
    /// Delivered opens, delivered closes, event refusals, and reconciliations with a refusal.
    occurred: [u32; 4],
}

impl Lockstep {
    /// Two fresh states with `R = 8` under one probe key, and empty truth.
    fn new(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let key: [u8; 32] = rng.gen();
        let fresh = || OnionAdmissionState::new(EPOCH, OnionReplayFilterKey::new(key), registry(8));
        Self {
            rng,
            sorted: fresh(),
            shuffled: fresh(),
            truth: BTreeSet::new(),
            modelled: BTreeSet::new(),
            next_generation: 0,
            reconciliations: 0,
            lost: 0,
            occurred: [0; 4],
        }
    }

    /// Core admits a new generation of `did`. The event is lost with probability ⅙. Otherwise both
    /// states open it; a refusal must be justified, and the shell then closes the link in core.
    fn open(&mut self, now_ms: u128, did: Did) {
        self.next_generation += 1;
        let opened = OnionAdmissionLink {
            did,
            generation: self.next_generation,
        };
        self.truth.insert((did, opened.generation));
        if self.rng.gen_ratio(1, 6) {
            self.lost += 1;
            return;
        }
        let verdict = self.sorted.link_opened(now_ms, opened);
        assert_eq!(self.shuffled.link_opened(now_ms, opened), verdict);
        match verdict {
            Ok(()) => {
                self.occurred[0] += 1;
                self.modelled.insert((did, opened.generation));
            }
            Err(OnionLinkTableFull) => {
                self.occurred[2] += 1;
                assert!(is_full_for(&self.sorted, self.modelled.len(), &opened));
                self.truth.remove(&(did, opened.generation));
            }
        }
    }

    /// Core retires a random live generation. The event is lost with probability ⅙; otherwise both
    /// states close it.
    fn retire(&mut self, now_ms: u128) {
        let index = self.rng.gen_range(0..self.truth.len().max(1));
        let Some((did, generation)) = self.truth.iter().nth(index).copied() else {
            return;
        };
        self.truth.remove(&(did, generation));
        if self.rng.gen_ratio(1, 6) {
            self.lost += 1;
            return;
        }
        let closed = OnionAdmissionLink { did, generation };
        self.sorted.link_closed(now_ms, closed);
        self.shuffled.link_closed(now_ms, closed);
        self.occurred[1] += 1;
        self.modelled.remove(&(did, generation));
    }

    /// Reconcile both states against `L`, sorted and shuffled, and check the four reconciliation
    /// laws and the justification of every refusal. The shell then closes the refused links in
    /// core.
    fn reconcile(&mut self, now_ms: u128) {
        self.reconciliations += 1;
        let snapshot = self
            .truth
            .iter()
            .map(|&(did, generation)| OnionAdmissionLink { did, generation })
            .collect::<Vec<_>>();
        let mut permuted = snapshot.clone();
        permuted.shuffle(&mut self.rng);
        let before = live_links(&self.sorted);
        let refused = self.sorted.reconcile(now_ms, snapshot.iter().copied());
        assert_eq!(
            self.shuffled.reconcile(now_ms, permuted.iter().copied()),
            refused
        );
        let refused_set = refused
            .links()
            .iter()
            .map(|link| (link.did, link.generation))
            .collect::<BTreeSet<_>>();
        let expected = self
            .truth
            .difference(&refused_set)
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(live_links(&self.sorted), expected);
        assert!(before
            .intersection(&self.truth)
            .all(|link| !refused_set.contains(link)));
        assert!(refused
            .links()
            .iter()
            .all(|link| is_full_for(&self.sorted, expected.len(), link)));
        self.occurred[3] += u32::from(!refused.links().is_empty());
        let ledgers = self.sorted.senders.clone();
        assert_eq!(
            self.sorted.reconcile(now_ms, snapshot.iter().copied()),
            refused
        );
        assert_eq!(
            self.shuffled.reconcile(now_ms, permuted.iter().copied()),
            refused
        );
        assert_eq!(self.sorted.senders, ledgers);
        self.truth = expected;
        self.modelled = self.truth.clone();
    }

    /// Charge a cell on a random modelled link, or with probability ¼ on a possibly stale
    /// generation of `did`. Both states agree, and `LinkNotLive` holds exactly when the model lacks
    /// the link.
    fn charge(&mut self, now_ms: u128, did: Did, tag: u128) {
        let index = self.rng.gen_range(0..self.modelled.len().max(1));
        let on = match self.modelled.iter().nth(index) {
            Some(&(did, generation)) if !self.rng.gen_ratio(1, 4) => {
                OnionAdmissionLink { did, generation }
            }
            _ => OnionAdmissionLink {
                did,
                generation: self.rng.gen_range(0..=self.next_generation),
            },
        };
        let cost = self.rng.gen_range(1..=64_u32);
        let x = layer(latest_expiry(now_ms), tag);
        let verdict = send_on(&mut self.sorted, now_ms, on, cost, x);
        assert_eq!(send_on(&mut self.shuffled, now_ms, on, cost, x), verdict);
        assert_eq!(
            verdict == Verdict::Unpaid(OnionChargeRejection::LinkNotLive),
            !self.modelled.contains(&(on.did, on.generation))
        );
    }
}

/// Property, seeded (M2 of the round-6 review): with lost `Admitted`/`Retired` events, periodic
/// reconciliation against core's truth `L` restores the laws. Two states are driven in lockstep
/// with the same inputs; one receives every snapshot sorted and the other shuffled. After every
/// reconciliation:
/// * `𝓛ᵢ = L \ refused`;
/// * no link that was live before and is in `L` is refused;
/// * a second reconciliation with the same snapshot changes nothing and refuses the same links;
/// * the shuffled snapshot yields the same state as the sorted one.
///
/// Every refusal is justified: after a refused `link_opened`, or after a reconciliation that
/// refuses a link, the table has no room for that link, counting live links from the model.
/// Throughout, a charge is `LinkNotLive` exactly when the modelled live set lacks the link, and
/// the state's live set equals the model's after every step. The shell closes every refused link
/// in core, so the model removes it from `L`. The run asserts that delivered opens, delivered
/// closes, event refusals and reconcile refusals all occur.
#[test]
fn test_reconcile_restores_core_truth_under_lost_events() {
    let mut model = Lockstep::new(0x0841_001b);
    let mut now_ms = ORIGIN_MS;
    for tag in 0..6_000_u128 {
        now_ms += model.rng.gen_range(0..=Q / 16);
        let did = Did::from(model.rng.gen_range(0..24_u32));
        match model.rng.gen_range(0..16) {
            0 | 1 => model.open(now_ms, did),
            2 => model.retire(now_ms),
            3 => model.reconcile(now_ms),
            _ => model.charge(now_ms, did, tag),
        }
        assert_eq!(live_links(&model.sorted), model.modelled);
        assert_eq!(model.sorted.senders, model.shuffled.senders);
    }
    assert!(
        model.reconciliations > 100 && model.lost > 50,
        "{} {}",
        model.reconciliations,
        model.lost
    );
    assert!(
        model.occurred.iter().all(|count| *count > 0),
        "{:?}",
        model.occurred
    );
}
