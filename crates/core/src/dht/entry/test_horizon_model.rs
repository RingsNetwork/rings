//! Seeded model check of data-topic removal under the element horizon (#867, #872, #874).
//!
//! The carriers are production [`Entry`] values, changed only by the production operations
//! (`Entry::operate` on stamped `Extend`, `Overwrite` and `Tombstone` operations),
//! `Entry::join`, the horizon projection `Entry::retired_under`, liveness
//! `Entry::is_live_under`, and the hand-off acknowledgement `SyncedEntryAck::confirms_local_value`,
//! composed the way the storage funnels compose them: a read projects the stored value at the
//! reader's clock and retires a carrier that is no longer live, a write stores the projected
//! result. The retention law is a parameter, its horizons and what holds a carrier past its
//! bound: the production law ([`ElementRetention::of`], [`Entry::is_live_under`]), or a
//! deliberately broken one, for which a test witnesses that the named law fails; every law,
//! Bounded included, has such a mutant. The model owns
//! only the world: which replica acts, when a carrier is delivered, and how far real time
//! advances. Every walk is a fixed-seed random interleaving, so a failure replays exactly.
//!
//! # Specification (TLA+ style)
//!
//! ```text
//! CONSTANTS  Owners = {0, 1, 2}, JoinCache = 3, ReplaceCache = 4, Replicas = 0..4
//!            Values  = {a, b, c}
//!            Offsets = {0, σ/4, σ/2, 3σ/4, σ}                   \* pairwise ≤ σ
//!            H       = EntryKind::Data.element_horizon_ms(),  σ = TS_OFFSET_TOLERANCE_MS
//!            Law     = the retention law under test
//!
//! VARIABLES  real     : ℕ                               \* clock(r) = real + skew[r]
//!            skew     : Replica → Offsets                \* a clock may step back by ≤ σ
//!            peak     : Replica → ℕ                      \* history: max clock(r) so far
//!            carrier  : Replica → Entry ∪ {⊥}            (production values)
//!            received : Replica → SUBSET Add              \* history: adds delivered to r
//!            killed   : Replica → SUBSET (Value × Dot)    \* history: removes r observed
//!            floor    : Replica → Version ∪ {⊥}           \* history: registers r observed
//!
//! read(r)       ≜ LET x ≜ retire_{clock(r)}(carrier[r]) IN IF live(x, clock(r)) THEN x ELSE ⊥
//! Add(o, v)     ≜ carrier'[o] = retire(read(o).operate(Extend(v) stamped at clock(o)))
//! Overwrite(o, V) ≜ carrier'[o] = retire(read(o).operate(Overwrite(V)))
//!                 ∧ floor'[o] = max(floor[o], register of the write)
//! Remove(o, p, v, w) ≜ v ∈ read(o) ∧ carrier'[p] = retire(read(p).operate(Tombstone(v)))
//!                 \* witness w ∈ {value, dot of v in read(o)}; p removes the dot it holds
//!                 ∧ killed'[p] = killed[p] ∪ {(v, dot p removed)}
//! Sync(s, r)    ≜ admissible(read(s), clock(r))
//!                 ∧ carrier'[r] = retire_{clock(r)}(read(r) ⊔ read(s))   \* ReplaceCache: read(s)
//!                 ∧ received'[r] = received[r] ∪ adds(read(s))         \* ReplaceCache: of s
//!                 ∧ killed'[r] = killed[r] ∪ killed[s] ∧ floor'[r] = max(floor[r], floor[s])
//! HandoffCopy(s, r) ≜ Sync(s, r) ∧ pending' = pending ∪ {(s, read(s))}
//! HandoffAck(s)  ≜ (s, c) ∈ pending ∧ pending' = pending ∖ {(s, c)}
//!                  ∧ (confirms(c, read(s), clock(s)) ⇒ carrier'[s] = ⊥
//!                                ∧ received'[s] = killed'[s] = ∅ ∧ floor'[s] = ⊥)
//! Tick(Δ)       ≜ real' = real + Δ,  Δ ∈ {1, σ/2, σ, H/3, H − σ, H + σ}
//! Drift(r, o)   ≜ skew'[r] = o,  o ∈ Offsets                 \* NTP steps within tolerance
//!
//! covered_r(v, d) ≜ (∃(v, k) ∈ killed[r]. d ≤ k) ∨ d.version < floor[r]
//!
//! Safety (□), at every replica r with x = read(r):
//!   NoLoss         ≜ ∀a ∈ received[r]. peak[r] < min(τ(a) + H, expires(a)) ∧ ¬covered_r(a)
//!                                        ⇒ ∃d ≥ dot(a). (value(a), d) ∈ x
//!   NoResurrection ≜ ∀(v, d) ∈ x. ¬covered_r(v, d)
//!   Bounded        ≜ |x.data| ≤ max_data_len
//!                    ∧ ∀(v, d) ∈ x. clock(r) < τ(d) + H
//!                    ∧ ∀(e, k) ∈ x.removes. clock(r) < τ(k) + H + σ
//!                    ∧ |x.removes| ≤ |{removes issued anywhere with clock(r) < τ(k) + H + σ}|
//!   Homomorphism   ≜ ∀x, y ∈ carrier, t ∈ clocks. retire_t(x ⊔ y) = retire_t(x) ⊔ retire_t(y)
//! Liveness, after the walk: two full exchange rounds at frozen clocks make every read, projected
//! at the greatest clock, show the same visible set (Convergence).
//! ```
//!
//! `NoLoss` is judged at the replica's peak clock: an add a replica retired on an earlier,
//! later-reading clock stays retired when its clock steps back. `NoResurrection` is
//! unconditional, which is where `σ` earns its place: every read and write projects at the
//! local clock, so on a monotone clock a replica that collects a remove has retired every add
//! it covered; but a clock may step back by up to `σ`, and a remove collected at `τ(k) + H`
//! would let a stale add of the same payload arrive, admitted and visible, on the stepped-back
//! clock. Collected at `τ(k) + H + σ`, every clock anywhere, stepped back or not, reads at
//! least `τ(k) + H`, the no-resurrection law of the `retention` module. The same law is what
//! requires a data carrier holding an unstable remove or register to stay live past its
//! retention bound (`Entry::is_live_at`).
//!
//! A hand-off is two steps, the copy and a later acknowledgement, so writes, ticks and drifts
//! interleave between them and the ack gate is exercised; a confirmed hand-off gives up the
//! sender's slot, so the sender's history restarts with it. Reader
//! caches of both kinds are modelled: one joins every reply it observes (the read-join #864
//! adds), one replaces its value with the last reply (`local_cache_put`), and
//! both are delivered back to owners, as read-repair of a missed placement does from the held
//! cache read (`PeerRing::local_cache_held`), which keeps a carrier past its bound.
//!
//! Not modelled, each stated where it matters: storage byte-budget eviction (it drops a carrier
//! whatever it holds; SECURITY.md), forged deltas (admission bounds them to `τ ≤ now + σ`, the
//! skew the model already spans), and a binding `max_data_len` cap (three values never reach
//! it; the cap commutes with `retire_t`, see the `retention` module). The #874 covering remove
//! is structural rather than a law parameter; `test_remove_covers_superseded_dot_of_same_payload`
//! is its directed witness.

