//! Seeded model check of data-topic removal under the element horizon (#867, #872, #874).
//!
//! The carriers are production [`Entry`] values, changed only by the production operations
//! (`Entry::operate` on stamped `Extend` and `Tombstone` operations), `Entry::join`, and the
//! horizon projection `Entry::retired_at`, composed the way the storage funnels compose them:
//! a read projects the stored value at the reader's clock and retires a carrier that is no
//! longer live, a write stores the projected result. The model owns only the world: which
//! replica acts, when a carrier is delivered, and how far real time advances. Every walk is a
//! fixed-seed random interleaving, so a failure replays exactly.
//!
//! # Specification (TLA+ style)
//!
//! ```text
//! CONSTANTS  Owners = {0, 1, 2}, Caches = {3, 4}, Replicas = Owners ∪ Caches
//!            Values = {a, b, c}
//!            Offsets = {0, σ/4, σ/2, 3σ/4, σ}                   \* pairwise ≤ σ
//!            H       = EntryKind::Data.element_horizon_ms(),  σ = TS_OFFSET_TOLERANCE_MS
//!
//! VARIABLES  real     : ℕ                               \* clock(r) = real + skew[r]
//!            skew     : Replica → Offsets                \* a clock may step back by ≤ σ
//!            peak     : Replica → ℕ                      \* history: max clock(r) so far
//!            carrier  : Replica → Entry ∪ {⊥}            (production values)
//!            received : Replica → SUBSET Add              \* history: adds delivered to r
//!            killed   : Replica → SUBSET (Value × Dot)    \* history: removes r observed
//!
//! read(r)       ≜ LET x ≜ retire_{clock(r)}(carrier[r]) IN IF live(x, clock(r)) THEN x ELSE ⊥
//! Add(o, v)     ≜ carrier'[o] = retire(read(o).operate(Extend(v) stamped at clock(o)))
//! Remove(o, v)  ≜ v ∈ read(o) ∧ carrier'[o] = retire(read(o).operate(Tombstone(v)))
//!                 ∧ killed'[o] = killed[o] ∪ {(v, dot of v in read(o))}
//! Sync(s, r)    ≜ admissible(read(s), clock(r))
//!                 ∧ carrier'[r] = retire_{clock(r)}(read(r) ⊔ read(s))
//!                 ∧ received'[r] = received[r] ∪ adds(read(s))
//!                 ∧ killed'[r] = killed[r] ∪ killed[s]
//! Tick(Δ)       ≜ real' = real + Δ,  Δ ∈ {1, σ/2, σ, H/3, H − σ, H + σ}
//! Drift(r, o)   ≜ skew'[r] = o,  o ∈ Offsets                 \* NTP steps within tolerance
//!
//! Next ≜ ∃o ∈ Owners, v ∈ Values: Add(o, v) ∨ Remove(o, v)
//!      ∨ ∃s ≠ r ∈ Replicas: Sync(s, r)  ∨  ∃Δ: Tick(Δ)  ∨  ∃r, o: Drift(r, o)
//!
//! covered_r(v, d) ≜ ∃(v, k) ∈ killed[r]. d ≤ k
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
//! requires a data carrier holding an unstable remove to stay live past its retention bound
//! (`Entry::is_live_at`): the model found that a carrier expiring with its removes let a replica
//! whose carrier other writes kept alive serve a removed payload back.
//!
//! A reader cache is a carrier that joins every reply it observes (the read-join of
//! #864); a cache that replaces its value with the last reply is a special case, one owner's
//! read. Caches are also delivered back to owners here, which production never does, so the
//! laws are checked against a strictly larger set of behaviours. `Overwrite` is left out: the
//! only removal it adds is the one `NoLoss` names explicitly.

use std::collections::BTreeSet;

use super::Entry;
use super::EntryDot;
use super::EntryKind;
use super::EntryOperation;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::tests::splitmix64;

/// The model's topic.
const MODEL_TOPIC: &str = "element horizon model";

/// The payloads owners write.
const MODEL_VALUES: [&str; 3] = ["a", "b", "c"];

/// The replicas that write: storage owners.
const OWNERS: usize = 3;

/// Every replica: the owners, then two reader caches.
const REPLICAS: usize = 5;

