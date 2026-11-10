use std::collections::BTreeSet;
use std::hash::Hash;
use std::hash::Hasher;

use bytes::Bytes;

use crate::algebra::JoinSemilattice;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryCrdt;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryTombstone;
use crate::dht::entry::EntryVersion;
use crate::dht::Did;

// ===================================================================
// Stage 3: storage CRDT SEC topology model.
//
// State variables:
//   phase    in {Partitioned, Merged}
//   replica  in StorageJoinValue^3
//
// Initial state:
//   replicas start with independent local writes A, B, C.
//   phase = Partitioned, where only same-side nodes may exchange state.
//
// Next-state relation:
//   Transfer(from, to) applies replica[to] := replica[to] join replica[from]
//   whenever the topology allows that edge.
//   Merge changes phase from Partitioned to Merged, enabling every edge.
//
// Carrier safety:
//   `test_storage_entry_join_satisfies_semilattice_laws` proves the real Entry
//   carriers are join-semilattices over this finite domain. This topology
//   model therefore does not duplicate the carrier <= LUB invariant; its
//   distinct obligation is the liveness/closure step below.
//
// Liveness expectation under fair anti-entropy:
//   from any reachable Merged state, repeated Transfer steps reach the single
//   least upper bound at every replica.
//
// Refinement:
//   An asynchronous send/deliver trace projects to this Transfer model because
//   delivery is a pure join of a sender snapshot into the receiver. Message
//   reordering and duplication are covered by the semilattice law checked
//   below: join is commutative and idempotent.
//
// Quotient:
//   `StorageJoinValue` hashes and compares only `(carrier, bits)` so BFS stays
//   finite. The test `test_storage_entry_join_satisfies_semilattice_laws` is the
//   refinement witness: for every finite carrier state, real Entry::join equals
//   canonical(bits_a union bits_b). The topology model is therefore checked on
//   the quotient, while carrier correctness is checked on the real entries.
// ===================================================================

pub(super) const STORAGE_REPLICA_COUNT: usize = 3;
/// Retention bound shared by every model value; the model checks the payload lattice, and a
/// common bound keeps the retention component of every join equal.
const MODEL_RETENTION_BOUND_MS: Option<u128> = Some(u128::MAX);
pub(super) const STORAGE_PARTITION_MASKS: [StoragePartition; STORAGE_REPLICA_COUNT] = [
    StoragePartition(0b001),
    StoragePartition(0b010),
    StoragePartition(0b011),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(super) struct StoragePartition(u8);

impl StoragePartition {
    fn permits(self, from: usize, to: usize) -> bool {
        if from == to {
            return false;
        }
        self.side(from) == self.side(to)
    }

    fn side(self, node: usize) -> bool {
        let shift = match u32::try_from(node) {
            Ok(shift) => shift,
            Err(_) => return false,
        };
        let Some(bit) = 1u8.checked_shl(shift) else {
            return false;
        };
        self.0 & bit != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(super) enum StorageJoinCarrier {
    DataBoundedTopN,
    DataFairMultiWriter,
    DataOverwriteReset,
    RelayTombstone,
}

#[derive(Clone, Debug)]
pub(super) struct StorageJoinValue {
    carrier: StorageJoinCarrier,
    pub(super) bits: u8,
    pub(super) entry: Entry,
}

impl StorageJoinValue {
    fn new(carrier: StorageJoinCarrier, bits: u8, entry: Entry) -> Self {
        match entry.try_into_storage_entry() {
            Ok(entry) => Self {
                carrier,
                bits,
                entry,
            },
            Err(error) => panic!("storage model entry must normalize: {error}"),
        }
    }

    fn bottom_like(&self) -> Self {
        storage_value_from_bits(self.carrier, 0)
    }
}

impl PartialEq for StorageJoinValue {
    fn eq(&self, other: &Self) -> bool {
        self.carrier == other.carrier && self.bits == other.bits
    }
}

impl Eq for StorageJoinValue {}

impl Hash for StorageJoinValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.carrier.hash(state);
        self.bits.hash(state);
    }
}

impl Ord for StorageJoinValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.carrier
            .cmp(&other.carrier)
            .then_with(|| self.bits.cmp(&other.bits))
    }
}