use std::collections::BTreeSet;

use bytes::Bytes;

use super::retention::ElementRetention;
use super::Entry;
use super::EntryDot;
use super::EntryKind;
use super::EntryOperation;
use super::EntryVersion;
use super::SyncedEntryAck;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::tests::splitmix64;

/// The model's topic.
const MODEL_TOPIC: &str = "element horizon model";

/// The payloads owners write.
const MODEL_VALUES: [&str; 3] = ["a", "b", "c"];

/// The replicas that write: storage owners.
const OWNERS: usize = 3;

/// The reader cache that replaces its value with the last reply; replica `OWNERS` is the
/// reader cache that joins every reply.
const REPLACE_CACHE: usize = 4;

/// Every replica: the owners, then the two reader caches.
const REPLICAS: usize = 5;

/// The fixed seeds, one walk each: the production law is walked on all of them, and a broken
/// law is searched over the same ones for a counterexample.
const SEEDS: u64 = 512;

/// The steps of one walk.
const STEPS: usize = 200;

/// Real time at the start of every walk.
const MODEL_EPOCH_MS: u128 = 1_700_000_000_000;

/// The clock offsets from real time a replica may take; they differ pairwise by at most `σ`.
const OFFSETS_MS: [u128; 5] = [
    0,
    TS_OFFSET_TOLERANCE_MS / 4,
    TS_OFFSET_TOLERANCE_MS / 2,
    TS_OFFSET_TOLERANCE_MS * 3 / 4,
    TS_OFFSET_TOLERANCE_MS,
];

