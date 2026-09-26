//! Replay store `R_i`: sliced Bloom filters keyed by the quantised expiry `x` (L9).
//!
//! # Model
//!
//! A *block* is a sliced Bloom filter (Almeida et al., 2007): `K` slices of `M` bits each, and
//! the address `g_j` hits slice `j` only. The addresses are independent and uniform, so after `n`
//! insertions each slice has expected fill `p_n = 1 − (1 − 1/M)ⁿ`. Slices are independent, so a
//! fresh tag is a false positive with probability exactly `p_nᴷ`. (The familiar approximation
//! `1 − e^{−n/M}` is a *lower* bound on `p_n`, so it cannot certify an upper bound on the rate.)
//! With `n = B = 16 384`, `K = 26` and `M = 23 680`:
//!
//! ```text
//! (1 − (1 − 1/M)^B)^K ≤ 2⁻²⁶   ⇔   M ≥ 23 637.8
//! ```
//!
//! A *filter* `R_i[x]` is a non-empty chain of blocks in which every block except the open one
//! holds exactly `B` tags. A tag enters the open block, which is sealed and succeeded when full.
//! The filter's rate is at most the sum of its block rates (union bound): `≤ β · 2⁻²⁶` for `β`
//! blocks, hence `≤ 2⁻²⁰` for the `β ≤ 64` blocks that the global admission budget permits.
//!
//! The store is the finite map `x ↦ R_i[x]`. [`ReplayStore::forget_through`] drops a whole filter
//! once its `x` has passed.
//!
//! # Laws
//!
//! * No false negatives: after `insert(x, ν)`, `contains(x, ν) = true` until `x` is forgotten.
//!   `insert` sets every bit addressed by `ν`, and bits are never cleared inside a live filter.
//! * Monotonicity: `contains(x, ·)` is increasing in the set of inserted tags. In the lattice of
//!   bit sets, `insert` is the join `R ↦ R ∨ bits(ν)` and `contains` is the order test
//!   `bits(ν) ≤ R`.
//! * Adversarial resistance: the probe `g(ν)` comes from `Keccak-256(κ ‖ ν ‖ i)`, keyed by a
//!   process-secret `κ`. A sender that chooses `ν` therefore cannot aim its tags at chosen bits to
//!   push the fill ratio above the random-oracle bound.

use std::collections::BTreeMap;

use arrayref::mut_array_refs;
use rings_core::ecc::keccak256;
use zeroize::Zeroize;

use super::OnionExpiry;
use super::ONION_ADMISSION_SENDER_UNITS;
use crate::onion::circuit::OnionReplayNonce;

/// Tags per block, `B`: one sender's whole budget fits in one block.
pub(super) const REPLAY_BLOCK_TAGS: u32 = ONION_ADMISSION_SENDER_UNITS;

/// Hash functions per block, `K`, one per slice. A full block has rate `≤ 2⁻ᴷ = 2⁻²⁶`.
pub(super) const REPLAY_BLOCK_HASHES: usize = 26;

/// 64-bit words per slice, `W = ⌈M_min / 64⌉ = 370`, where `M_min = 23 637.8` is the least `M`
/// satisfying the exact block bound.
pub(super) const REPLAY_SLICE_WORDS: usize = 370;

/// Bits per slice, `M = 64 · W = 23 680`.
pub(super) const REPLAY_SLICE_BITS: u32 = 23_680;

/// Keccak-256 digests in a probe's address stream: `4 · K ≤ 32 · 4` bytes of 32-bit words.
const REPLAY_PROBE_DIGESTS: usize = 4;

/// Compile-time geometry laws:
/// * `M = 64 · W`, so every address `< M` names a word of its slice;
/// * `M ≤ 2¹⁶`, so an address fits `u16`;
/// * the stream has a 32-bit word for each of the `K` slices.
const _: () = assert!(
    REPLAY_SLICE_BITS as usize == 64 * REPLAY_SLICE_WORDS
        && REPLAY_SLICE_BITS <= 1 << 16
        && 4 * REPLAY_BLOCK_HASHES <= 32 * REPLAY_PROBE_DIGESTS
);

/// Secret key `κ` of the probe family `g(ν)`.
///
/// Drawn once per process, like the process epoch. It is neither cloned nor serialised, and it is
/// zeroised on drop.
pub(crate) struct OnionReplayFilterKey([u8; 32]);