impl PartialOrd for StorageJoinValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl JoinSemilattice for StorageJoinValue {
    fn join(self, other: Self) -> Self {
        if self.carrier != other.carrier {
            panic!("storage model joins only one carrier");
        }
        let carrier = self.carrier;
        let bits = self.bits | other.bits;
        let joined = storage_join_entry(self.entry, other.entry);
        Self::new(carrier, bits, joined)
    }
}

pub(super) struct StorageJoinScenario {
    pub(super) name: &'static str,
    pub(super) initial: [StorageJoinValue; STORAGE_REPLICA_COUNT],
}

impl StorageJoinScenario {
    fn bottom(&self) -> StorageJoinValue {
        self.initial[0].bottom_like()
    }

    pub(super) fn global_lub(&self) -> StorageJoinValue {
        self.initial
            .iter()
            .cloned()
            .fold(self.bottom(), JoinSemilattice::join)
    }
}

fn storage_model_did(offset: u32) -> Did {
    Did::from(10_000u32.saturating_add(offset))
}

fn storage_version(time: u128, actor: u32, operation: u32) -> EntryVersion {
    EntryVersion::new(time, Did::from(actor), Did::from(operation))
}

fn storage_index(index: usize) -> u32 {
    match u32::try_from(index) {
        Ok(index) => index,
        Err(_) => panic!("storage model index must fit in u32"),
    }
}

fn storage_dot(version: EntryVersion, index: usize) -> EntryDot {
    let index = storage_index(index);
    EntryDot { version, index }
}

/// The element whose bytes are `label`'s: elements are stored as their bytes (#926).
pub(super) fn storage_element(label: &str) -> Bytes {
    Bytes::copy_from_slice(label.as_bytes())
}

pub(super) fn storage_join_entry(left: Entry, right: Entry) -> Entry {
    let joined = match left.join(right) {
        Ok(entry) => entry,
        Err(error) => panic!("storage model joins only compatible entries: {error}"),
    };
    match joined.try_into_storage_entry() {
        Ok(entry) => entry,
        Err(error) => panic!("storage model join result must normalize: {error}"),
    }
}

fn data_actor_value_range(
    did: Did,
    actor: u32,
    label: &'static str,
    start_time: u128,
    count: usize,
) -> Entry {
    let data = (0..count)
        .map(|index| storage_element(&format!("{label}-{index}")))
        .collect::<Vec<_>>();
    let dots = (0..count)
        .map(|offset| {
            let index = storage_index(offset);
            let time = start_time.saturating_add(u128::from(index));
            storage_dot(
                storage_version(time, actor, 1_000u32.saturating_add(index)),
                offset,
            )
        })
        .collect::<Vec<_>>();
    Entry {
        did,
        data,
        kind: EntryKind::Data,
        crdt: EntryCrdt {
            register: None,
            dots,
            co_dots: Vec::new(),
            co_registers: Vec::new(),
            proofs: Vec::new(),
            tombstones: Vec::new(),
        },
        expires_at_ms: MODEL_RETENTION_BOUND_MS,
    }
}

fn data_value_range(did: Did, label: &'static str, start_time: u128, count: usize) -> Entry {
    data_actor_value_range(did, 1, label, start_time, count)
}

fn data_overwrite_value(did: Did, label: &'static str, version: EntryVersion) -> Entry {
    Entry {
        did,
        data: vec![storage_element(label)],
        kind: EntryKind::Data,
        crdt: EntryCrdt {
            register: Some(version),
            dots: vec![storage_dot(version, 0)],
            co_dots: Vec::new(),
            co_registers: Vec::new(),
            proofs: Vec::new(),
            tombstones: Vec::new(),
        },
        expires_at_ms: MODEL_RETENTION_BOUND_MS,
    }
}

