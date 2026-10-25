//! Block leases of sender sequences (#1043): the sender persists a high-water lease, not each
//! sequence it hands out, and issues sequences from memory while they stay inside the lease.
//!
//! A sender stream's allocator is a pair `(issued, leased)` in memory, and its record holds one
//! durable bound `D`. [`allocate`] is the pure transition; the reservation transition of
//! `TransactionReplay` stages the lease an allocation extends to into its batch, and returns
//! the range only once that batch is durable (the release law of `commit`, #916):
//!
//! ```text
//! allocate((issued, leased), n) =
//!   first = issued + 1            (0 for a new stream; ⊥ on u64 overflow)
//!   last  = first + n − 1         (⊥ on u64 overflow)
//!   last ≤ leased  ⟹  (first..=last, (last, leased), no write)
//!   otherwise      ⟹  (first..=last, (last, L), write D := L),
//!                     L = max(last, first + TRANSACTION_SEQUENCE_LEASE − 1)
//! restore(D)       = (D, D)
//! ```
//!
//! **Law (no reuse across crashes).** Let `I` be the highest sequence any run of the node has
//! returned for the stream. At every point where the process may stop, `I ≤ D`; a restarted
//! allocator is `restore(D)`, so its first sequence is `D + 1 > I`, and no sequence is issued
//! twice, whatever the crash point. Proof sketch, by induction over transitions, with two
//! invariants over the allocator the released transitions leave (the durable prefix; later
//! transitions run on a speculative allocator that a failed batch undoes, and return nothing
//! before their batch is durable):
//!
//! - *(allocator)* `issued ≤ leased`: an in-lease allocation sets `issued := last ≤ leased`;
//!   an extending one sets `issued := last ≤ L = leased`.
//! - *(durability)* `leased ≤ D`: a reservation is released only once the batch holding its
//!   lease extension, or an earlier one, is durable, and every write stores `L ≥ last >
//!   leased`, so no write, even a failed one that still lands, lowers `D` below `leased`. A
//!   record write is atomic (each store leaves the record whole at its previous or its new
//!   value), so `D` is always one of the written values.
//!
//! So `I = issued ≤ leased ≤ D` over what was released. A crash restores `(D, D)`, which keeps
//! both invariants and `I ≤ D`. Each crash point in turn:
//!
//! | Crash point                         | `D` at the restart | Restart issues from |
//! |-------------------------------------|--------------------|---------------------|
//! | before the lease write              | old `leased ≥ I`   | `leased + 1`        |
//! | after the lease write, before reply | `L ≥ last`         | `L + 1`             |
//! | mid-block (no write since)          | `leased ≥ issued`  | `leased + 1`        |
//! | at the block boundary (`I = D`)     | `D = I`            | `I + 1`             |
//!
//! Before the write, no sequence of the allocation was returned; after it, the range was
//! durably covered before it was returned. A crash after the batch's write and before its
//! release is the second row: the range was covered and never returned. A store lost whole is
//! the documented reset of the module documentation, which no lease scheme can outlive.
//!
//! **Law (a gap is harmless).** A restart skips the sequences `I + 1 ..= D` that the old run
//! leased but never issued. A write stores `L = max(last, first + N − 1)`, with `first` the
//! sequence the run would issue next, and in-lease allocations only raise `issued`, so with
//! `N = TRANSACTION_SEQUENCE_LEASE` each restart skips at most `max(N, n)` sequences past the
//! one its run would have issued next, `n` the count of a reservation whose write landed but
//! which never returned (a crash after its write, or a failed write that landed anyway); for
//! the single reservations a signer makes, at most `N`. Restarts that issue nothing between
//! them add their skips. No transaction ever carries a skipped sequence, since none was signed,
//! so a receiver whose high watermark is `h ≤ I` sees, whatever the gap's size, under the
//! window semantics of [`observe`](super::observe):
//!
//! - the first sequence of the restarted run, `D + 1 > h`, as `Advance` across a normal gap
//!   (a gap of `REORDER_WINDOW` or more clears the window, whose cleared slots fall below the
//!   new retained minimum and stay `Stale`, never re-admitted);
//! - no `Fork` from the sender, since no sequence is signed twice;
//! - `Late` only for a sequence the old run really issued that is still inside the window;
//! - `Stale` only for an old-run frame overtaken by more than the window, which the in-class
//!   order law excludes on one edge and which is then lost as any frame so overtaken.
//!
//! To the receiver a restart is therefore the same as that many cancelled reservations, which
//! the law of whole transitions already allows.

