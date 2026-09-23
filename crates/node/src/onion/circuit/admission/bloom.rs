//! Replay store `R_i`: sliced Bloom filters keyed by the quantised expiry `x` (L9).
//!
//! # Model
//!
//! A *block* is a sliced Bloom filter (Almeida et al., 2007): `K` slices of `M` bits each, and
//! hash `g_j` addresses slice `j` only. After `n` insertions a slice has fill ratio
//! `p = 1 − (1 − 1/M)ⁿ ≤ 1 − e^{−n/M}`, and a fresh tag is a false positive with probability
//! `pᴷ`. With `n = B`, `M ≥ B / ln 2` and `K = 26`, `p ≤ ½`, so the block rate is `≤ 2⁻²⁶`.
//!
//! A *filter* `R_i[x]` is a non-empty chain of blocks: every block but the open one holds exactly
//! `B` tags, and a tag enters the open block, which is sealed and succeeded when full. Its rate
//! is at most the sum of its block rates (union bound): `≤ β · 2⁻²⁶` for `β` blocks, hence
//! `≤ 2⁻²⁰` for the `β ≤ 64` blocks that the global admission budget permits.
//!
//! The store is the finite map `x ↦ R_i[x]`. [`ReplayStore::forget_through`] drops a whole
//! filter once its `x` has passed.
//!
//! # Laws
//!
//! * No false negatives: `insert(x, ν) ; contains(x, ν) = true` until `x` is forgotten. Every
//!   bit addressed by `ν` is set by `insert`, and bits are never cleared inside a live filter.
//! * Monotonicity: `contains(x, ·)` is increasing in the set of inserted tags. In the lattice of
//!   bit sets, `insert` is the join `R ↦ R ∨ bits(ν)` and `contains` is the order test
//!   `bits(ν) ≤ R`.
//! * Adversarial resistance: the probe `g(ν)` is derived from `Keccak-256(κ ‖ ν ‖ i)`, keyed by a process-secret
//!   `κ`. A sender that chooses `ν` therefore cannot aim its tags at chosen bits to inflate the
//!   fill ratio above the random-oracle bound.

use std::collections::BTreeMap;

use arrayref::mut_array_refs;
use rings_core::ecc::keccak256;

use super::OnionExpiry;
use super::ONION_ADMISSION_SENDER_UNITS;
use crate::onion::circuit::OnionForwardNonce;

/// Tags per block, `B`: one sender's whole budget fits in one block.
pub(super) const REPLAY_BLOCK_TAGS: u32 = ONION_ADMISSION_SENDER_UNITS;

/// Hash functions per block, `K`, one per slice: a full block has rate `2⁻ᴷ = 2⁻²⁶`.
pub(super) const REPLAY_BLOCK_HASHES: usize = 26;

/// 64-bit words per slice, `W = ⌈⌈B / ln 2⌉ / 64⌉ = ⌈23 638 / 64⌉ = 370`.
pub(super) const REPLAY_SLICE_WORDS: usize = 370;

/// Bits per slice, `M = 64 · W = 23 680 ≥ B / ln 2`, so a full block's fill ratio is at most `½`.
pub(super) const REPLAY_SLICE_BITS: u32 = 23_680;

/// Secret key `κ` of the probe family `g(ν)`, derived from `Keccak-256(κ ‖ ν ‖ i)`.
///
/// Drawn once per process, like the process epoch. It is never serialised and never leaves the
/// admission state.
#[derive(Clone)]
pub(crate) struct OnionReplayFilterKey([u8; 32]);

impl OnionReplayFilterKey {
    /// Wrap 32 secret bytes supplied by the caller's RNG, which keeps time and randomness injected.
    pub(crate) const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// Keccak-256 blocks in a probe's address stream: `⌈4 · K / 32⌉ = 4`.
const REPLAY_PROBE_DIGESTS: usize = 4;

/// The bit addresses of one tag, computed once and shared by every block of every filter.
///
/// Slice `j` is addressed at `g_j(ν) = w_j mod M`, where `w_j` is the `j`-th little-endian 32-bit
/// word of the keyed stream `Keccak-256(κ ‖ ν ‖ 0) ‖ … ‖ Keccak-256(κ ‖ ν ‖ 3)`. The addresses are
/// independent. Double hashing `h₁ + j·h₂ mod M` would give only `M² ≈ 2²⁹` probe patterns, and
/// two tags with one pattern collide in every block. That puts a false-positive floor of
/// `n / M² ≈ 2⁻⁹` at `n = 64·B` tags, far above `2⁻²⁰`. The reduction bias of `w_j mod M` is at
/// most `M / 2³² < 2⁻¹⁷` relative per address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReplayProbe {
    /// `g_0(ν), …, g_{K−1}(ν)`, each in `[0, M)`.
    addresses: [u32; REPLAY_BLOCK_HASHES],
}