fn relay_add_value(did: Did, label: &'static str, dot: EntryDot) -> Entry {
    Entry {
        did,
        data: vec![storage_element(label)],
        kind: EntryKind::RelayMessage,
        crdt: EntryCrdt {
            register: None,
            dots: vec![dot],
            co_dots: Vec::new(),
            co_registers: Vec::new(),
            proofs: Vec::new(),
            tombstones: Vec::new(),
        },
        expires_at_ms: MODEL_RETENTION_BOUND_MS,
    }
}

/// A relay-inbox removal of the held message `label`, covering its dots up to `dot`.
fn relay_remove_value(did: Did, label: &'static str, dot: EntryDot) -> Entry {
    Entry {
        did,
        data: Vec::new(),
        kind: EntryKind::RelayMessage,
        crdt: EntryCrdt {
            register: None,
            dots: Vec::new(),
            co_dots: Vec::new(),
            co_registers: Vec::new(),
            proofs: Vec::new(),
            tombstones: vec![EntryTombstone::of(&storage_element(label), dot)],
        },
        expires_at_ms: MODEL_RETENTION_BOUND_MS,
    }
}

fn storage_delta_entry(carrier: StorageJoinCarrier, bit: u8) -> Entry {
    match carrier {
        StorageJoinCarrier::DataBoundedTopN => data_value_range(
            storage_model_did(1),
            match bit {
                0b001 => "low",
                0b010 => "mid",
                0b100 => "high",
                _ => panic!("storage model delta bit must be singleton"),
            },
            match bit {
                0b001 => 1,
                0b010 => 1_000,
                0b100 => 2_000,
                _ => panic!("storage model delta bit must be singleton"),
            },
            ENTRY_DATA_MAX_LEN,
        ),
        StorageJoinCarrier::DataFairMultiWriter => match bit {
            0b001 => data_actor_value_range(
                storage_model_did(4),
                1,
                "flood-a1",
                2_000,
                ENTRY_DATA_MAX_LEN,
            ),
            0b010 => {
                let mut entry =
                    data_actor_value_range(storage_model_did(4), 2, "honest-a2", 100, 2);
                if let Some(first) = entry.data.first_mut() {
                    *first = storage_element("flood-a1-0");
                }
                entry
            }
            0b100 => data_actor_value_range(storage_model_did(4), 3, "honest-a3", 500, 2),
            _ => panic!("storage model delta bit must be singleton"),
        },
        StorageJoinCarrier::DataOverwriteReset => match bit {
            0b001 => data_value_range(storage_model_did(2), "stale-a", 1, 3),
            0b010 => {
                data_overwrite_value(storage_model_did(2), "reset", storage_version(100, 2, 200))
            }
            0b100 => data_value_range(storage_model_did(2), "stale-c", 10, 3),
            _ => panic!("storage model delta bit must be singleton"),
        },
        StorageJoinCarrier::RelayTombstone => {
            let relay_a_dot = storage_dot(storage_version(1, 1, 10), 0);
            let relay_b_dot = storage_dot(storage_version(2, 2, 20), 0);
            match bit {
                0b001 => relay_add_value(storage_model_did(3), "relay-a", relay_a_dot),
                0b010 => relay_add_value(storage_model_did(3), "relay-b", relay_b_dot),
                0b100 => relay_remove_value(storage_model_did(3), "relay-a", relay_a_dot),
                _ => panic!("storage model delta bit must be singleton"),
            }
        }
    }
}

fn storage_bottom_entry(carrier: StorageJoinCarrier) -> Entry {
    let (did, kind) = match carrier {
        StorageJoinCarrier::DataBoundedTopN => (storage_model_did(1), EntryKind::Data),
        StorageJoinCarrier::DataFairMultiWriter => (storage_model_did(4), EntryKind::Data),
        StorageJoinCarrier::DataOverwriteReset => (storage_model_did(2), EntryKind::Data),
        StorageJoinCarrier::RelayTombstone => (storage_model_did(3), EntryKind::RelayMessage),
    };
    Entry::new(did, Vec::new(), kind)
}