/// The fixed seeds, one walk each.
const SEEDS: u64 = 48;

/// The steps of one walk.
const STEPS: usize = 160;

/// Real time at the start of every walk.
const MODEL_EPOCH_MS: u128 = 1_700_000_000_000;

/// The data element horizon `H`.
fn horizon_ms() -> Result<u128> {
    EntryKind::Data
        .element_horizon_ms()
        .map(u128::from)
        .ok_or_else(|| Error::InvalidMessage("a data topic has an element horizon".to_string()))
}

/// The clock offsets from real time a replica may take; they differ pairwise by at most `σ`.
const OFFSETS_MS: [u128; 5] = [
    0,
    TS_OFFSET_TOLERANCE_MS / 4,
    TS_OFFSET_TOLERANCE_MS / 2,
    TS_OFFSET_TOLERANCE_MS * 3 / 4,
    TS_OFFSET_TOLERANCE_MS,
];

/// The storage node that acts for replica `replica`.
fn actor(replica: usize) -> Did {
    Did::from(u32::try_from(replica).unwrap_or(u32::MAX).saturating_add(1))
}

/// The real-time advances a `Tick` draws from: on either side of `σ`, `H`, and `H + σ`, so
/// clocks land on both sides of both thresholds at replicas whose clocks differ by up to `σ`.
fn ticks_ms() -> Result<[u128; 6]> {
    let horizon = horizon_ms()?;
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
}