use std::num::NonZeroU64;
use std::ops::RangeInclusive;

/// Sequences one sender lease write covers: a reservation that outruns its stream's lease
/// extends it to at least this many sequences from the reservation's first one.
///
/// Derivation. Safety (no reuse) holds for every lease of at least one sequence, so the size
/// trades only the write rate against the gap a restart leaves:
///
/// - *Write rate.* A stream reserving one sequence per transaction pays one record write per
///   `N` transactions, a commit of about 15 ms on a browser's strict IndexedDB store and
///   17–18 ms on the flushed native store, under the node's one replay lock. The receiver
///   still pays one write per admission, so with `N = 64` a hop's sender share of the lock is
///   `1/64` of its receiver share: about 1.5% of the node's replay writes instead of half.
///   Doubling `N` would save under 0.8% more.
/// - *Gap.* A restart skips at most `N` single sequences of a `u64` space, and the receiver takes
///   the gap as one `Advance` (the gap law of this module): no message is lost to it.
/// - *Window.* `N > REORDER_WINDOW`, so the single reservations that refill one class lane's
///   window (fewer than `REORDER_WINDOW` in flight) cross at most one lease boundary: a window's
///   worth of frames waits on at most one write.
///
/// Measured in the browser onion WebView flow (#1043), a request made about 1,030 reservations
/// and 1,012 admissions, each one write. With leases of 64 the reservations write at most
/// `⌈k_s / 64⌉` times per stream `s`: about 16 writes per request plus one per sender stream
/// that crosses a lease boundary, so a request makes about 1,030 replay writes instead of
/// about 2,040.
pub(crate) const TRANSACTION_SEQUENCE_LEASE: u64 = 64;
const _: () = assert!(TRANSACTION_SEQUENCE_LEASE > super::REORDER_WINDOW);

/// A sender stream's allocator: the last sequence it issued and the last sequence its durable
/// lease covers (the allocator invariant `issued ≤ leased` holds by construction).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SenderLease {
    /// The last sequence handed out.
    issued: u64,
    /// The last sequence the stream's record covers.
    leased: u64,
}

impl SenderLease {
    /// The allocator of a stream restored from its durable lease `leased`: the whole lease
    /// counts as issued, since the record does not say how much of it the last run handed out.
    pub(super) const fn restored(leased: u64) -> Self {
        Self {
            issued: leased,
            leased,
        }
    }

    /// The last sequence the stream's record covers; read only by the native lease model.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) const fn leased(self) -> u64 {
        self.leased
    }
}

/// One allocation of sender sequences: the range handed out, the allocator after it, and the
/// lease to persist before either is adopted, when the range outruns the current lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Allocation {
    /// The sequences handed out.
    pub(super) range: RangeInclusive<u64>,
    /// The allocator once the allocation is adopted.
    pub(super) next: SenderLease,
    /// The lease bound to persist first; `None` when the range lies inside the current lease.
    pub(super) extend: Option<u64>,
}

/// Allocate `count` sequences after `previous` (pure; the transition of the module
/// documentation). `None` when the stream's sequence space is exhausted: the range would
/// wrap `u64`.
pub(super) fn allocate(previous: Option<SenderLease>, count: NonZeroU64) -> Option<Allocation> {
    let first = match previous {
        Some(lease) => lease.issued.checked_add(1)?,
        None => 0,
    };
    let last = first.checked_add(count.get().saturating_sub(1))?;
    let range = first..=last;
    match previous {
        Some(lease) if last <= lease.leased => Some(Allocation {
            range,
            next: SenderLease {
                issued: last,
                leased: lease.leased,
            },
            extend: None,
        }),
        _ => {
            let leased =
                last.max(first.saturating_add(TRANSACTION_SEQUENCE_LEASE.saturating_sub(1)));
            Some(Allocation {
                range,
                next: SenderLease {
                    issued: last,
                    leased,
                },
                extend: Some(leased),
            })
        }
    }
}