pub(super) fn storage_value_from_bits(carrier: StorageJoinCarrier, bits: u8) -> StorageJoinValue {
    let mut entry = storage_bottom_entry(carrier);
    for bit in [0b001, 0b010, 0b100] {
        if bits & bit != 0 {
            entry = storage_join_entry(entry, storage_delta_entry(carrier, bit));
        }
    }
    StorageJoinValue::new(carrier, bits, entry)
}

pub(super) fn storage_join_scenarios() -> Vec<StorageJoinScenario> {
    vec![
        StorageJoinScenario {
            name: "data bounded top-n",
            initial: [
                storage_value_from_bits(StorageJoinCarrier::DataBoundedTopN, 0b001),
                storage_value_from_bits(StorageJoinCarrier::DataBoundedTopN, 0b010),
                storage_value_from_bits(StorageJoinCarrier::DataBoundedTopN, 0b100),
            ],
        },
        StorageJoinScenario {
            name: "data fair multi-writer water-filling cap",
            initial: [
                storage_value_from_bits(StorageJoinCarrier::DataFairMultiWriter, 0b001),
                storage_value_from_bits(StorageJoinCarrier::DataFairMultiWriter, 0b010),
                storage_value_from_bits(StorageJoinCarrier::DataFairMultiWriter, 0b100),
            ],
        },
        StorageJoinScenario {
            name: "data overwrite reset floor",
            initial: [
                storage_value_from_bits(StorageJoinCarrier::DataOverwriteReset, 0b001),
                storage_value_from_bits(StorageJoinCarrier::DataOverwriteReset, 0b010),
                storage_value_from_bits(StorageJoinCarrier::DataOverwriteReset, 0b100),
            ],
        },
        StorageJoinScenario {
            name: "relay tombstone prevents resurrection",
            initial: [
                storage_value_from_bits(StorageJoinCarrier::RelayTombstone, 0b001),
                storage_value_from_bits(StorageJoinCarrier::RelayTombstone, 0b010),
                storage_value_from_bits(StorageJoinCarrier::RelayTombstone, 0b100),
            ],
        },
    ]
}

pub(super) fn storage_join_carriers() -> [StorageJoinCarrier; 4] {
    [
        StorageJoinCarrier::DataBoundedTopN,
        StorageJoinCarrier::DataFairMultiWriter,
        StorageJoinCarrier::DataOverwriteReset,
        StorageJoinCarrier::RelayTombstone,
    ]
}