impl OnionReplayFilterKey {
    /// Wrap 32 secret bytes supplied by the caller's RNG, which keeps randomness injected.
    pub(crate) const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl Drop for OnionReplayFilterKey {
    /// Zeroise `κ` so it does not outlive the admission state in freed memory.
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// The bit addresses of one tag, computed once and shared by every block of every filter.
///
/// Slice `j` is addressed at `g_j(ν) = w_j mod M`, where `w_j` is the `j`-th little-endian 32-bit
/// word of the keyed stream `Keccak-256(κ ‖ ν ‖ 0) ‖ … ‖ Keccak-256(κ ‖ ν ‖ 3)`. The addresses are
/// independent. Double hashing (`h₁ + j·h₂ mod M`) would give only `M² ≈ 2²⁹` probe patterns, and
/// two tags with the same pattern collide in every block, which puts a false-positive floor of
/// `n / M² ≈ 2⁻⁹` at `n = 64·B` tags. The reduction bias of `w_j mod M` is below `M / 2³² < 2⁻¹⁷`
/// relative per address.
///
/// The addresses are secret-keyed PRF output, so the probe is neither `Debug` nor `Copy`: it lives
/// for one step, by reference.
pub(super) struct ReplayProbe {
    /// `g_0(ν), …, g_{K−1}(ν)`, each in `[0, M)`.
    addresses: [u16; REPLAY_BLOCK_HASHES],
}

impl ReplayProbe {
    /// Derive the probe of `tag` under `key` from the keyed Keccak-256 counter stream.
    fn of(key: &OnionReplayFilterKey, tag: OnionReplayNonce) -> Self {
        let OnionReplayFilterKey(key_bytes) = key;
        let OnionReplayNonce(tag_bytes) = tag;
        let mut preimage = [0_u8; 49];
        let mut stream = [0_u8; 32 * REPLAY_PROBE_DIGESTS];
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
        preimage.zeroize();
        let mut addresses = [0_u16; REPLAY_BLOCK_HASHES];
        addresses
            .iter_mut()
            .zip(stream.as_chunks::<4>().0)
            .for_each(|(address, word)| {
                // Lossless: the remainder is below `M ≤ 2¹⁶` (const-asserted).
                *address = (u32::from_le_bytes(word.to_owned()) % REPLAY_SLICE_BITS) as u16;
            });
        stream.zeroize();
        Self { addresses }
    }
}

/// Split a slice-local bit address into its word index and the mask of its bit in that word.
/// The index is below `W`, because `address < M = 64 · W`.
fn word_and_mask(address: u16) -> (usize, u64) {
    (usize::from(address / 64), 1_u64 << (address % 64))
}

/// One slice: `M` bits in `W` words.
type ReplaySlice = [u64; REPLAY_SLICE_WORDS];

/// One sliced Bloom block of at most `B` tags at false-positive rate `≤ 2⁻²⁶`.
#[derive(Debug)]
pub(super) struct ReplayBlock {
    /// The `K` slices. Their count is part of the type, so zipping them with a probe's `K`
    /// addresses covers every slice.
    bits: Box<[ReplaySlice; REPLAY_BLOCK_HASHES]>,
    /// Number of tags inserted, `≤ B`.
    tags: u32,
}

impl ReplayBlock {
    /// A block with no tag: `K · W` zero words (≈ 76.96 KB).
    fn empty() -> Self {
        Self {
            bits: Box::new([[0; REPLAY_SLICE_WORDS]; REPLAY_BLOCK_HASHES]),
            tags: 0,
        }
    }

    /// Whether the block already holds its `B` tags and must be succeeded by a fresh one.
    fn is_full(&self) -> bool {
        self.tags >= REPLAY_BLOCK_TAGS
    }

    /// Order test `bits(ν) ≤ R`: every addressed bit of every slice is set.
    ///
    /// `get` cannot miss, because the word index is below `W`. It returns `Option` only because
    /// the index is a runtime value. The `None` arm counts as present, and [`Self::insert`] skips
    /// the same arm, so both range over the same bit set and `insert ; contains = true` holds
    /// unconditionally.
    fn contains(&self, probe: &ReplayProbe) -> bool {
        self.bits
            .iter()
            .zip(probe.addresses)
            .all(|(slice, address)| {
                let (word, mask) = word_and_mask(address);
                slice.get(word).is_none_or(|bits| bits & mask == mask)
            })
    }

    /// Join `R ∨ bits(ν)`: set every addressed bit of every slice and count the tag.
    fn insert(&mut self, probe: &ReplayProbe) {
        self.bits
            .iter_mut()
            .zip(probe.addresses)
            .for_each(|(slice, address)| {
                let (word, mask) = word_and_mask(address);
                if let Some(bits) = slice.get_mut(word) {
                    *bits |= mask;
                }
            });
        self.tags = self.tags.saturating_add(1);
    }

    /// Exact false-positive rate of this block for a uniformly random probe: `∏_j (|slice_j| / M)`.
    #[cfg(all(test, rings_native))]
    pub(super) fn false_positive_rate(&self) -> f64 {
        self.bits
            .iter()
            .map(|slice| {
                let ones = slice.iter().map(|word| word.count_ones()).sum::<u32>();
                f64::from(ones) / f64::from(REPLAY_SLICE_BITS)
            })
            .product()
    }
}

/// The filter `R_i[x]`: a chain of full sealed blocks and one open block.
///
/// An empty filter is unrepresentable: a filter is created by its first insertion, so its open
/// block always holds a tag. A sealed block that is not full is representable but not reachable:
/// only [`Self::insert`] seals a block, and it does so only when the block is full.
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
    pub(super) fn probe(&self, tag: OnionReplayNonce) -> ReplayProbe {
        ReplayProbe::of(&self.key, tag)
    }

    /// Drop every filter whose expiry has passed, `x ≤ now`, all at once.
    pub(super) fn forget_through(&mut self, now_ms: u128) {
        self.filters
            .retain(|expiry, _| !expiry.has_passed_at(now_ms));
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