/// One step of the environment.
#[derive(Clone, Copy, Debug)]
enum Action {
    /// An owner appends a value, asking for the maximal retention when the flag is set.
    Add(usize, &'static str, bool),
    /// An owner removes a value it sees.
    Remove(usize, &'static str),
    /// A carrier is read at the first replica and joined into the second.
    Sync(usize, usize),
    /// Real time advances.
    Tick(u128),
    /// A replica's clock offset changes to the given one, stepping its clock back or forward.
    Drift(usize, u128),
}

/// A delivered add: the history `NoLoss` quantifies over.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Add {
    /// The payload.
    value: Encoded,
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
    killed: BTreeSet<(Encoded, EntryDot)>,
}

/// The model state.
struct World {
    /// The topic's entry DID.
    topic: Did,
    /// The payloads, encoded.
    values: Vec<(&'static str, Encoded)>,
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
}

impl World {
    /// The initial state: empty carriers at [`MODEL_EPOCH_MS`].
    fn new() -> Result<Self> {
        Ok(Self {
            topic: Entry::gen_did(MODEL_TOPIC)?,
            values: MODEL_VALUES
                .into_iter()
                .map(|value| Ok((value, value.to_string().encode()?)))
                .collect::<Result<Vec<_>>>()?,
            real_ms: MODEL_EPOCH_MS,
            skews_ms: [0; REPLICAS],
            peaks_ms: [MODEL_EPOCH_MS; REPLICAS],
            replicas: vec![Replica::default(); REPLICAS],
            issued: BTreeSet::new(),
            removes: Vec::new(),
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
    fn encoded(&self, value: &str) -> Result<Encoded> {
        self.values
            .iter()
            .find_map(|(name, encoded)| (*name == value).then(|| encoded.clone()))
            .ok_or_else(|| Error::InvalidMessage(format!("unknown model value {value}")))
    }

    /// The empty carrier of the topic.
    fn empty(&self) -> Entry {
        Entry::new(self.topic, Vec::new(), EntryKind::Data)
    }

    /// `read(r)`: the stored value projected at `r`'s clock, `None` when it is not live.
    fn read(&self, replica: usize) -> Result<Option<Entry>> {
        let now_ms = self.clock(replica);
        match self.replicas.get(replica).and_then(|r| r.carrier.clone()) {
            Some(carrier) => {
                let carrier = carrier.retired_at(now_ms)?;
                Ok(carrier.is_live_at(now_ms).then_some(carrier))
            }
            None => Ok(None),
        }
    }

    /// The replica `replica`, mutably.
    fn replica_mut(&mut self, replica: usize) -> Result<&mut Replica> {
        self.replicas
            .get_mut(replica)
            .ok_or_else(|| Error::InvalidMessage(format!("unknown replica {replica}")))
    }

    /// Store `entry` at `replica` as the storage funnels do: projected, and only when live.
    fn store(&mut self, replica: usize, entry: Entry) -> Result<()> {
        let now_ms = self.clock(replica);
        let entry = entry.retired_at(now_ms)?;
        self.replica_mut(replica)?.carrier = entry.is_live_at(now_ms).then_some(entry);
        Ok(())
    }

    /// The dot `entry` holds for `value`, if it holds `value`.
    fn held_dot(entry: &Entry, value: &Encoded) -> Option<EntryDot> {
        entry
            .data
            .iter()
            .zip(entry.crdt.dots.iter().copied())
            .find_map(|(held, dot)| (held == value).then_some(dot))
    }

    /// Apply one action. `Remove` of a value its owner does not see is a no-op.
    fn step(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Add(owner, value, long_lived) => self.add(owner, value, long_lived)?,
            Action::Remove(owner, value) => self.remove(owner, value)?,
            Action::Sync(sender, receiver) => self.sync(sender, receiver)?,
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

    /// `Add(o, v)`: the production append path at `o`'s clock.
    fn add(&mut self, owner: usize, value: &str, long_lived: bool) -> Result<()> {
        let now_ms = self.clock(owner);
        let value = self.encoded(value)?;
        let lifetime_ms = match long_lived {
            true => horizon_ms()?,
            false => u128::from(DEFAULT_TTL_MS),
        };
        let mut delta = Entry::new(self.topic, vec![value.clone()], EntryKind::Data);
        delta.expires_at_ms = Some(now_ms + lifetime_ms);
        let op = EntryOperation::Extend(delta).stamped(now_ms, actor(owner))?;
        let dot = op
            .entry()
            .crdt
            .dots
            .first()
            .copied()
            .ok_or_else(|| Error::InvalidMessage("a stamped add carries a dot".to_string()))?;
        let local = self.read(owner)?.unwrap_or_else(|| self.empty());
        self.store(owner, local.operate(now_ms, op, actor(owner))?)?;
        let add = Add {
            value,
            dot,
            expires_at_ms: now_ms + lifetime_ms,
        };
        self.issued.insert(add.clone());
        self.replica_mut(owner)?.received.insert(add);
        Ok(())
    }

    /// `Remove(o, v)`: the production removal path at `o`'s clock, when `o` sees `v`.
    fn remove(&mut self, owner: usize, value: &str) -> Result<()> {
        let now_ms = self.clock(owner);
        let value = self.encoded(value)?;
        let Some(local) = self.read(owner)? else {
            return Ok(());
        };
        let Some(dot) = Self::held_dot(&local, &value) else {
            return Ok(());
        };
        let removal = Entry::new(self.topic, vec![value.clone()], EntryKind::Data);
        let op = EntryOperation::Tombstone(removal).stamped(now_ms, actor(owner))?;
        self.store(owner, local.operate(now_ms, op, actor(owner))?)?;
        self.removes.push(dot);
        self.replica_mut(owner)?.killed.insert((value, dot));
        Ok(())
    }

    /// `Sync(s, r)`: `s`'s read joined into `r`, as a hand-off, a repair, or a fetch reply, when
    /// `r` admits it.
    fn sync(&mut self, sender: usize, receiver: usize) -> Result<()> {
        let Some(delta) = self.read(sender)? else {
            return Ok(());
        };
        // Admission is judged at the receiver's clock; a rejected delivery is no delivery.
        if delta.validate_bounds_at(self.clock(receiver)).is_err() {
            return Ok(());
        }
        let joined = match self.read(receiver)? {
            Some(local) => local.join(delta.clone())?,
            None => delta.clone(),
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
        let killed = self
            .replicas
            .get(sender)
            .map(|replica| replica.killed.clone())
            .unwrap_or_default();
        let receiving = self.replica_mut(receiver)?;
        receiving.received.extend(delivered);
        receiving.killed.extend(killed);
        Ok(())
    }

    /// Whether `replica` has observed a remove covering `(value, dot)`.
    fn covered(&self, replica: &Replica, value: &Encoded, dot: EntryDot) -> bool {
        replica
            .killed
            .iter()
            .any(|(killed, cover)| killed == value && dot <= *cover)
    }

    /// Check `NoLoss`, `NoResurrection`, and `Bounded` at every replica.
    fn check_safety(&self) -> Result<()> {
        let horizon = horizon_ms()?;
        for (index, replica) in self.replicas.iter().enumerate() {
            let now_ms = self.clock(index);
            let read = self.read(index)?.unwrap_or_else(|| self.empty());
            let visible = read
                .data
                .iter()
                .cloned()
                .zip(read.crdt.dots.iter().copied())
                .collect::<Vec<_>>();
            for add in replica.received.iter() {
                let peak_ms = self.peak(index);
                let live = peak_ms < add.dot.version.logical_time_ms + horizon
                    && peak_ms < add.expires_at_ms;
                if live && !self.covered(replica, &add.value, add.dot) {
                    let kept = visible
                        .iter()
                        .any(|(value, dot)| *value == add.value && *dot >= add.dot);
                    ensure(kept, format!("NoLoss at replica {index}: {add:?}"))?;
                }
            }
            for (value, dot) in visible.iter() {
                ensure(
                    !self.covered(replica, value, *dot),
                    format!("NoResurrection at replica {index}: {value:?} at {dot:?}"),
                )?;
                ensure(
                    now_ms < dot.version.logical_time_ms + horizon,
                    format!("Bounded (add horizon) at replica {index}: {dot:?}"),
                )?;
            }
            let window_ms = horizon + TS_OFFSET_TOLERANCE_MS;
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

    /// Check `Homomorphism` on one drawn pair of stored carriers at one drawn replica clock.
    fn check_homomorphism(&self, prng: &mut Prng) -> Result<()> {
        let pick = |index: usize| {
            self.replicas
                .get(index)
                .and_then(|replica| replica.carrier.clone())
        };
        let (Some(x), Some(y)) = (pick(prng.below(REPLICAS)), pick(prng.below(REPLICAS))) else {
            return Ok(());
        };
        let now_ms = self.clock(prng.below(REPLICAS));
        let lhs = x.join(y.clone())?.retired_at(now_ms)?;
        let rhs = x
            .retired_at(now_ms)?
            .join(y.retired_at(now_ms)?)?
            .try_into_storage_entry()?;
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
            .map(|replica| match replica.carrier.clone() {
                Some(carrier) => {
                    let carrier = carrier.retired_at(latest_ms)?;
                    Ok(match carrier.is_live_at(latest_ms) {
                        true => carrier.data.into_iter().collect::<BTreeSet<_>>(),
                        false => BTreeSet::new(),
                    })
                }
                None => Ok(BTreeSet::new()),
            })
            .collect::<Result<Vec<_>>>()?;
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
    let value = MODEL_VALUES
        .get(prng.below(MODEL_VALUES.len()))
        .copied()
        .unwrap_or("a");
    match prng.below(22) {
        0..=5 => Action::Add(prng.below(OWNERS), value, prng.below(2) == 0),
        6..=9 => Action::Remove(prng.below(OWNERS), value),
        10..=16 => {
            let sender = prng.below(REPLICAS);
            let receiver = (sender + 1 + prng.below(REPLICAS - 1)) % REPLICAS;
            Action::Sync(sender, receiver)
        }
        17..=19 => Action::Tick(ticks.get(prng.below(ticks.len())).copied().unwrap_or(1)),
        _ => Action::Drift(
            prng.below(REPLICAS),
            OFFSETS_MS
                .get(prng.below(OFFSETS_MS.len()))
                .copied()
                .unwrap_or(0),
        ),
    }
}

/// Run one seeded walk, checking every safety law after every step and convergence at the end.
fn walk(seed: u64) -> Result<()> {
    let ticks = ticks_ms()?;
    let mut prng = Prng(seed);
    let mut world = World::new()?;
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

/// Every law of the element horizon holds on every fixed-seed interleaving of add, remove,
/// sync, join, and retire across three owners and two reader caches with skewed clocks.
#[test]
fn test_horizon_model_laws_hold_on_seeded_interleavings() -> Result<()> {
    (0..SEEDS).try_for_each(walk)
}
