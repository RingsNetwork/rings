//! What one admission presents to the replay runtime, and when its stream is spent
//! (load-bounds paper, Law 2 and Def. 3).

use super::StreamKey;
use super::TransactionDigest;
use crate::consts::MAX_RELAY_INBOX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::fair_admission::Principal;
use crate::fair_admission::Shares;
use crate::message::types::MessageCategory;
use crate::message::OriginQuotaCharge;
use crate::message::OriginQuotaInstant;
use crate::utils::Instant;

/// The shares of the receiver table (load-bounds paper, Law 2): `f = 32` new streams per slot,
/// a pool of 4096, a local share of 1024 for this node's own signaling and an inbox share of
/// 768 for deliveries out of its relay inbox, so `B(s) = R(s)·32 + 5888`: 16 384 at `s = 3`,
/// the table's hard bound
/// ([`TRANSACTION_REPLAY_STREAM_CAPACITY`](super::TRANSACTION_REPLAY_STREAM_CAPACITY)), which
/// binds first past `s = 3`.
pub(crate) const RECEIVER_STREAM_SHARES: Shares = Shares {
    floor: 32,
    pool: 4096,
    local: 1024,
    inbox: 768,
    custody: 0,
};

const _: () = assert!(
    RECEIVER_STREAM_SHARES.bound(crate::dht::topology::edge_slots(
        crate::dht::topology::DEFAULT_SUCCESSOR_CAPACITY
    )) == super::TRANSACTION_REPLAY_STREAM_CAPACITY
);

/// `ι(class)`: how long after its proof expires a transaction of `class` may still be delivered.
/// Application mail may be held in a relay inbox and delivered as of its hold instant, at most
/// [`MAX_RELAY_INBOX_TTL_MS`] after a hold no later than its sender proof (by the holder's
/// clock, within `σ` of this one); every other class is delivered only while its proof is live.
const fn delivery_horizon_ms(class: MessageCategory) -> u128 {
    match class {
        MessageCategory::Application => {
            (MAX_RELAY_INBOX_TTL_MS as u128).saturating_add(TS_OFFSET_TOLERANCE_MS)
        }
        MessageCategory::DhtControl | MessageCategory::Storage | MessageCategory::E2e => 0,
    }
}

/// The instant a receiver stream holding a transaction of `class` signed at `ts_ms` for
/// `ttl_ms` may be spent (load-bounds paper, Def. 3): `ts + ttl + ι(class) + σ`.
///
/// Law (eviction is sound, Lemma 2): at any `t ≥ spent_at`, by a clock up to `σ` behind, the
/// transaction can no longer be delivered, live or out of a relay inbox, so forgetting the
/// stream reopens no replay of it.
pub(crate) fn spent_at(class: MessageCategory, ts_ms: u128, ttl_ms: u64) -> u128 {
    ts_ms
        .saturating_add(u128::from(ttl_ms))
        .saturating_add(delivery_horizon_ms(class))
        .saturating_add(TS_OFFSET_TOLERANCE_MS)
}

/// One reading of the replay clock, taken under the replay lock.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReplayInstant {
    /// The runtime-local quota instant: monotonic time since the runtime started.
    pub(super) quota: OriginQuotaInstant,
    /// The wall clock, which spent instants are judged by.
    pub(super) wall_ms: u128,
}

impl ReplayInstant {
    /// Test builds: the quota instant `quota`, at the wall clock now.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn quota_at(quota: OriginQuotaInstant) -> Self {
        Self {
            quota,
            wall_ms: crate::utils::get_epoch_ms(),
        }
    }
}

/// The replay clock: monotonic time elapsed since `started_at` and the wall clock, read when
/// called.
pub(super) fn replay_clock(started_at: Instant) -> impl FnOnce() -> ReplayInstant {
    move || ReplayInstant {
        quota: OriginQuotaInstant::from_nanos(
            Instant::now()
                .saturating_duration_since(started_at)
                .as_nanos(),
        ),
        wall_ms: crate::utils::get_epoch_ms(),
    }
}

/// One transaction presented for admission at its destination.
pub(crate) struct Admission {
    /// Its stream.
    pub(crate) key: StreamKey,
    /// Its sequence in the stream.
    pub(crate) sequence: u64,
    /// Its digest.
    pub(crate) digest: TransactionDigest,
    /// The quota charge of its class.
    pub(crate) charge: OriginQuotaCharge,
    /// Its byte cost against the origin's quota.
    pub(crate) byte_cost: usize,
    /// The principal that delivered it.
    pub(crate) introducer: Principal,
    /// The instant its stream may be spent, as far as it is concerned ([`spent_at`]).
    pub(crate) spent_at_ms: u128,
}

impl Admission {
    /// Test builds: an application transaction of `key`, introduced by this node, costing
    /// nothing, whose stream is spent a default lifetime after `now_ms`.
    #[cfg(test)]
    pub(crate) fn for_test(
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
        now_ms: u128,
    ) -> Self {
        Self {
            key,
            sequence,
            digest,
            charge: MessageCategory::Application.into(),
            byte_cost: 0,
            introducer: Principal::Local,
            spent_at_ms: spent_at(key.class, now_ms, crate::consts::DEFAULT_TTL_MS),
        }
    }
}
