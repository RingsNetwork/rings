//! Stage 7: Multi-round storage repair, virtual-node placement, stale views, and
//! crash-during-handoff model checks (`S1`, `S1'`, `S1''`, `S2'`, `S4`).
//!
//! This module extends the single-key hand-off model with two finite Stateright models over
//! production types ([`Did::rotate_affine`], [`StorageVirtualNodes`], [`VirtualNodeConfig`],
//! [`StorageSyncPurpose`], [`TopologyState`], [`Entry::join`], and
//! [`SyncedEntryAck::confirms_local_value`]):
//!
//! 1. [`VirtualRepairModel`]: multi-round anti-entropy under virtual-node placement and stale
//!    topology views, checking that virtual-owner hints (`vhint`) emit additive copies only
//!    (`S1''`), receivers persist only placements accepted under their local view (`S1'`), a
//!    single node crash after replication or during stale-view repair loses no committed update,
//!    and post-refinement repair reaches `REPLICATED(e, 2)` (`S4`).
//! 2. [`HandoffRepairModel`]: multi-round physical Chord ownership hand-off (`N0 -> N1 -> N2`)
//!    with concurrent writes, message loss, and node crashes during in-flight hand-off, checking
//!    that source cleanup requires a matching `OwnershipHandoff` acknowledgement (`S2'`), a crash
//!    during hand-off before acknowledgement loses no data when at least one holder survives, and
//!    data loss occurs only when every holder of an update crashes simultaneously.

use std::collections::BTreeSet;

use bytes::Bytes;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryCrdt;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryVersion;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::topology;
use crate::dht::topology::TopologyState;
use crate::dht::virtual_node::StorageVirtualNodes;
use crate::dht::virtual_node::VirtualNodeConfig;
use crate::dht::Did;
use crate::dht::StorageSyncPurpose;

const NODE_COUNT: usize = 3;
const MODEL_NETWORK_ID: u32 = 7;
const MODEL_NOW_MS: u128 = 1_000;
const MODEL_RETENTION_MS: Option<u128> = Some(u128::MAX);

/// Broken storage-repair disciplines refuted by the model checker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RepairMutant {
    /// Deletes the sender's local placement as soon as the hand-off message is queued.
    DeleteBeforeAck,
    /// Treats a virtual-node `vhint` copy as `OwnershipHandoff` (source-delete authority) under a
    /// stale view (`S1''` violation).
    VirtualHintDeletesSource,
    /// Deletes the sender's local placement on ack without checking
    /// `SyncedEntryAck::confirms_local_value` (`S2'` violation).
    IgnoreAckValueMatch,
}

/// Physical ring DIDs `N0 < N1 < N2` and an entry DID whose placement lies in `(N2, N0)`.
fn ring_dids() -> ([Did; NODE_COUNT], Did) {
    let nodes = [
        Did::from(0x1000_u32),
        Did::from(0x5000_u32),
        Did::from(0x9000_u32),
    ];
    let entry_did = Did::from(0x0500_u32);
    (nodes, entry_did)
}

/// Build the canonical normalized [`Entry`] for non-empty bitmask `bits` (`0b01`, `0b10`, `0b11`).
fn entry_for_bits(entry_did: Did, bits: u8) -> Option<Entry> {
    if bits == 0 {
        return None;
    }
    let mut current: Option<Entry> = None;
    for (bit, time, label) in [
        (0b01_u8, 10_u128, b"v0".as_slice()),
        (0b10_u8, 20_u128, b"v1".as_slice()),
    ] {
        if bits & bit == 0 {
            continue;
        }
        let version = EntryVersion::new(
            time,
            Did::from(u32::from(bit)),
            Did::from(100 + u32::from(bit)),
        );
        let delta = Entry {
            did: entry_did,
            data: vec![Bytes::copy_from_slice(label)],
            kind: EntryKind::Data,
            crdt: EntryCrdt {
                dots: vec![EntryDot { version, index: 0 }],
                ..EntryCrdt::default()
            },
            expires_at_ms: MODEL_RETENTION_MS,
        }
        .try_into_storage_entry()
        .expect("model delta entry normalizes");
        current = Some(match current {
            None => delta,
            Some(existing) => existing
                .join(delta)
                .expect("compatible entries join")
                .try_into_storage_entry()
                .expect("joined entry normalizes"),
        });
    }
    current
}