pub(super) fn storage_value_by_bits(values: &[StorageJoinValue], bits: u8) -> &StorageJoinValue {
    match values.get(usize::from(bits)) {
        Some(value) => value,
        None => panic!("storage model bitmask must be in the finite carrier"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(super) enum StorageJoinPhase {
    Partitioned,
    Merged,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(super) struct StorageJoinState {
    partition: StoragePartition,
    pub(super) phase: StorageJoinPhase,
    replicas: [StorageJoinValue; STORAGE_REPLICA_COUNT],
}

impl StorageJoinState {
    fn initial(
        partition: StoragePartition,
        replicas: [StorageJoinValue; STORAGE_REPLICA_COUNT],
    ) -> Self {
        Self {
            partition,
            phase: StorageJoinPhase::Partitioned,
            replicas,
        }
    }

    fn topology_permits(&self, from: usize, to: usize) -> bool {
        if from == to {
            return false;
        }
        match self.phase {
            StorageJoinPhase::Partitioned => self.partition.permits(from, to),
            StorageJoinPhase::Merged => from < STORAGE_REPLICA_COUNT && to < STORAGE_REPLICA_COUNT,
        }
    }

    fn transfer_current(&self, from: usize, to: usize) -> Option<Self> {
        if !self.topology_permits(from, to) {
            return None;
        }
        let value = self.replicas.get(from).cloned()?;
        let mut next = self.clone();
        let replica = next.replicas.get_mut(to)?;
        *replica = replica.clone().join(value);
        Some(next)
    }

    fn merge_partition(&self) -> Option<Self> {
        if self.phase == StorageJoinPhase::Merged {
            return None;
        }
        Some(Self {
            phase: StorageJoinPhase::Merged,
            ..self.clone()
        })
    }

    fn successors(&self) -> Vec<Self> {
        let mut next = Vec::new();
        if let Some(merged) = self.merge_partition() {
            next.push(merged);
        }
        for from in 0..STORAGE_REPLICA_COUNT {
            for to in 0..STORAGE_REPLICA_COUNT {
                if let Some(transferred) = self.transfer_current(from, to) {
                    next.push(transferred);
                }
            }
        }
        next
    }

    pub(super) fn is_quiescent_lub(&self, global_lub: &StorageJoinValue) -> bool {
        self.replicas.iter().all(|value| value == global_lub)
    }

    fn transfer_all_current(&self) -> Self {
        let mut state = self.clone();
        for from in 0..STORAGE_REPLICA_COUNT {
            for to in 0..STORAGE_REPLICA_COUNT {
                if let Some(next) = state.transfer_current(from, to) {
                    state = next;
                }
            }
        }
        state
    }

    pub(super) fn drive_to_quiescent_lub(&self, global_lub: &StorageJoinValue) -> Self {
        let mut state = self.clone();
        for _ in 0..STORAGE_REPLICA_COUNT * 8 {
            if state.is_quiescent_lub(global_lub) {
                return state;
            }
            let next = state.transfer_all_current();
            if next == state {
                return next;
            }
            state = next;
        }
        state
    }
}

pub(super) fn reachable_storage_join_states(
    partition: StoragePartition,
    replicas: [StorageJoinValue; STORAGE_REPLICA_COUNT],
) -> BTreeSet<StorageJoinState> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![StorageJoinState::initial(partition, replicas)];
    while let Some(state) = frontier.pop() {
        if !seen.insert(state.clone()) {
            continue;
        }
        for next in state.successors() {
            if !seen.contains(&next) {
                frontier.push(next);
            }
        }
    }
    seen
}

// ===================================================================
// Stage 4: storage hand-off cleanup safety and ABA custody conservation
// for one placement key (#614, #1207).
//
// SCOPE: this is the #614 S2' / #1207 ABA cleanup model, not the storage
// convergence theorem. Convergence is Stage 3's join-semilattice fact. This
// stage abstracts one placement key across two concurrent hand-off slots
// (modeling duplicate/retried hand-off transactions T1, T2), arbitrary local
// writes, and a return hand-off from successor back to local over a finite
// representative value domain:
//
//   local(v, g) --SendCopy(slot)--> copy_in_flight[slot](v, g), handoff_observed = true
//   copy_in_flight[slot](v, g) --DeliverCopy(slot)--> successor(v) + ack_in_flight[slot](v, g)
//   local(_, g) --LocalWrite(w)--> local(w, g' = g + if handoff_observed { 1 } else { 0 })
//   successor(v), local = None --ReturnCustodyToLocal--> local(v, g), successor = None
//   ack_in_flight[slot](v, g_ack) --DeliverAckDelete(slot)-->
//       delete local and advance g := g + 1 iff local == Some(v) && g == g_ack
//
// Properties checked below:
//
//   Always S2': local(k) is removed only if successor(k) >= removed at the
//   moment of removal (successor holds at least the removed join state).
//   Always Custody Conservation (#1207): local.is_some() || successor.is_some()
//   in every reachable state (a delayed duplicate ack after return hand-off
//   never eliminates every live copy).
// ===================================================================

const MAX_MODEL_INCARNATION: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum StorageSyncStep {
    SendCopy(usize),
    DeliverCopy(usize),
    LocalWrite(StorageValue),
    ReturnCustodyToLocal,
    DeliverAckDelete(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum StorageValue {
    V0,
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct StorageSyncState {
    pub(super) local: Option<StorageValue>,
    pub(super) local_incarnation: u8,
    pub(super) handoff_observed: bool,
    pub(super) successor: Option<StorageValue>,
    pub(super) copy_in_flight: [Option<(StorageValue, u8)>; 2],
    pub(super) ack_in_flight: [Option<(StorageValue, u8)>; 2],
}

impl StorageSyncState {
    pub(super) fn initial() -> Self {
        Self {
            local: Some(StorageValue::V0),
            local_incarnation: 0,
            handoff_observed: false,
            successor: None,
            copy_in_flight: [None, None],
            ack_in_flight: [None, None],
        }
    }

    fn has_in_flight(&self) -> bool {
        self.copy_in_flight.iter().any(Option::is_some)
            || self.ack_in_flight.iter().any(Option::is_some)
    }

    pub(super) fn step(self, step: StorageSyncStep) -> Option<Self> {
        match step {
            StorageSyncStep::SendCopy(slot) if slot < 2 => {
                if self.copy_in_flight[slot].is_some()
                    || self.ack_in_flight[slot].is_some()
                    || self.local_incarnation >= MAX_MODEL_INCARNATION
                {
                    return None;
                }
                let value = self.local?;
                let mut copy_in_flight = self.copy_in_flight;
                copy_in_flight[slot] = Some((value, self.local_incarnation));
                Some(Self {
                    handoff_observed: true,
                    copy_in_flight,
                    ..self
                })
            }
            StorageSyncStep::DeliverCopy(slot) if slot < 2 => {
                let (copied, inc) = self.copy_in_flight[slot]?;
                let mut copy_in_flight = self.copy_in_flight;
                let mut ack_in_flight = self.ack_in_flight;
                copy_in_flight[slot] = None;
                ack_in_flight[slot] = Some((copied, inc));
                let successor = Some(
                    self.successor
                        .map_or(copied, |existing| existing.max(copied)),
                );
                Some(Self {
                    successor,
                    copy_in_flight,
                    ack_in_flight,
                    ..self
                })
            }
            StorageSyncStep::LocalWrite(value)
                if self.has_in_flight() && self.local_incarnation < MAX_MODEL_INCARNATION =>
            {
                let (local_incarnation, handoff_observed) = if self.handoff_observed {
                    (self.local_incarnation.saturating_add(1), false)
                } else {
                    (self.local_incarnation, false)
                };
                Some(Self {
                    local: Some(value),
                    local_incarnation,
                    handoff_observed,
                    ..self
                })
            }
            StorageSyncStep::ReturnCustodyToLocal
                if self.local.is_none() && self.local_incarnation < MAX_MODEL_INCARNATION =>
            {
                let returned = self.successor?;
                let (local_incarnation, handoff_observed) = if self.handoff_observed {
                    (self.local_incarnation.saturating_add(1), false)
                } else {
                    (self.local_incarnation, false)
                };
                Some(Self {
                    local: Some(returned),
                    local_incarnation,
                    handoff_observed,
                    successor: None,
                    ..self
                })
            }
            StorageSyncStep::DeliverAckDelete(slot) if slot < 2 => {
                let (acked, ack_inc) = self.ack_in_flight[slot]?;
                let mut ack_in_flight = self.ack_in_flight;
                ack_in_flight[slot] = None;
                let (local, local_incarnation, handoff_observed) = match self.local {
                    Some(current) if current == acked && self.local_incarnation == ack_inc => {
                        (None, self.local_incarnation.saturating_add(1), false)
                    }
                    current => (current, self.local_incarnation, self.handoff_observed),
                };
                Some(Self {
                    local,
                    local_incarnation,
                    handoff_observed,
                    ack_in_flight,
                    ..self
                })
            }
            _ => None,
        }
    }

    pub(super) fn removed_local_value(self, next: Self) -> Option<StorageValue> {
        match (self.local, next.local) {
            (Some(removed), None) => Some(removed),
            _ => None,
        }
    }
}