impl ReplayProbe {
    /// Derive the probe of `tag` under `key` from the keyed Keccak-256 counter stream.
    pub(super) fn of(key: &OnionReplayFilterKey, tag: OnionForwardNonce) -> Self {
        let OnionReplayFilterKey(key_bytes) = key;
        let OnionForwardNonce(tag_bytes) = tag;
        let mut preimage = [0_u8; 49];
        let mut stream = [0_u8; 32 * REPLAY_PROBE_DIGESTS];
        let mut addresses = [0_u32; REPLAY_BLOCK_HASHES];
        stream
            .as_chunks_mut::<32>()
            .0
            .iter_mut()
            .zip(0_u8..)
            .for_each(|(digest, counter)| {
                let (key_part, tag_part, counter_part) = mut_array_refs!(&mut preimage, 32, 16, 1);
                key_part.copy_from_slice(key_bytes.as_slice());
                tag_part.copy_from_slice(tag_bytes.as_slice());
                *counter_part = [counter];
                *digest = keccak256(preimage.as_slice());
            });
        addresses
            .iter_mut()
            .zip(stream.as_chunks::<4>().0)
            .for_each(|(address, word)| {
                *address = u32::from_le_bytes(word.to_owned()) % REPLAY_SLICE_BITS;
            });
        Self { addresses }
    }

    /// The `K` slice-local bit addresses in slice order.
    fn addresses(&self) -> impl Iterator<Item = u32> + '_ {
        self.addresses.iter().copied()
    }
}

/// Split a slice-local bit address into its word index and the mask of its bit in that word.
///
/// The index is `None` only where `usize` is narrower than 32 bits, which Rings does not target.
/// Callers resolve `None` towards presence, so even a violated geometry could only add false
/// positives, never false negatives.
fn word_and_mask(address: u32) -> (Option<usize>, u64) {
    (
        usize::try_from(address / u64::BITS).ok(),
        1_u64 << (address % u64::BITS),
    )
}

/// One sliced Bloom block of at most `B` tags at false-positive rate `≤ 2⁻²⁶`.
#[derive(Debug)]
pub(super) struct ReplayBlock {
    /// `K` consecutive slices of `W` words each.
    bits: Box<[u64]>,
    /// Number of tags inserted, `≤ B`.
    tags: u32,
}

impl ReplayBlock {
    /// A block with no tag: `K · W` zero words (≈ 76.96 KB).
    fn empty() -> Self {
        Self {
            bits: vec![0; REPLAY_BLOCK_HASHES * REPLAY_SLICE_WORDS].into_boxed_slice(),
            tags: 0,
        }
    }

    /// Whether the block already holds its `B` tags and must be succeeded by a fresh one.
    fn is_full(&self) -> bool {
        self.tags >= REPLAY_BLOCK_TAGS
    }

    /// Order test `bits(ν) ≤ R`: every addressed bit of every slice is set.
    fn contains(&self, probe: &ReplayProbe) -> bool {
        self.bits
            .chunks_exact(REPLAY_SLICE_WORDS)
            .zip(probe.addresses())
            .all(|(slice, address)| {
                let (word, mask) = word_and_mask(address);
                word.and_then(|word| slice.get(word))
                    .is_none_or(|bits| bits & mask == mask)
            })
    }

    /// Join `R ∨ bits(ν)`: set every addressed bit of every slice and count the tag.
    fn insert(&mut self, probe: &ReplayProbe) {
        self.bits
            .chunks_exact_mut(REPLAY_SLICE_WORDS)
            .zip(probe.addresses())
            .for_each(|(slice, address)| {
                let (word, mask) = word_and_mask(address);
                if let Some(bits) = word.and_then(|word| slice.get_mut(word)) {
                    *bits |= mask;
                }
            });
        self.tags = self.tags.saturating_add(1);
    }