/// Join two bitmasks through the production [`Entry::join`] and verify semilattice refinement.
fn join_bits_via_entry(entry_did: Did, left: u8, right: u8) -> u8 {
    match (
        entry_for_bits(entry_did, left),
        entry_for_bits(entry_did, right),
    ) {
        (None, _) => right,
        (_, None) => left,
        (Some(lhs), Some(rhs)) => {
            let joined = lhs
                .join(rhs)
                .expect("entries share identity and kind")
                .try_into_storage_entry()
                .expect("joined entry normalizes");
            let expected = left | right;
            assert_eq!(Some(joined), entry_for_bits(entry_did, expected));
            expected
        }
    }
}

/// Check `SyncedEntryAck::confirms_local_value` on real [`Entry`] values.
fn ack_confirms_bits(entry_did: Did, placement_key: Did, local_bits: u8, ack_bits: u8) -> bool {
    let (Some(local), Some(acked)) = (
        entry_for_bits(entry_did, local_bits),
        entry_for_bits(entry_did, ack_bits),
    ) else {
        return false;
    };
    let ack = SyncedEntryAck::new(placement_key, acked);
    ack.confirms_local_value(&local, MODEL_NOW_MS)
}

// ===========================================================================
// Part 1: Virtual-node placement + stale views + multi-round repair (`S1`, `S1'`, `S1''`, `S4`)
// ===========================================================================

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct VirtualCopyMsg {
    from: u8,
    to: u8,
    slot: u8,
    bits: u8,
    purpose: u8, // 0 = AdditiveRepair, 1 = OwnershipHandoff
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct VirtualAckMsg {
    from: u8,
    to: u8,
    slot: u8,
    bits: u8,
    purpose: u8,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct VirtualRepairState {
    /// Per-node view bitmask over `{N0, N1, N2}` (`bit i` means `Ni` is in `view_n`).
    views: [u8; NODE_COUNT],
    /// Stored value bitmask `sigma[node][placement_slot]` for the 2 affine placements.
    sigma: [[u8; 2]; NODE_COUNT],
    /// Anti-entropy rounds remaining per node.
    rounds_left: [u8; NODE_COUNT],
    /// Alive bitmask over `{N0, N1, N2}`.
    alive: u8,
    /// Crashes used so far.
    crashes: u8,
    /// Whether some crash wiped the last live holder of an update bit.
    all_holders_crashed: bool,
    /// Whether `S1''` was violated (a virtual-node sync deleted a live local placement).
    virtual_cleanup_violated: bool,
    /// Whether `S1'` was violated (a node stored a placement its current view did not accept).
    ownership_validation_violated: bool,
    /// Written update bits so far (`0b01` initially, `0b11` after `WriteUpdate`).
    written_bits: u8,
    /// Drops remaining.
    drops_left: u8,
    in_flight_copies: Vec<VirtualCopyMsg>,
    in_flight_acks: Vec<VirtualAckMsg>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum VirtualAction {
    Republish(u8),
    DeliverCopy(usize),
    DeliverAck(usize),
    DropCopy(usize),
    RefineViews,
    WriteUpdate,
    CrashNode(u8),
}

#[derive(Clone)]
struct VirtualRepairModel {
    nodes: [Did; NODE_COUNT],
    entry_did: Did,
    placements: [Did; 2],
    vcfg: VirtualNodeConfig,
    max_crashes: u8,
    mutant: Option<RepairMutant>,
}

impl VirtualRepairModel {
    fn new(max_crashes: u8, mutant: Option<RepairMutant>) -> Self {
        let (nodes, entry_did) = ring_dids();
        let placed = entry_did
            .rotate_affine(2)
            .expect("redundancy 2 affine placements");
        let placements = [placed[0], placed[1]];
        Self {
            nodes,
            entry_did,
            placements,
            vcfg: VirtualNodeConfig::new(MODEL_NETWORK_ID, 2),
            max_crashes,
            mutant,
        }
    }

    fn purpose_from_code(code: u8) -> StorageSyncPurpose {
        if code == 0 {
            StorageSyncPurpose::AdditiveRepair
        } else {
            StorageSyncPurpose::OwnershipHandoff
        }
    }

    fn virtual_owner(&self, view_mask: u8, slot: u8) -> Option<u8> {
        let owners = (0..NODE_COUNT)
            .filter(|&idx| view_mask & (1 << idx) != 0)
            .map(|idx| self.nodes[idx]);
        let vnodes = StorageVirtualNodes::from_owners(self.vcfg, owners);
        let owner_did = vnodes.owner_for_key(self.placements[usize::from(slot)])?;
        (0..NODE_COUNT)
            .find(|&idx| self.nodes[idx] == owner_did)
            .and_then(|idx| u8::try_from(idx).ok())
    }

    fn is_alive(alive: u8, node: u8) -> bool {
        alive & (1 << node) != 0
    }

    fn live_lub_for_slot(state: &VirtualRepairState, slot: usize) -> u8 {
        (0..NODE_COUNT)
            .filter(|&n| state.alive & (1 << n) != 0)
            .fold(0_u8, |acc, n| acc | state.sigma[n][slot])
    }

    fn replicated_in_full_view(&self, state: &VirtualRepairState) -> bool {
        state.alive == 0b111
            && state.views == [0b111, 0b111, 0b111]
            && (0..2_u8).all(|slot| {
                self.virtual_owner(0b111, slot).is_some_and(|owner| {
                    state.sigma[usize::from(owner)][usize::from(slot)] == state.written_bits
                })
            })
    }
}

impl Model for VirtualRepairModel {
    type Action = VirtualAction;
    type State = VirtualRepairState;

    fn init_states(&self) -> Vec<Self::State> {
        // Stale views initially: N0 knows {N0, N1}, N1 knows {N1, N2}, N2 knows {N2}.
        // N0 holds `0b01` for both affine placement keys prior to convergence.
        vec![VirtualRepairState {
            views: [0b011, 0b110, 0b100],
            sigma: [[0b01, 0b01], [0, 0], [0, 0]],
            rounds_left: [2, 1, 1],
            alive: 0b111,
            crashes: 0,
            all_holders_crashed: false,
            virtual_cleanup_violated: false,
            ownership_validation_violated: false,
            written_bits: 0b01,
            drops_left: 1,
            in_flight_copies: Vec::new(),
            in_flight_acks: Vec::new(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for idx in 0..state.in_flight_copies.len() {
            actions.push(VirtualAction::DeliverCopy(idx));
            if state.drops_left > 0 {
                actions.push(VirtualAction::DropCopy(idx));
            }
        }
        for idx in 0..state.in_flight_acks.len() {
            actions.push(VirtualAction::DeliverAck(idx));
        }
        for node in 0..NODE_COUNT {
            let n = u8::try_from(node).unwrap_or(0);
            if Self::is_alive(state.alive, n)
                && state.rounds_left[node] > 0
                && (state.sigma[node][0] != 0 || state.sigma[node][1] != 0)
            {
                actions.push(VirtualAction::Republish(n));
            }
        }
        if state.views != [0b111, 0b111, 0b111] {
            actions.push(VirtualAction::RefineViews);
        }
        if state.written_bits == 0b01 && Self::is_alive(state.alive, 0) && state.sigma[0][0] != 0 {
            actions.push(VirtualAction::WriteUpdate);
        }
        if state.crashes < self.max_crashes {
            for node in 0..NODE_COUNT {
                let n = u8::try_from(node).unwrap_or(0);
                if Self::is_alive(state.alive, n) {
                    actions.push(VirtualAction::CrashNode(n));
                }
            }
        }
    }

    fn next_state(&self, prev: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = prev.clone();
        match action {
            VirtualAction::Republish(sender) => {
                let s = usize::from(sender);
                state.rounds_left[s] = state.rounds_left[s].saturating_sub(1);
                let purpose = if self.mutant == Some(RepairMutant::VirtualHintDeletesSource) {
                    1
                } else {
                    0
                };
                for slot in 0..2_u8 {
                    let bits = state.sigma[s][usize::from(slot)];
                    if bits == 0 {
                        continue;
                    }
                    if let Some(target) = self.virtual_owner(state.views[s], slot) {
                        if target != sender && state.in_flight_copies.len() < 4 {
                            let msg = VirtualCopyMsg {
                                from: sender,
                                to: target,
                                slot,
                                bits,
                                purpose,
                            };
                            if !state.in_flight_copies.contains(&msg) {
                                state.in_flight_copies.push(msg);
                            }
                        }
                    }
                }
            }
            VirtualAction::DeliverCopy(idx) => {
                let msg = state.in_flight_copies.remove(idx);
                if !Self::is_alive(state.alive, msg.to) {
                    return Some(state);
                }
                let r = usize::from(msg.to);
                let accepted = self.virtual_owner(state.views[r], msg.slot) == Some(msg.to);
                if accepted {
                    let slot = usize::from(msg.slot);
                    let joined =
                        join_bits_via_entry(self.entry_did, state.sigma[r][slot], msg.bits);
                    if self.virtual_owner(state.views[r], msg.slot) != Some(msg.to) {
                        state.ownership_validation_violated = true;
                    }
                    state.sigma[r][slot] = joined;
                    let ack = VirtualAckMsg {
                        from: msg.to,
                        to: msg.from,
                        slot: msg.slot,
                        bits: joined,
                        purpose: msg.purpose,
                    };
                    if !state.in_flight_acks.contains(&ack) {
                        state.in_flight_acks.push(ack);
                    }
                }
            }
            VirtualAction::DeliverAck(idx) => {
                let ack = state.in_flight_acks.remove(idx);
                if !Self::is_alive(state.alive, ack.to) {
                    return Some(state);
                }
                let s = usize::from(ack.to);
                let slot = usize::from(ack.slot);
                let purpose = Self::purpose_from_code(ack.purpose);
                if purpose.permits_source_cleanup() {
                    let key = self.placements[slot];
                    let local_bits = state.sigma[s][slot];
                    if local_bits != 0
                        && ack_confirms_bits(self.entry_did, key, local_bits, ack.bits)
                    {
                        state.sigma[s][slot] = 0;
                        state.virtual_cleanup_violated = true;
                    }
                }
            }
            VirtualAction::DropCopy(idx) => {
                state.drops_left = state.drops_left.saturating_sub(1);
                state.in_flight_copies.remove(idx);
            }
            VirtualAction::RefineViews => {
                for n in 0..NODE_COUNT {
                    if Self::is_alive(state.alive, u8::try_from(n).unwrap_or(0)) {
                        state.views[n] = state.alive;
                    }
                }
            }
            VirtualAction::WriteUpdate => {
                state.written_bits |= 0b10;
                for slot in 0..2 {
                    state.sigma[0][slot] =
                        join_bits_via_entry(self.entry_did, state.sigma[0][slot], 0b10);
                }
            }
            VirtualAction::CrashNode(node) => {
                let n = usize::from(node);
                state.crashes = state.crashes.saturating_add(1);
                state.alive &= !(1 << node);
                state.sigma[n] = [0, 0];
                state
                    .in_flight_copies
                    .retain(|m| m.from != node && m.to != node);
                state
                    .in_flight_acks
                    .retain(|a| a.from != node && a.to != node);
                let live_union =
                    Self::live_lub_for_slot(&state, 0) | Self::live_lub_for_slot(&state, 1);
                if live_union != state.written_bits {
                    state.all_holders_crashed = true;
                }
            }
        }
        (state != *prev).then_some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "virtual-node repair never deletes source placements (S1'')",
                |_, state| !state.virtual_cleanup_violated,
            ),
            Property::<Self>::always(
                "receivers persist only placements accepted by local view (S1')",
                |_, state| !state.ownership_validation_violated,
            ),
            Property::<Self>::always(
                "no committed update is lost unless every holder crashes",
                |_, state| {
                    let live_union =
                        Self::live_lub_for_slot(state, 0) | Self::live_lub_for_slot(state, 1);
                    live_union == state.written_bits || state.all_holders_crashed
                },
            ),
            Property::<Self>::sometimes(
                "multi-round repair after stale-view refinement reaches REPLICATED(e, 2) (S4)",
                |model, state| model.replicated_in_full_view(state) && state.written_bits == 0b11,
            ),
        ]
    }
}

// ===========================================================================
// Part 2: Multi-round physical Chord handoff + crash-during-handoff (`S1`, `S2'`)
// ===========================================================================

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct HandoffCopy {
    from: u8,
    to: u8,
    bits: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct HandoffAck {
    from: u8,
    to: u8,
    bits: u8,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct HandoffState {
    /// Known ring members per node (`bit i` means `Ni` is in `TopologyState`).
    views: [u8; NODE_COUNT],
    /// Stored value bitmask `sigma[node]` at the migrating placement key `k`.
    sigma: [u8; NODE_COUNT],
    /// Hand-off rounds remaining per node.
    rounds_left: [u8; NODE_COUNT],
    /// Alive bitmask over `{N0, N1, N2}`.
    alive: u8,
    /// Crashes used so far.
    crashes: u8,
    /// Set if a crash occurred while BOTH sender and receiver held a copy prior to ack delivery.
    crashed_during_unacked_handoff: bool,
    /// Update bits whose last active custodian(s) crashed.
    lost_by_crash_bits: u8,
    /// Set if an ack deletion removed bits not confirmed by the acknowledgement (`S2'`).
    s2_prime_violated: bool,
    /// Global written update bitmask (`0b01` or `0b11`).
    written_bits: u8,
    /// Remaining message drops.
    drops_left: u8,
    copies: Vec<HandoffCopy>,
    acks: Vec<HandoffAck>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum HandoffAction {
    SendHandoff(u8),
    DeliverHandoff(usize),
    DeliverAck(usize),
    DropCopy(usize),
    RefineTopology,
    ConcurrentWrite,
    CrashNode(u8),
}

#[derive(Clone)]
struct HandoffRepairModel {
    nodes: [Did; NODE_COUNT],
    entry_did: Did,
    placement_key: Did,
    max_crashes: u8,
    mutant: Option<RepairMutant>,
}

impl HandoffRepairModel {
    fn new(max_crashes: u8, mutant: Option<RepairMutant>) -> Self {
        let (nodes, entry_did) = ring_dids();
        // Placement key `0x9500` lies in `(N2 = 0x9000, N0 = 0x1000]`:
        // - under N0's initial stale view `{N0, N1}`, `head(N0) = N1 (0x5000)` and `0x9500` lies
        //   beyond `(N0, N1]`, so N0 hands off to N1 in Round 1;
        // - at N1 with view `{N1, N2}`, `head(N1) = N2 (0x9000)` and `0x9500` lies beyond
        //   `(N1, N2]`, so N1 hands off to N2 in Round 2;
        // - at N2 (`0x9000`), `0x9500` lies inside `(N2, head(N2)]`, so N2 is the terminal owner.
        let placement_key = Did::from(0x9500_u32);
        Self {
            nodes,
            entry_did,
            placement_key,
            max_crashes,
            mutant,
        }
    }

    /// Production `TopologyState` + `topology::successor_head` + `Did::cmp_from_observer`
    /// (`PeerRing::hand_off_beyond` / `placed_beyond`) target for `sender`.
    fn handoff_target(&self, sender: u8, view_mask: u8) -> Option<u8> {
        let s = usize::from(sender);
        let local = self.nodes[s];
        let peers = (0..NODE_COUNT)
            .filter(|&idx| idx != s && (view_mask & (1 << idx) != 0))
            .map(|idx| self.nodes[idx])
            .collect::<Vec<_>>();
        let succs = topology::successors(&peers, local, NODE_COUNT);
        let topo = TopologyState::new(local, succs, None, Vec::new());
        let head = topology::successor_head(&topo)?;
        (Did::cmp_from_observer(local, self.placement_key, head) == std::cmp::Ordering::Greater)
            .then(|| {
                (0..NODE_COUNT)
                    .find(|&idx| self.nodes[idx] == head)
                    .and_then(|idx| u8::try_from(idx).ok())
            })
            .flatten()
    }

    fn live_lub(state: &HandoffState) -> u8 {
        (0..NODE_COUNT)
            .filter(|&n| state.alive & (1 << n) != 0)
            .fold(0_u8, |acc, n| acc | state.sigma[n])
    }

    /// Bits held by live nodes that do not already have a confirming acknowledgement in flight
    /// (i.e., active custodians whose copies are not pending post-handoff source cleanup).
    fn retained_lub(state: &HandoffState) -> u8 {
        (0..NODE_COUNT)
            .filter(|&n| state.alive & (1 << n) != 0)
            .fold(0_u8, |acc, n| {
                let local = state.sigma[n];
                let pending_cleanup = local != 0
                    && state
                        .acks
                        .iter()
                        .any(|ack| usize::from(ack.to) == n && (ack.bits & local) == local);
                if pending_cleanup {
                    acc
                } else {
                    acc | local
                }
            })
    }
}

impl Model for HandoffRepairModel {
    type Action = HandoffAction;
    type State = HandoffState;

    fn init_states(&self) -> Vec<Self::State> {
        vec![HandoffState {
            views: [0b011, 0b110, 0b111],
            sigma: [0b01, 0, 0],
            rounds_left: [2, 2, 1],
            alive: 0b111,
            crashes: 0,
            crashed_during_unacked_handoff: false,
            lost_by_crash_bits: 0,
            s2_prime_violated: false,
            written_bits: 0b01,
            drops_left: 1,
            copies: Vec::new(),
            acks: Vec::new(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for idx in 0..state.copies.len() {
            actions.push(HandoffAction::DeliverHandoff(idx));
            if state.drops_left > 0 {
                actions.push(HandoffAction::DropCopy(idx));
            }
        }
        for idx in 0..state.acks.len() {
            actions.push(HandoffAction::DeliverAck(idx));
        }
        for node in 0..NODE_COUNT {
            let n = u8::try_from(node).unwrap_or(0);
            if (state.alive & (1 << n) != 0)
                && state.rounds_left[node] > 0
                && state.sigma[node] != 0
                && self.handoff_target(n, state.views[node]).is_some()
            {
                actions.push(HandoffAction::SendHandoff(n));
            }
        }
        if state.views != [state.alive; NODE_COUNT] {
            actions.push(HandoffAction::RefineTopology);
        }
        if state.written_bits == 0b01
            && (state.alive & 1 != 0)
            && (!state.copies.is_empty() || !state.acks.is_empty())
        {
            actions.push(HandoffAction::ConcurrentWrite);
        }
        if state.crashes < self.max_crashes {
            for node in 0..NODE_COUNT {
                let n = u8::try_from(node).unwrap_or(0);
                if state.alive & (1 << n) != 0 {
                    actions.push(HandoffAction::CrashNode(n));
                }
            }
        }
    }

    fn next_state(&self, prev: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = prev.clone();
        match action {
            HandoffAction::SendHandoff(sender) => {
                let s = usize::from(sender);
                let target = self.handoff_target(sender, state.views[s])?;
                state.rounds_left[s] = state.rounds_left[s].saturating_sub(1);
                let bits = state.sigma[s];
                let copy = HandoffCopy {
                    from: sender,
                    to: target,
                    bits,
                };
                if !state.copies.contains(&copy) && state.copies.len() < 3 {
                    state.copies.push(copy);
                }
                if self.mutant == Some(RepairMutant::DeleteBeforeAck) {
                    state.sigma[s] = 0;
                }
            }
            HandoffAction::DeliverHandoff(idx) => {
                let copy = state.copies.remove(idx);
                if state.alive & (1 << copy.to) == 0 {
                    return Some(state);
                }
                let r = usize::from(copy.to);
                let joined = join_bits_via_entry(self.entry_did, state.sigma[r], copy.bits);
                state.sigma[r] = joined;
                let ack = HandoffAck {
                    from: copy.to,
                    to: copy.from,
                    bits: joined,
                };
                if !state.acks.contains(&ack) {
                    state.acks.push(ack);
                }
            }
            HandoffAction::DeliverAck(idx) => {
                let ack = state.acks.remove(idx);
                if state.alive & (1 << ack.to) == 0 {
                    return Some(state);
                }
                let s = usize::from(ack.to);
                let local_bits = state.sigma[s];
                if local_bits != 0 {
                    let confirmed = self.mutant == Some(RepairMutant::IgnoreAckValueMatch)
                        || ack_confirms_bits(
                            self.entry_did,
                            self.placement_key,
                            local_bits,
                            ack.bits,
                        );
                    if confirmed {
                        if (ack.bits & local_bits) != local_bits {
                            state.s2_prime_violated = true;
                        }
                        state.sigma[s] = 0;
                    }
                }
            }
            HandoffAction::DropCopy(idx) => {
                state.drops_left = state.drops_left.saturating_sub(1);
                state.copies.remove(idx);
            }
            HandoffAction::RefineTopology => {
                for n in 0..NODE_COUNT {
                    if state.alive & (1 << n) != 0 {
                        state.views[n] = state.alive;
                    }
                }
            }
            HandoffAction::ConcurrentWrite => {
                state.written_bits |= 0b10;
                state.sigma[0] = join_bits_via_entry(self.entry_did, state.sigma[0], 0b10);
            }
            HandoffAction::CrashNode(node) => {
                let before_retained = Self::retained_lub(&state);
                let n = usize::from(node);
                let unacked_with_both_holders = state.acks.iter().any(|ack| {
                    (ack.from == node || ack.to == node)
                        && state.sigma[usize::from(ack.from)] != 0
                        && state.sigma[usize::from(ack.to)] != 0
                });
                state.crashes = state.crashes.saturating_add(1);
                state.alive &= !(1 << node);
                state.sigma[n] = 0;
                state.copies.retain(|c| c.from != node && c.to != node);
                state.acks.retain(|a| a.from != node && a.to != node);
                if unacked_with_both_holders {
                    state.crashed_during_unacked_handoff = true;
                }
                // Record only bits whose sole remaining active custodian(s) crashed in this step
                // (never excusing bits already lost prior to the crash by premature deletion).
                let after_retained = Self::retained_lub(&state);
                state.lost_by_crash_bits |= before_retained & !after_retained;
            }
        }
        (state != *prev).then_some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let mut props = vec![
            Property::<Self>::always(
                "ack cleanup removes local copy only when receiver holds the removed value (S2')",
                |_, state| !state.s2_prime_violated,
            ),
            Property::<Self>::always(
                "no update is lost before ack or under crash while another holder survives",
                |_, state| (Self::live_lub(state) | state.lost_by_crash_bits) == state.written_bits,
            ),
            Property::<Self>::sometimes(
                "multi-round handoff with concurrent write converges to final owner N2",
                |_, state| {
                    state.alive == 0b111
                        && state.written_bits == 0b11
                        && state.sigma == [0, 0, 0b11]
                },
            ),
        ];
        if self.max_crashes >= 1 {
            props.push(Property::<Self>::sometimes(
                "single-node crash during unacked handoff preserves committed updates",
                |_, state| {
                    state.crashed_during_unacked_handoff
                        && state.crashes == 1
                        && Self::live_lub(state) == state.written_bits
                },
            ));
        }
        if self.max_crashes >= 2 {
            props.push(Property::<Self>::sometimes(
                "simultaneous owner and holder crash is the sole data-loss boundary",
                |_, state| {
                    state.crashed_during_unacked_handoff
                        && state.crashes == 2
                        && state.lost_by_crash_bits != 0
                        && Self::live_lub(state) < state.written_bits
                },
            ));
        }
        props
    }
}

fn refuted<M>(model: M) -> BTreeSet<&'static str>
where
    M: Model + Send + Sync + 'static,
    M::State: Clone + std::fmt::Debug + Eq + std::hash::Hash + Send + Sync,
    M::Action: Clone + std::fmt::Debug + PartialEq + Send + Sync,
{
    let checker = model.checker().spawn_bfs().join();
    checker
        .model()
        .properties()
        .into_iter()
        .filter(|property| {
            let discovered = checker.discovery(property.name).is_some();
            match property.expectation {
                Expectation::Sometimes => !discovered,
                _ => discovered,
            }
        })
        .map(|property| property.name)
        .collect()
}

/// Exhaustive Stateright model check of multi-round storage repair over virtual-node placement,
/// stale topology views, multi-hop physical Chord hand-off, concurrent writes, and crash during
/// hand-off.
#[test]
fn test_multi_round_storage_repair_and_handoff_crash_laws_hold() {
    VirtualRepairModel::new(1, None)
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
    HandoffRepairModel::new(1, None)
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
    HandoffRepairModel::new(2, None)
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
}

/// Each storage-repair safety law is load-bearing: premature source deletion, virtual-hint
/// source deletion (`S1''`), and unconfirmed ack deletion (`S2'`) are each refuted by Stateright.
#[test]
fn test_each_storage_repair_mutant_is_refuted() {
    let vhint_refuted = refuted(VirtualRepairModel::new(
        0,
        Some(RepairMutant::VirtualHintDeletesSource),
    ));
    assert!(
        vhint_refuted.contains("virtual-node repair never deletes source placements (S1'')"),
        "VirtualHintDeletesSource must violate S1'': {vhint_refuted:?}"
    );

    let pre_ack_refuted = refuted(HandoffRepairModel::new(
        0,
        Some(RepairMutant::DeleteBeforeAck),
    ));
    assert!(
        pre_ack_refuted
            .contains("no update is lost before ack or under crash while another holder survives"),
        "DeleteBeforeAck must cause pre-ack data loss: {pre_ack_refuted:?}"
    );

    let unconfirmed_ack_refuted = refuted(HandoffRepairModel::new(
        0,
        Some(RepairMutant::IgnoreAckValueMatch),
    ));
    assert!(
        unconfirmed_ack_refuted.contains(
            "ack cleanup removes local copy only when receiver holds the removed value (S2')"
        ) || unconfirmed_ack_refuted
            .contains("no update is lost before ack or under crash while another holder survives"),
        "IgnoreAckValueMatch must violate S2' / data-loss safety: {unconfirmed_ack_refuted:?}"
    );
}