/// The production data retention law.
fn production_law() -> Result<ElementRetention> {
    ElementRetention::of(EntryKind::Data)
        .ok_or_else(|| Error::InvalidMessage("a data topic has an element horizon".to_string()))
}

/// The storage node that acts for replica `replica`.
fn actor(replica: usize) -> Did {
    Did::from(u32::try_from(replica).unwrap_or(u32::MAX).saturating_add(1))
}

/// The real-time advances a `Tick` draws from: on either side of `σ`, `H`, and `H + σ`, so
/// clocks land on both sides of both thresholds at replicas whose clocks differ by up to `σ`.
fn ticks_ms() -> Result<[u128; 6]> {
    let horizon = production_law()?.add_horizon_ms;
    let sigma = TS_OFFSET_TOLERANCE_MS;
    Ok([
        1,
        sigma / 2,
        sigma,
        horizon / 3,
        horizon - sigma,
        horizon + sigma,
    ])
}

/// A seeded SplitMix64 stream.
struct Prng(u64);

impl Prng {
    /// A uniform draw in `0..bound`.
    fn below(&mut self, bound: usize) -> usize {
        self.0 = splitmix64(self.0);
        let bound = u64::try_from(bound.max(1)).unwrap_or(u64::MAX);
        usize::try_from(self.0 % bound).unwrap_or(0)
    }

    /// One of `items`.
    fn pick<T: Copy>(&mut self, items: &[T], fallback: T) -> T {
        items
            .get(self.below(items.len()))
            .copied()
            .unwrap_or(fallback)
    }
}

/// How a removal names what it removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Witness {
    /// The payload bytes: the target removes the dot it holds.
    Value,
    /// The dot the issuer holds: the target removes it only if it holds exactly that dot.
    Dot,
}

/// One step of the environment.
#[derive(Clone, Copy, Debug)]
enum Action {
    /// An owner appends a value, asking for the maximal retention when the flag is set.
    Add(usize, &'static str, bool),
    /// An owner overwrites the topic with the values in the mask (bit `i` for value `i`).
    Overwrite(usize, u8),
    /// The first owner removes a value it sees, applied at the second owner's storage.
    Remove(usize, usize, &'static str, Witness),
    /// A carrier is read at the first replica and delivered to the second.
    Sync(usize, usize),
    /// An ownership hand-off copy from the first owner, joined at the second.
    HandoffCopy(usize, usize),
    /// The acknowledgement of the owner's oldest pending hand-off copy: the ack-gated delete.
    HandoffAck(usize),
    /// Real time advances.
    Tick(u128),
    /// A replica's clock offset changes to the given one, stepping its clock back or forward.
    Drift(usize, u128),
}

/// A delivered add: the history `NoLoss` quantifies over.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Add {
    /// The payload.
    value: Bytes,
    /// The dot its writer issued.
    dot: EntryDot,
    /// The retention bound the write asked for.
    expires_at_ms: u128,
}

/// One replica: its stored carrier and its history variables.
#[derive(Clone, Debug, Default)]
struct Replica {
    /// The stored value, `None` for an empty slot.
    carrier: Option<Entry>,
    /// Every add delivered here while visible at its sender.
    received: BTreeSet<Add>,
    /// Every remove observed here, as the payload and the dot the remove covers up to.
    killed: BTreeSet<(Bytes, EntryDot)>,
    /// The greatest overwrite register observed here.
    floor: Option<EntryVersion>,
}

/// What holds a carrier live past its retention bound: under the production law, every unstable
/// remove and the unstable register; a mutant drops one of the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holders {
    /// The production liveness, [`Entry::is_live_under`].
    Production,
    /// Mutant: an unstable remove does not hold its carrier.
    RegisterOnly,
    /// Mutant: the unstable register does not hold its carrier.
    RemovesOnly,
}

impl Holders {
    /// Whether `carrier` is live at `now_ms` under `law` and these holders.
    fn live(self, carrier: &Entry, law: ElementRetention, now_ms: u128) -> bool {
        let (removes_hold, register_holds) = match self {
            Self::Production => return carrier.is_live_under(Some(law), now_ms),
            Self::RegisterOnly => (false, true),
            Self::RemovesOnly => (true, false),
        };
        let removes = carrier
            .crdt
            .tombstones
            .iter()
            .map(|tombstone| tombstone.dot.version)
            .filter(|_| removes_hold);
        let held_until = removes
            .chain(carrier.crdt.register.filter(|_| register_holds))
            .map(|version| law.stable_at(&version))
            .max();
        carrier.expires_at_ms.is_some()
            && (carrier.bound_live_at(now_ms) || held_until.is_some_and(|until| now_ms < until))
    }
}

