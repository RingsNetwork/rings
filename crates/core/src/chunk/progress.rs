//! The rate floor of an open message (load-bounds paper, Def. 7 and Law 4, #932).
//!
//! A message holds its whole reservation while it is open, so how long it may stay open is
//! bounded by what its sender streams. The receiver samples the message at instants
//! `t₀ < t₁ < …` (its chunks' arrivals and the reassembler's sweeps) and reads at each the credit
//! `c` its sender holds unused on the carrier: advertised by this end and not yet used. Credit
//! the receiver holds back (the frames that took it, a release awaiting its batch, an
//! advertisement deferred at the node's soft limit) is not in `c`, so the clock pauses while the
//! receiver throttles the sender. Read at the receiver, `c` also counts credit still in transit
//! to the sender and frames already sent on it that are still in flight: with the frames the
//! receiver holds, that is the lane's outstanding liability, at most one window `W`, however
//! long the round trip. With `ρ` the rate floor, `d₀` the message's chunk length, `W`
//! the lane window and `b` the bytes received:
//!
//! ```text
//! owed  += min(ρ·(tₖ₊₁ − tₖ), cₖ·d₀)          (ρ·τ̂, its credit-time at the rate floor)
//! stalled ⟺ b + W·d₀ < owed                   (b < ρ·τ̂ − W·d₀)
//! ```
//!
//! Laws: `owed` is monotone and runs ahead of what a sender delivering `ρ` while it holds credit
//! has delivered by at most the lane's outstanding liability, `W·d₀`, which the grace covers, so
//! such a sender is never stalled, however long its round trip (Prop. 4); a sender that holds
//! credit and sends nothing is stalled once `(W + 1)·d₀ < ρ·τ̂` (Prop. 5).

/// The credit-time ledger of one open message (module documentation).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Progress {
    /// `b`: the message's bytes received.
    received: u64,
    /// `ρ·τ̂`: the bytes the message's credit-time owes at the rate floor.
    owed: u64,
    /// The instant of the last sample, in epoch milliseconds.
    sampled_ms: u128,
    /// `c` at the last sample: the credit its sender held unused on the carrier, in frames.
    credit: u64,
}

/// The parameters of the rate floor for one message: `ρ` in bytes per millisecond, `d₀` in
/// bytes, and the grace `W` in chunks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RateFloor {
    /// `ρ`, in bytes per millisecond.
    pub(super) rate: u64,
    /// `d₀`, the message's chunk length.
    pub(super) chunk: u64,
    /// `W`, the chunks a receiver may hold unprocessed while their sender's credit is spent.
    pub(super) grace_chunks: u64,
}

impl Progress {
    /// A message opened at `now_ms` by a first chunk of `received` bytes, its sender then
    /// holding `credit` unused.
    pub(super) const fn open(now_ms: u128, credit: u64, received: u64) -> Self {
        Self {
            received,
            owed: 0,
            sampled_ms: now_ms,
            credit,
        }
    }

    /// Advance the credit-time to `now_ms` by the credit last sampled, then sample `credit`.
    /// An instant before the last sample advances nothing.
    pub(super) fn observe(&mut self, now_ms: u128, credit: u64, floor: RateFloor) {
        let elapsed = u64::try_from(now_ms.saturating_sub(self.sampled_ms)).unwrap_or(u64::MAX);
        let advance = floor
            .rate
            .saturating_mul(elapsed)
            .min(self.credit.saturating_mul(floor.chunk));
        self.owed = self.owed.saturating_add(advance);
        self.sampled_ms = self.sampled_ms.max(now_ms);
        self.credit = credit;
    }

    /// Count `bytes` more received.
    pub(super) fn receive(&mut self, bytes: u64) {
        self.received = self.received.saturating_add(bytes);
    }

    /// Whether the message fell below the rate floor: `b < ρ·τ̂ − W·d₀`.
    pub(super) const fn stalled(&self, floor: RateFloor) -> bool {
        self.received
            .saturating_add(floor.grace_chunks.saturating_mul(floor.chunk))
            < self.owed
    }
}