    /// Exact false-positive rate of this block for a uniformly random probe: `∏_j (|slice_j| / M)`.
    #[cfg(all(test, rings_native))]
    pub(super) fn false_positive_rate(&self) -> f64 {
        self.bits
            .chunks_exact(REPLAY_SLICE_WORDS)
            .map(|slice| {
                let ones = slice.iter().map(|word| word.count_ones()).sum::<u32>();
                f64::from(ones) / f64::from(REPLAY_SLICE_BITS)
            })
            .product()
    }
}

/// The filter `R_i[x]`: a chain of full sealed blocks and one open block.
///
/// Illegal states are unrepresentable: a filter always holds at least one tag in its open block
/// (it is created by its first insertion), and a block is sealed only when it is full.
#[derive(Debug)]
pub(super) struct ReplayFilter {
    /// Full blocks, each with exactly `B` tags, in insertion order.
    sealed: Vec<ReplayBlock>,
    /// The block that receives the next tag.
    open: ReplayBlock,
}

impl ReplayFilter {
    /// The filter `{ν}` created by its first tag.
    fn singleton(probe: &ReplayProbe) -> Self {
        let mut open = ReplayBlock::empty();
        open.insert(probe);
        Self {
            sealed: Vec::new(),
            open,
        }
    }

    /// Whether any block may hold `ν`. There are no false negatives.
    fn contains(&self, probe: &ReplayProbe) -> bool {
        self.open.contains(probe) || self.sealed.iter().any(|block| block.contains(probe))
    }

    /// Insert `ν` into the open block, sealing it first when it already holds `B` tags.
    fn insert(&mut self, probe: &ReplayProbe) {
        if self.open.is_full() {
            self.sealed
                .push(std::mem::replace(&mut self.open, ReplayBlock::empty()));
        }
        self.open.insert(probe);
    }

    /// The blocks in insertion order, the open block last.
    #[cfg(all(test, rings_native))]
    pub(super) fn blocks(&self) -> impl Iterator<Item = &ReplayBlock> {
        self.sealed.iter().chain([&self.open])
    }
}

/// The replay store `R_i = x ↦ R_i[x]` with its secret probe key.
pub(super) struct ReplayStore {
    /// Secret key `κ` of the probe family.
    key: OnionReplayFilterKey,
    /// Live filters, ordered by expiry.
    filters: BTreeMap<OnionExpiry, ReplayFilter>,
}

impl ReplayStore {
    /// An empty store under the process-secret key `κ`.
    pub(super) fn new(key: OnionReplayFilterKey) -> Self {
        Self {
            key,
            filters: BTreeMap::new(),
        }
    }

    /// The probe of `tag` under this store's key.
    pub(super) fn probe(&self, tag: OnionForwardNonce) -> ReplayProbe {
        ReplayProbe::of(&self.key, tag)
    }

    /// Drop every filter whose expiry has passed, `x ≤ now`, all at once.
    pub(super) fn forget_through(&mut self, now_ms: u128) {
        self.filters.retain(|expiry, _| expiry.as_ms() > now_ms);
    }

    /// Whether `ν` may already be in `R_i[x]`. There are no false negatives.
    pub(super) fn contains(&self, expiry: OnionExpiry, probe: &ReplayProbe) -> bool {
        self.filters
            .get(&expiry)
            .is_some_and(|filter| filter.contains(probe))
    }

    /// `R_i[x] ← R_i[x] ∪ {ν}`, creating the filter with its first tag.
    pub(super) fn insert(&mut self, expiry: OnionExpiry, probe: &ReplayProbe) {
        self.filters
            .entry(expiry)
            .and_modify(|filter| filter.insert(probe))
            .or_insert_with(|| ReplayFilter::singleton(probe));
    }

    /// The live filters, ordered by expiry.
    #[cfg(all(test, rings_native))]
    pub(super) fn filters(&self) -> &BTreeMap<OnionExpiry, ReplayFilter> {
        &self.filters
    }
}