/// The model state.
struct World {
    /// The retention law under test.
    law: ElementRetention,
    /// What holds a carrier live past its bound under the law under test.
    holders: Holders,
    /// The topic's entry DID.
    topic: Did,
    /// The payloads, encoded.
    values: Vec<(&'static str, Bytes)>,
    /// Real time.
    real_ms: u128,
    /// Each replica's clock offset from real time.
    skews_ms: [u128; REPLICAS],
    /// Each replica's greatest clock so far.
    peaks_ms: [u128; REPLICAS],
    /// The replicas.
    replicas: Vec<Replica>,
    /// Every add ever issued, for its requested retention.
    issued: BTreeSet<Add>,
    /// Every remove ever issued, as the dot it covers up to.
    removes: Vec<EntryDot>,
    /// Hand-off copies awaiting their acknowledgement, oldest first, with their sender.
    pending: Vec<(usize, Entry)>,
}

impl World {
    /// The initial state under `law`: empty carriers at [`MODEL_EPOCH_MS`].
    fn new(law: ElementRetention, holders: Holders) -> Result<Self> {
        Ok(Self {
            law,
            holders,
            topic: Entry::gen_did(MODEL_TOPIC)?,
            values: MODEL_VALUES
                .into_iter()
                .map(|value| Ok((value, Bytes::from(value.to_string()))))
                .collect::<Result<Vec<_>>>()?,
            real_ms: MODEL_EPOCH_MS,
            skews_ms: [0; REPLICAS],
            peaks_ms: [MODEL_EPOCH_MS; REPLICAS],
            replicas: vec![Replica::default(); REPLICAS],
            issued: BTreeSet::new(),
            removes: Vec::new(),
            pending: Vec::new(),
        })
    }

    /// Replica `replica`'s clock.
    fn clock(&self, replica: usize) -> u128 {
        self.real_ms + self.skews_ms.get(replica).copied().unwrap_or(0)
    }

    /// Replica `replica`'s greatest clock so far.
    fn peak(&self, replica: usize) -> u128 {
        self.peaks_ms.get(replica).copied().unwrap_or(u128::MAX)
    }

    /// Record every replica's current clock into its peak.
    fn observe_clocks(&mut self) {
        for replica in 0..REPLICAS {
            let clock = self.clock(replica);
            if let Some(peak) = self.peaks_ms.get_mut(replica) {
                *peak = (*peak).max(clock);
            }
        }
    }

    /// The encoded payload named `value`.
    fn encoded(&self, value: &str) -> Result<Bytes> {
        self.values
            .iter()
            .find_map(|(name, encoded)| (*name == value).then(|| encoded.clone()))
            .ok_or_else(|| Error::InvalidMessage(format!("unknown model value {value}")))
    }

    /// The empty carrier of the topic.
    fn empty(&self) -> Entry {
        Entry::new(self.topic, Vec::new(), EntryKind::Data)
    }

    /// Project `carrier` at `now_ms` under the law, `None` when the result is not live.
    fn project(&self, carrier: Entry, now_ms: u128) -> Option<Entry> {
        let carrier = carrier.retired_under(Some(self.law), now_ms);
        self.holders
            .live(&carrier, self.law, now_ms)
            .then_some(carrier)
    }

    /// `read(r)`: the stored value projected at `r`'s clock, `None` when it is not live.
    fn read(&self, replica: usize) -> Option<Entry> {
        let carrier = self.replicas.get(replica)?.carrier.clone()?;
        self.project(carrier, self.clock(replica))
    }

    /// The replica `replica`.
    fn replica(&self, replica: usize) -> Result<&Replica> {
        self.replicas
            .get(replica)
            .ok_or_else(|| Error::InvalidMessage(format!("unknown replica {replica}")))
    }

    /// The replica `replica`, mutably.
    fn replica_mut(&mut self, replica: usize) -> Result<&mut Replica> {
        self.replicas
            .get_mut(replica)
            .ok_or_else(|| Error::InvalidMessage(format!("unknown replica {replica}")))
    }

    /// Store `entry` at `replica` as the storage funnels do: projected, and only when live.
    fn store(&mut self, replica: usize, entry: Entry) -> Result<()> {
        let stored = self.project(entry, self.clock(replica));
        self.replica_mut(replica)?.carrier = stored;
        Ok(())
    }

    /// The dot `entry` holds for `value`, if it holds `value`.
    fn held_dot(entry: &Entry, value: &Bytes) -> Option<EntryDot> {
        entry
            .data
            .iter()
            .zip(entry.crdt.dots.iter().copied())
            .find_map(|(held, dot)| (held == value).then_some(dot))
    }

    /// Apply one action; an action whose precondition fails is a no-op.
    fn step(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Add(owner, value, long_lived) => self.add(owner, value, long_lived)?,
            Action::Overwrite(owner, mask) => self.overwrite(owner, mask)?,
            Action::Remove(issuer, target, value, witness) => {
                self.remove(issuer, target, value, witness)?
            }
            Action::Sync(sender, receiver) => {
                self.sync(sender, receiver)?;
            }
            Action::HandoffCopy(sender, receiver) => self.handoff_copy(sender, receiver)?,
            Action::HandoffAck(sender) => self.handoff_ack(sender),
            Action::Tick(advance_ms) => self.real_ms += advance_ms,
            Action::Drift(replica, offset_ms) => {
                if let Some(skew) = self.skews_ms.get_mut(replica) {
                    *skew = offset_ms;
                }
            }
        }
        self.observe_clocks();
        Ok(())
    }

    /// Apply the stamped write `op` at `owner`'s storage and record the adds it issues.
    fn write(&mut self, owner: usize, op: EntryOperation) -> Result<()> {
        let now_ms = self.clock(owner);
        let op = op.stamped(now_ms, actor(owner))?;
        let written = op.entry().clone();
        let local = self.read(owner).unwrap_or_else(|| self.empty());
        self.store(owner, local.operate(now_ms, op, actor(owner))?)?;
        let expires_at_ms = written.expires_at_ms.unwrap_or(now_ms);
        let adds = written
            .data
            .into_iter()
            .zip(written.crdt.dots)
            .map(|(value, dot)| Add {
                value,
                dot,
                expires_at_ms,
            })
            .collect::<Vec<_>>();
        self.issued.extend(adds.iter().cloned());
        let replica = self.replica_mut(owner)?;
        replica.received.extend(adds);
        replica.floor = replica.floor.max(written.crdt.register);
        Ok(())
    }

    /// `Add(o, v)`: the production append path at `o`'s clock.
    fn add(&mut self, owner: usize, value: &str, long_lived: bool) -> Result<()> {
        let lifetime_ms = match long_lived {
            true => production_law()?.add_horizon_ms,
            false => u128::from(DEFAULT_TTL_MS),
        };
        let mut delta = Entry::new(self.topic, vec![self.encoded(value)?], EntryKind::Data);
        delta.expires_at_ms = Some(self.clock(owner) + lifetime_ms);
        self.write(owner, EntryOperation::Extend(delta))
    }

    /// `Overwrite(o, V)`: the production overwrite path at `o`'s clock, with the default
    /// retention.
    fn overwrite(&mut self, owner: usize, mask: u8) -> Result<()> {
        let values = MODEL_VALUES
            .into_iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, value)| self.encoded(value))
            .collect::<Result<Vec<_>>>()?;
        let replacement = Entry::new(self.topic, values, EntryKind::Data);
        self.write(owner, EntryOperation::Overwrite(replacement))
    }

    /// `Remove(o, p, v, w)`: `o` removes a value it sees, applied at `p`'s storage, which
    /// removes the dot it holds itself (a value witness) or the issuer's dot if it holds it.
    fn remove(
        &mut self,
        issuer: usize,
        target: usize,
        value: &str,
        witness: Witness,
    ) -> Result<()> {
        let value = self.encoded(value)?;
        let Some(issuer_dot) = self
            .read(issuer)
            .and_then(|seen| Self::held_dot(&seen, &value))
        else {
            return Ok(());
        };
        let Some(local) = self.read(target) else {
            return Ok(());
        };
        let Some(held) = Self::held_dot(&local, &value) else {
            return Ok(());
        };
        let removal = match witness {
            Witness::Value => Entry::new(self.topic, vec![value.clone()], EntryKind::Data),
            Witness::Dot => {
                let mut removal = self.empty();
                removal.crdt.dots = vec![issuer_dot];
                removal
            }
        };
        let removed = witness == Witness::Value || held == issuer_dot;
        let now_ms = self.clock(target);
        let op = EntryOperation::Tombstone(removal).stamped(now_ms, actor(issuer))?;
        self.store(target, local.operate(now_ms, op, actor(issuer))?)?;
        if removed {
            self.removes.push(held);
            self.replica_mut(target)?.killed.insert((value, held));
        }
        Ok(())
    }

    /// `Sync(s, r)`: `s`'s read delivered to `r`, as a repair, a fetch reply, or a hand-off
    /// copy, when `r` admits it. The replacing cache takes the delivery as its whole state.
    ///
    /// Post: whether `r` admitted the delivery.
    fn sync(&mut self, sender: usize, receiver: usize) -> Result<bool> {
        let Some(delta) = self.read(sender) else {
            return Ok(false);
        };
        // Admission is judged at the receiver's clock; a rejected delivery is no delivery.
        if delta.validate_bounds_at(self.clock(receiver)).is_err() {
            return Ok(false);
        }
        let replaces = receiver == REPLACE_CACHE;
        let joined = match (replaces, self.read(receiver)) {
            (false, Some(local)) => local.join(delta.clone())?,
            _ => delta.clone(),
        };
        self.store(receiver, joined)?;
        let delivered = delta
            .data
            .iter()
            .zip(delta.crdt.dots.iter())
            .filter_map(|(value, dot)| {
                self.issued
                    .iter()
                    .find(|add| add.value == *value && add.dot == *dot)
                    .cloned()
            })
            .collect::<Vec<_>>();
        let sending = self.replica(sender)?.clone();
        let receiving = self.replica_mut(receiver)?;
        if replaces {
            receiving.received = BTreeSet::new();
            receiving.killed = BTreeSet::new();
            receiving.floor = None;
        }
        receiving.received.extend(delivered);
        receiving.killed.extend(sending.killed);
        receiving.floor = receiving.floor.max(sending.floor);
        Ok(true)
    }

    /// `HandoffCopy(s, r)`: the copy is joined at `r` and awaits its acknowledgement.
    fn handoff_copy(&mut self, sender: usize, receiver: usize) -> Result<()> {
        let Some(copy) = self.read(sender) else {
            return Ok(());
        };
        if self.sync(sender, receiver)? {
            self.pending.push((sender, copy));
        }
        Ok(())
    }

    /// `HandoffAck(s)`: `s` deletes its slot iff the production acknowledgement of its oldest
    /// pending copy confirms its current value, and its history restarts with the slot.
    fn handoff_ack(&mut self, sender: usize) {
        let Some(position) = self.pending.iter().position(|(from, _)| *from == sender) else {
            return;
        };
        let (_, copy) = self.pending.remove(position);
        let ack = SyncedEntryAck::new(self.topic, copy);
        let confirmed = self
            .read(sender)
            .is_none_or(|local| ack.confirms_local_value(&local, self.clock(sender)));
        if confirmed {
            if let Some(replica) = self.replicas.get_mut(sender) {
                *replica = Replica::default();
            }
        }
    }

    /// Whether `replica` has observed a remove or a register shadowing `(value, dot)`.
    fn covered(replica: &Replica, value: &Bytes, dot: EntryDot) -> bool {
        replica
            .killed
            .iter()
            .any(|(killed, cover)| killed == value && dot <= *cover)
            || replica.floor.is_some_and(|floor| dot.version < floor)
    }

    /// Check `NoLoss`, `NoResurrection`, and `Bounded` at every replica.
    fn check_safety(&self) -> Result<()> {
        // Judged against the production law, so a law under test that holds too long fails.
        let horizon = production_law()?.add_horizon_ms;
        let window_ms = production_law()?.remove_horizon_ms;
        for (index, replica) in self.replicas.iter().enumerate() {
            let now_ms = self.clock(index);
            let read = self.read(index).unwrap_or_else(|| self.empty());
            let visible = read
                .data
                .iter()
                .cloned()
                .zip(read.crdt.dots.iter().copied())
                .collect::<Vec<_>>();
            let peak_ms = self.peak(index);
            let production_horizon = production_law()?.add_horizon_ms;
            for add in replica.received.iter() {
                let live = peak_ms < add.dot.version.logical_time_ms + production_horizon
                    && peak_ms < add.expires_at_ms;
                if live && !Self::covered(replica, &add.value, add.dot) {
                    let kept = visible
                        .iter()
                        .any(|(value, dot)| *value == add.value && *dot >= add.dot);
                    ensure(kept, format!("NoLoss at replica {index}: {add:?}"))?;
                }
            }
            for (value, dot) in visible.iter() {
                ensure(
                    !Self::covered(replica, value, *dot),
                    format!("NoResurrection at replica {index}: {value:?} at {dot:?}"),
                )?;
                ensure(
                    now_ms < dot.version.logical_time_ms + horizon,
                    format!("Bounded (add horizon) at replica {index}: {dot:?}"),
                )?;
            }
            for tombstone in read.crdt.tombstones.iter() {
                ensure(
                    now_ms < tombstone.dot.version.logical_time_ms + window_ms,
                    format!("Bounded (remove horizon) at replica {index}: {tombstone:?}"),
                )?;
            }
            let issued_in_window = self
                .removes
                .iter()
                .filter(|dot| now_ms < dot.version.logical_time_ms + window_ms)
                .count();
            ensure(
                read.data.len() <= EntryKind::Data.max_data_len()
                    && read.crdt.tombstones.len() <= issued_in_window,
                format!("Bounded (counts) at replica {index}"),
            )?;
        }
        Ok(())
    }

    /// Check `Homomorphism` of `retire_t` on one drawn pair of stored carriers at one drawn
    /// replica clock.
    ///
    /// The three draws are made whatever the state, so the draw sequence, and with it the action
    /// trace of a seed, is the same under every law.
    fn check_homomorphism(&self, prng: &mut Prng) -> Result<()> {
        let pick = |index: usize| {
            self.replicas
                .get(index)
                .and_then(|replica| replica.carrier.clone())
        };
        let (left, right, clock) = (
            prng.below(REPLICAS),
            prng.below(REPLICAS),
            prng.below(REPLICAS),
        );
        let (Some(x), Some(y)) = (pick(left), pick(right)) else {
            return Ok(());
        };
        let now_ms = self.clock(clock);
        let lhs = x.join(y.clone())?.horizon_retired_under(self.law, now_ms);
        let rhs = x
            .horizon_retired_under(self.law, now_ms)
            .join(y.horizon_retired_under(self.law, now_ms))?;
        ensure(lhs == rhs, "Homomorphism".to_string())
    }

    /// Two full exchange rounds at frozen clocks, then `Convergence` at the greatest clock.
    fn check_convergence(&mut self) -> Result<()> {
        for _ in 0..2 {
            for sender in 0..REPLICAS {
                for receiver in 0..REPLICAS {
                    if sender != receiver {
                        self.sync(sender, receiver)?;
                    }
                }
            }
        }
        let latest_ms = (0..REPLICAS)
            .map(|replica| self.clock(replica))
            .max()
            .unwrap_or(self.real_ms);
        let views = self
            .replicas
            .iter()
            .map(|replica| {
                replica
                    .carrier
                    .clone()
                    .and_then(|carrier| self.project(carrier, latest_ms))
                    .map(|carrier| carrier.data.into_iter().collect::<BTreeSet<_>>())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        ensure(
            views.windows(2).all(|pair| pair.first() == pair.get(1)),
            format!("Convergence: {views:?}"),
        )
    }
}

/// Fail with `law` unless `holds`.
fn ensure(holds: bool, law: String) -> Result<()> {
    match holds {
        true => Ok(()),
        false => Err(Error::InvalidMessage(format!("law violated: {law}"))),
    }
}

/// Draw one action.
fn draw(prng: &mut Prng, ticks: &[u128; 6]) -> Action {
    let value = prng.pick(&MODEL_VALUES, "a");
    let owner = prng.below(OWNERS);
    let other = |prng: &mut Prng, of: usize, among: usize| (of + 1 + prng.below(among - 1)) % among;
    match prng.below(26) {
        0..=5 => Action::Add(owner, value, prng.below(2) == 0),
        6..=7 => Action::Overwrite(owner, u8::try_from(prng.below(8)).unwrap_or(0)),
        8..=11 => {
            let target = match prng.below(2) {
                0 => owner,
                _ => other(prng, owner, OWNERS),
            };
            let witness = prng.pick(&[Witness::Value, Witness::Dot], Witness::Value);
            Action::Remove(owner, target, value, witness)
        }
        12..=17 => {
            let sender = prng.below(REPLICAS);
            Action::Sync(sender, other(prng, sender, REPLICAS))
        }
        18 => Action::HandoffCopy(owner, other(prng, owner, OWNERS)),
        19 => Action::HandoffAck(owner),
        20..=22 => Action::Tick(prng.pick(ticks, 1)),
        _ => Action::Drift(prng.below(REPLICAS), prng.pick(&OFFSETS_MS, 0)),
    }
}

/// Run one seeded walk under `law`, checking every safety law after every step and
/// convergence at the end.
fn walk(law: ElementRetention, holders: Holders, seed: u64) -> Result<()> {
    let ticks = ticks_ms()?;
    let mut prng = Prng(seed);
    let mut world = World::new(law, holders)?;
    let mut trace = Vec::with_capacity(STEPS);
    for _ in 0..STEPS {
        let action = draw(&mut prng, &ticks);
        trace.push(action);
        world
            .step(action)
            .and_then(|()| world.check_safety())
            .and_then(|()| world.check_homomorphism(&mut prng))
            .map_err(|error| {
                Error::InvalidMessage(format!("seed {seed}: {error}; trace {trace:?}"))
            })?;
    }
    world
        .check_convergence()
        .map_err(|error| Error::InvalidMessage(format!("seed {seed}: {error}; trace {trace:?}")))
}

/// Witness that the broken law `law` violates the law named `violated` on some seed, and that
/// the production law passes that seed: the draws do not depend on the law, so the production
/// walk replays the very trace that witnessed the violation.
fn assert_mutant_fails(law: ElementRetention, holders: Holders, violated: &str) -> Result<()> {
    let needle = format!("law violated: {violated}");
    let witness = (0..SEEDS).find(|seed| {
        walk(law, holders, *seed).is_err_and(|error| error.to_string().contains(needle.as_str()))
    });
    let Some(seed) = witness else {
        return ensure(
            false,
            format!("no seed witnesses {violated} under {law:?}, {holders:?}"),
        );
    };
    walk(production_law()?, Holders::Production, seed)
}

/// Every law of the element horizon holds on every fixed-seed interleaving of add, overwrite,
/// remove, sync, hand-off, join, and retire across three owners and two reader caches with
/// skewed and stepping clocks.
#[test]
fn test_horizon_model_laws_hold_on_seeded_interleavings() -> Result<()> {
    let law = production_law()?;
    (0..SEEDS).try_for_each(|seed| walk(law, Holders::Production, seed))
}

/// Mutant: retiring adds at `H / 2` loses live adds.
#[test]
fn test_horizon_model_catches_early_add_retirement() -> Result<()> {
    let law = production_law()?;
    let mutant = ElementRetention {
        add_horizon_ms: law.add_horizon_ms / 2,
        ..law
    };
    assert_mutant_fails(mutant, Holders::Production, "NoLoss")
}

/// Mutant: retiring adds at `2H` holds them past the horizon.
#[test]
fn test_horizon_model_catches_late_add_retirement() -> Result<()> {
    let law = production_law()?;
    let mutant = ElementRetention {
        add_horizon_ms: law.add_horizon_ms * 2,
        ..law
    };
    assert_mutant_fails(mutant, Holders::Production, "Bounded")
}

/// Mutant: collecting removes at `2(H + σ)` holds tombstones past their window.
#[test]
fn test_horizon_model_catches_late_remove_collection() -> Result<()> {
    let law = production_law()?;
    let mutant = ElementRetention {
        remove_horizon_ms: law.remove_horizon_ms * 2,
        ..law
    };
    assert_mutant_fails(mutant, Holders::Production, "Bounded")
}

/// Mutant: a remove that does not hold its carrier past the bound lets a removed payload back.
#[test]
fn test_horizon_model_catches_removes_not_holding_carrier() -> Result<()> {
    assert_mutant_fails(production_law()?, Holders::RegisterOnly, "NoResurrection")
}

/// Mutant: collecting removes at `H`, without `σ`, lets a stepped-back clock take a stale add.
#[test]
fn test_horizon_model_catches_removes_collected_without_skew() -> Result<()> {
    let law = production_law()?;
    let mutant = ElementRetention {
        remove_horizon_ms: law.add_horizon_ms,
        ..law
    };
    assert_mutant_fails(mutant, Holders::Production, "NoResurrection")
}

/// Mutant: a register that does not hold its carrier lets an overwrite collapse an unstable
/// remove into a carrier that expires at its own bound.
#[test]
fn test_horizon_model_catches_register_not_holding_carrier() -> Result<()> {
    assert_mutant_fails(production_law()?, Holders::RemovesOnly, "NoResurrection")
}
