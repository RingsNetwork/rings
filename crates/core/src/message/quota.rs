//! Runtime-local origin quotas for final-destination transactions.
//!
//! A quota key is `(network_id, origin account DID, destination DID, logical lane)`. Each key
//! owns independent fixed-point message and byte token buckets. [`OriginQuota::admit`] is the
//! pure transition; the destination replay runtime owns the bounded table and supplies monotonic
//! time. Quota records are intentionally absent from the durable replay snapshot.
//!
//! A logical lane is either a message class under the configured [`OriginQuotaConfig`], or a
//! paced direct-edge lane whose message rate its owning protocol supplied (see
//! [`OriginQuotaLane`]). Both resolve to one [`OriginQuotaLimits`] before any transition.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::message::paced_lane::OriginQuotaLane;
use crate::message::paced_lane::OriginQuotaLaneId;
use crate::message::paced_lane::PacedRate;
use crate::message::types::MessageCategory;

const NANOS_PER_SECOND: u128 = 1_000_000_000;
/// Default message rate admitted for one origin in one logical lane.
pub const DEFAULT_ORIGIN_QUOTA_MESSAGES_PER_SECOND: u64 = 8;
/// Default instantaneous message allowance for one origin in one logical lane.
pub const DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST: u64 = 32;
/// Default byte rate admitted for one origin in one logical lane.
pub const DEFAULT_ORIGIN_QUOTA_BYTES_PER_SECOND: u64 = 4 * 1024 * 1024;
/// Default instantaneous byte allowance for one origin in one logical lane.
///
/// This exceeds the 60 MB logical-message protocol ceiling, so every valid message can fit an
/// otherwise full bucket.
pub const DEFAULT_ORIGIN_QUOTA_BYTE_BURST: u64 = 64 * 1024 * 1024;
/// Default number of runtime-local origin records retained per logical lane.
pub const DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE: usize = 1024;

/// Validated token-bucket and record-table limits for one logical lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(into = "OriginQuotaLaneConfigWire")]
pub struct OriginQuotaLaneConfig {
    message_rate_per_second: u64,
    message_burst: u64,
    byte_rate_per_second: u64,
    byte_burst: u64,
    max_records: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct OriginQuotaLaneConfigWire {
    message_rate_per_second: u64,
    message_burst: u64,
    byte_rate_per_second: u64,
    byte_burst: u64,
    max_records: usize,
}

impl OriginQuotaLaneConfig {
    /// Validate and construct one lane's token-bucket and table limits.
    pub fn new(
        message_rate_per_second: u64,
        message_burst: u64,
        byte_rate_per_second: u64,
        byte_burst: u64,
        max_records: usize,
    ) -> std::result::Result<Self, OriginQuotaConfigError> {
        if message_rate_per_second == 0 {
            return Err(OriginQuotaConfigError::ZeroMessageRate);
        }
        if message_burst == 0 {
            return Err(OriginQuotaConfigError::ZeroMessageBurst);
        }
        if byte_rate_per_second == 0 {
            return Err(OriginQuotaConfigError::ZeroByteRate);
        }
        if byte_burst == 0 {
            return Err(OriginQuotaConfigError::ZeroByteBurst);
        }
        if max_records == 0 {
            return Err(OriginQuotaConfigError::ZeroRecordCapacity);
        }
        Ok(Self {
            message_rate_per_second,
            message_burst,
            byte_rate_per_second,
            byte_burst,
            max_records,
        })
    }

    /// Messages replenished per second.
    pub const fn message_rate_per_second(self) -> u64 {
        self.message_rate_per_second
    }

    /// Maximum accumulated message tokens.
    pub const fn message_burst(self) -> u64 {
        self.message_burst
    }

    /// Bytes replenished per second.
    pub const fn byte_rate_per_second(self) -> u64 {
        self.byte_rate_per_second
    }

    /// Maximum accumulated byte tokens.
    pub const fn byte_burst(self) -> u64 {
        self.byte_burst
    }

    /// Maximum runtime-local records retained for this lane.
    pub const fn max_records(self) -> usize {
        self.max_records
    }
}

impl Default for OriginQuotaLaneConfig {
    fn default() -> Self {
        Self {
            message_rate_per_second: DEFAULT_ORIGIN_QUOTA_MESSAGES_PER_SECOND,
            message_burst: DEFAULT_ORIGIN_QUOTA_MESSAGE_BURST,
            byte_rate_per_second: DEFAULT_ORIGIN_QUOTA_BYTES_PER_SECOND,
            byte_burst: DEFAULT_ORIGIN_QUOTA_BYTE_BURST,
            max_records: DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE,
        }
    }
}

impl From<OriginQuotaLaneConfig> for OriginQuotaLaneConfigWire {
    fn from(value: OriginQuotaLaneConfig) -> Self {
        Self {
            message_rate_per_second: value.message_rate_per_second,
            message_burst: value.message_burst,
            byte_rate_per_second: value.byte_rate_per_second,
            byte_burst: value.byte_burst,
            max_records: value.max_records,
        }
    }
}

impl TryFrom<OriginQuotaLaneConfigWire> for OriginQuotaLaneConfig {
    type Error = OriginQuotaConfigError;

    fn try_from(value: OriginQuotaLaneConfigWire) -> Result<Self, Self::Error> {
        Self::new(
            value.message_rate_per_second,
            value.message_burst,
            value.byte_rate_per_second,
            value.byte_burst,
            value.max_records,
        )
    }
}

impl<'de> Deserialize<'de> for OriginQuotaLaneConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        OriginQuotaLaneConfigWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// Per-lane final-destination quota configuration.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OriginQuotaConfig {
    /// Chord control-lane limits.
    pub dht_control: OriginQuotaLaneConfig,
    /// Storage-lane limits.
    pub storage: OriginQuotaLaneConfig,
    /// End-to-end protocol-lane limits.
    pub e2e: OriginQuotaLaneConfig,
    /// Application-lane limits.
    pub application: OriginQuotaLaneConfig,
}

impl OriginQuotaConfig {
    /// Construct a validated per-lane configuration.
    pub const fn new(
        dht_control: OriginQuotaLaneConfig,
        storage: OriginQuotaLaneConfig,
        e2e: OriginQuotaLaneConfig,
        application: OriginQuotaLaneConfig,
    ) -> Self {
        Self {
            dht_control,
            storage,
            e2e,
            application,
        }
    }

    /// Return the limits for `lane`.
    pub const fn lane(self, lane: MessageCategory) -> OriginQuotaLaneConfig {
        match lane {
            MessageCategory::DhtControl => self.dht_control,
            MessageCategory::Storage => self.storage,
            MessageCategory::E2e => self.e2e,
            MessageCategory::Application => self.application,
        }
    }

    /// Resolve the token-bucket limits of `lane`.
    ///
    /// A paced lane replaces only the message dimension with the supplied rate. Its byte
    /// dimension and record bound stay the Application lane's, so pacing never widens the byte
    /// bound of application traffic.
    pub fn limits(self, lane: OriginQuotaLane) -> OriginQuotaLimits {
        match lane {
            OriginQuotaLane::Class(class) => self.lane(class).into(),
            OriginQuotaLane::Paced(paced) => {
                OriginQuotaLimits::paced(paced.rate(), self.application)
            }
        }
    }
}

impl Default for OriginQuotaConfig {
    fn default() -> Self {
        let lane = OriginQuotaLaneConfig::default();
        Self::new(lane, lane, lane, lane)
    }
}

/// Why a lane quota configuration is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OriginQuotaConfigError {
    /// A zero rate could never replenish a message bucket.
    #[error("origin quota message rate must be non-zero")]
    ZeroMessageRate,
    /// A zero burst could never admit a message.
    #[error("origin quota message burst must be non-zero")]
    ZeroMessageBurst,
    /// A zero rate could never replenish a byte bucket.
    #[error("origin quota byte rate must be non-zero")]
    ZeroByteRate,
    /// A zero burst could never admit message bytes.
    #[error("origin quota byte burst must be non-zero")]
    ZeroByteBurst,
    /// A zero bound could retain no origin record.
    #[error("origin quota record capacity must be non-zero")]
    ZeroRecordCapacity,
}

/// Tokens replenished per period: `amount` tokens every `period_seconds` seconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RefillRate {
    /// Tokens replenished per period.
    amount: u64,
    /// Length of one period in seconds.
    period_seconds: NonZeroU64,
}

impl RefillRate {
    /// `amount` tokens every second.
    const fn per_second(amount: u64) -> Self {
        Self {
            amount,
            period_seconds: NonZeroU64::MIN,
        }
    }

    /// Scaled tokens (`token × 10⁹`) replenished over `elapsed_nanos`, saturating.
    ///
    /// `elapsed · amount / period` rounds down, so a record never gains a token early.
    fn replenished(self, elapsed_nanos: u128) -> u128 {
        elapsed_nanos.saturating_mul(u128::from(self.amount))
            / u128::from(self.period_seconds.get())
    }
}

/// Token-bucket limits of one quota record, resolved from its lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OriginQuotaLimits {
    /// Message-token refill rate.
    message_rate: RefillRate,
    /// Maximum accumulated message tokens.
    message_burst: u64,
    /// Byte-token refill rate.
    byte_rate: RefillRate,
    /// Maximum accumulated byte tokens.
    byte_burst: u64,
    /// Maximum runtime-local records retained for this lane.
    max_records: usize,
}

impl OriginQuotaLimits {
    /// The limits of a paced lane: `rate` for messages, `base` for bytes and records.
    ///
    /// The burst equals the rate's budget, so the bucket enforces the same bound as a window
    /// admission of `budget` messages per period.
    fn paced(rate: PacedRate, base: OriginQuotaLaneConfig) -> Self {
        Self {
            message_rate: RefillRate {
                amount: rate.budget().get(),
                period_seconds: rate.period_seconds(),
            },
            message_burst: rate.budget().get(),
            ..base.into()
        }
    }
}

impl From<OriginQuotaLaneConfig> for OriginQuotaLimits {
    fn from(config: OriginQuotaLaneConfig) -> Self {
        Self {
            message_rate: RefillRate::per_second(config.message_rate_per_second),
            message_burst: config.message_burst,
            byte_rate: RefillRate::per_second(config.byte_rate_per_second),
            byte_burst: config.byte_burst,
            max_records: config.max_records,
        }
    }
}

/// Monotonic runtime-local instant used by the pure quota transition.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct OriginQuotaInstant(u128);

impl OriginQuotaInstant {
    /// The runtime start instant.
    pub const ZERO: Self = Self(0);

    /// Construct an instant from elapsed monotonic nanoseconds.
    pub const fn from_nanos(nanos: u128) -> Self {
        Self(nanos)
    }
}

/// Identity of one runtime-local origin quota record.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OriginQuotaKey {
    /// Overlay in which the transaction signature is valid.
    pub network_id: u32,
    /// Account DID recovered from the transaction signature.
    pub origin_account: Did,
    /// Final logical destination.
    pub destination: Did,
    /// Identity of the logical quota lane selected from the message and its delivering edge.
    pub lane: OriginQuotaLaneId,
}

impl OriginQuotaKey {
    /// Name one origin-to-destination lane inside an overlay.
    pub const fn new(
        network_id: u32,
        origin_account: Did,
        destination: Did,
        lane: OriginQuotaLaneId,
    ) -> Self {
        Self {
            network_id,
            origin_account,
            destination,
            lane,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TokenBucket {
    scaled_tokens: u128,
}

impl TokenBucket {
    fn full(burst: u64) -> Self {
        Self {
            scaled_tokens: u128::from(burst) * NANOS_PER_SECOND,
        }
    }

    fn refill(self, rate: RefillRate, burst: u64, elapsed_nanos: u128) -> Self {
        let capacity = u128::from(burst) * NANOS_PER_SECOND;
        let replenished = rate.replenished(elapsed_nanos);
        Self {
            scaled_tokens: self.scaled_tokens.saturating_add(replenished).min(capacity),
        }
    }

    fn has(self, scaled_cost: u128) -> bool {
        self.scaled_tokens >= scaled_cost
    }

    fn consume(&mut self, scaled_cost: u128) {
        self.scaled_tokens = self.scaled_tokens.saturating_sub(scaled_cost);
    }

    fn is_full(self, burst: u64) -> bool {
        self.scaled_tokens == u128::from(burst) * NANOS_PER_SECOND
    }
}

/// Pure message and byte token-bucket state for one origin quota key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OriginQuota {
    messages: TokenBucket,
    bytes: TokenBucket,
    last_refill: OriginQuotaInstant,
}

impl OriginQuota {
    /// Construct a fully replenished record at `now`.
    pub fn full(config: impl Into<OriginQuotaLimits>, now: OriginQuotaInstant) -> Self {
        let config = config.into();
        Self {
            messages: TokenBucket::full(config.message_burst),
            bytes: TokenBucket::full(config.byte_burst),
            last_refill: now,
        }
    }

    /// Apply one deterministic logical-message admission transition.
    ///
    /// Rejected transitions retain any refill but consume neither dimension. The byte cost is the
    /// verified transaction's serialized logical `data` length; chunk-envelope sizes never enter
    /// this transition.
    pub fn admit(
        self,
        config: impl Into<OriginQuotaLimits>,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<(Self, OriginQuotaVerdict), OriginQuotaArithmeticError> {
        let config = config.into();
        let mut next = self.refilled(config, now)?;
        if !next.messages.has(NANOS_PER_SECOND) {
            return Ok((next, OriginQuotaVerdict::MessageRateExhausted));
        }
        let byte_cost =
            u128::try_from(byte_cost).map_err(|_| OriginQuotaArithmeticError::ByteCostOverflow)?;
        let scaled_byte_cost = byte_cost
            .checked_mul(NANOS_PER_SECOND)
            .ok_or(OriginQuotaArithmeticError::ByteCostOverflow)?;
        if !next.bytes.has(scaled_byte_cost) {
            return Ok((next, OriginQuotaVerdict::ByteRateExhausted));
        }
        next.messages.consume(NANOS_PER_SECOND);
        next.bytes.consume(scaled_byte_cost);
        Ok((next, OriginQuotaVerdict::Admitted))
    }

    fn refilled(
        self,
        config: OriginQuotaLimits,
        now: OriginQuotaInstant,
    ) -> Result<Self, OriginQuotaArithmeticError> {
        let elapsed = now
            .0
            .checked_sub(self.last_refill.0)
            .ok_or(OriginQuotaArithmeticError::MonotonicTimeRegressed)?;
        Ok(Self {
            messages: self
                .messages
                .refill(config.message_rate, config.message_burst, elapsed),
            bytes: self
                .bytes
                .refill(config.byte_rate, config.byte_burst, elapsed),
            last_refill: now,
        })
    }

    fn fully_replenished_at(
        self,
        config: OriginQuotaLimits,
        now: OriginQuotaInstant,
    ) -> Result<bool, OriginQuotaArithmeticError> {
        let replenished = self.refilled(config, now)?;
        Ok(replenished.messages.is_full(config.message_burst)
            && replenished.bytes.is_full(config.byte_burst))
    }

    #[cfg(test)]
    fn whole_message_tokens(self) -> u128 {
        self.messages.scaled_tokens / NANOS_PER_SECOND
    }

    #[cfg(test)]
    fn whole_byte_tokens(self) -> u128 {
        self.bytes.scaled_tokens / NANOS_PER_SECOND
    }
}

/// Typed result of applying an origin quota decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginQuotaVerdict {
    /// Both token dimensions admitted and consumed the logical message.
    Admitted,
    /// The message token bucket lacks one complete token.
    MessageRateExhausted,
    /// The byte token bucket lacks the complete deterministic logical-message cost.
    ByteRateExhausted,
}

impl OriginQuotaVerdict {
    fn rejection(self) -> Option<OriginQuotaRejection> {
        match self {
            Self::Admitted => None,
            Self::MessageRateExhausted => Some(OriginQuotaRejection::MessageRate),
            Self::ByteRateExhausted => Some(OriginQuotaRejection::ByteRate),
        }
    }
}

/// Arithmetic failure in the pure quota transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OriginQuotaArithmeticError {
    /// The supplied monotonic instant preceded the record's last refill.
    #[error("origin quota monotonic time regressed")]
    MonotonicTimeRegressed,
    /// The deterministic logical-message byte cost could not be represented safely.
    #[error("origin quota byte cost overflowed fixed-point arithmetic")]
    ByteCostOverflow,
}

/// Failure at the final-destination origin-quota boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OriginQuotaError {
    /// A verified origin exhausted its message-rate allowance.
    #[error("Origin quota message rate exhausted for {key:?}")]
    MessageRateExhausted {
        /// Origin, destination, overlay, and logical lane sharing the allowance.
        key: OriginQuotaKey,
    },
    /// A verified origin exhausted its byte-rate allowance.
    #[error(
        "Origin quota byte rate exhausted for {key:?} while admitting {requested_bytes} bytes"
    )]
    ByteRateExhausted {
        /// Origin, destination, overlay, and logical lane sharing the allowance.
        key: OriginQuotaKey,
        /// Deterministic logical-message bytes requested.
        requested_bytes: usize,
    },
    /// A lane's bounded origin table had no fully replenished record safe to reuse.
    #[error("Origin quota table for {lane:?} exhausted its {capacity} records")]
    TableCapacityExhausted {
        /// Logical lane whose record bound was reached.
        lane: OriginQuotaLaneId,
        /// Maximum retained records for that lane.
        capacity: usize,
    },
    /// The pure origin-quota transition could not be evaluated safely.
    #[error("Origin quota arithmetic failed for {key:?}: {source}")]
    Arithmetic {
        /// Origin, destination, overlay, and logical lane being evaluated.
        key: OriginQuotaKey,
        /// Typed arithmetic or monotonic-clock failure.
        #[source]
        source: OriginQuotaArithmeticError,
    },
}

#[derive(Debug)]
pub(super) enum OriginQuotaAdmissionError {
    Verdict(OriginQuotaRejection),
    Arithmetic(OriginQuotaArithmeticError),
}

#[derive(Debug)]
pub(super) enum OriginQuotaRejection {
    MessageRate,
    ByteRate,
    Capacity { capacity: usize },
}

#[derive(Debug)]
pub(super) struct OriginQuotaReservation {
    key: OriginQuotaKey,
    previous: Option<OriginQuota>,
    evicted: Option<(OriginQuotaKey, OriginQuota)>,
}

impl OriginQuotaReservation {
    pub(super) fn rollback(self, table: &mut OriginQuotaTable) {
        match self.previous {
            Some(previous) => {
                table.records.insert(self.key, previous);
            }
            None => {
                table.records.remove(&self.key);
            }
        }
        if let Some((key, quota)) = self.evicted {
            table.records.insert(key, quota);
        }
    }
}

pub(super) struct OriginQuotaTable {
    config: OriginQuotaConfig,
    records: BTreeMap<OriginQuotaKey, OriginQuota>,
}

impl OriginQuotaTable {
    pub(super) fn new(config: OriginQuotaConfig) -> Self {
        Self {
            config,
            records: BTreeMap::new(),
        }
    }

    /// The limits of `lane` under this table's configuration.
    pub(super) fn limits(&self, lane: OriginQuotaLane) -> OriginQuotaLimits {
        self.config.limits(lane)
    }

    /// Reserve one admission of `byte_cost` for `key` under `lane_config`, the limits of the
    /// lane `key.lane` names.
    pub(super) fn reserve(
        &mut self,
        key: OriginQuotaKey,
        lane_config: OriginQuotaLimits,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<OriginQuotaReservation, OriginQuotaAdmissionError> {
        if let Some(previous) = self.records.get(&key).copied() {
            let (next, verdict) = previous
                .admit(lane_config, byte_cost, now)
                .map_err(OriginQuotaAdmissionError::Arithmetic)?;
            self.records.insert(key, next);
            if let Some(rejection) = verdict.rejection() {
                return Err(OriginQuotaAdmissionError::Verdict(rejection));
            }
            return Ok(OriginQuotaReservation {
                key,
                previous: Some(previous),
                evicted: None,
            });
        }

        let (next, verdict) = OriginQuota::full(lane_config, now)
            .admit(lane_config, byte_cost, now)
            .map_err(OriginQuotaAdmissionError::Arithmetic)?;
        if let Some(rejection) = verdict.rejection() {
            return Err(OriginQuotaAdmissionError::Verdict(rejection));
        }

        let lane_records = self
            .records
            .keys()
            .filter(|candidate| candidate.lane == key.lane)
            .count();
        let evicted = if lane_records >= lane_config.max_records {
            let victim = self.safe_victim(key.lane, lane_config, now)?;
            let Some(victim_key) = victim else {
                return Err(OriginQuotaAdmissionError::Verdict(
                    OriginQuotaRejection::Capacity {
                        capacity: lane_config.max_records,
                    },
                ));
            };
            self.records
                .remove(&victim_key)
                .map(|quota| (victim_key, quota))
        } else {
            None
        };
        self.records.insert(key, next);
        Ok(OriginQuotaReservation {
            key,
            previous: None,
            evicted,
        })
    }

    fn safe_victim(
        &self,
        lane: OriginQuotaLaneId,
        config: OriginQuotaLimits,
        now: OriginQuotaInstant,
    ) -> Result<Option<OriginQuotaKey>, OriginQuotaAdmissionError> {
        let mut victim = None;
        for (key, quota) in self.records.iter().filter(|(key, _)| key.lane == lane) {
            if !quota
                .fully_replenished_at(config, now)
                .map_err(OriginQuotaAdmissionError::Arithmetic)?
            {
                continue;
            }
            let candidate = (quota.last_refill, *key);
            if victim.is_none_or(|current| candidate < current) {
                victim = Some(candidate);
            }
        }
        Ok(victim.map(|(_, key)| key))
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.records.len()
    }

    /// The lanes holding a record of `origin`, in key order.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn lanes_of(&self, origin: Did) -> Vec<OriginQuotaLaneId> {
        self.records
            .keys()
            .filter(|key| key.origin_account == origin)
            .map(|key| key.lane)
            .collect()
    }

    #[cfg(test)]
    pub(super) fn get(&self, key: OriginQuotaKey) -> Option<OriginQuota> {
        self.records.get(&key).copied()
    }
}

/// Aggregate quota drop counters for one logical lane.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OriginQuotaLaneCounters {
    /// Drops caused by message-token exhaustion.
    pub message_rate_exhausted: u64,
    /// Drops caused by byte-token exhaustion.
    pub byte_rate_exhausted: u64,
    /// Drops caused by the bounded table having no safe victim.
    pub capacity_exhausted: u64,
    /// Drops caused by arithmetic or monotonic-clock failure.
    pub arithmetic_failure: u64,
}

/// Aggregate quota drop counters partitioned only by bounded logical lane.
///
/// Every paced lane shares one aggregate, so the counter set stays bounded however many paced
/// lanes the application layer registers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OriginQuotaCounters {
    /// Class lanes in [`MessageCategory`] order.
    lanes: [OriginQuotaLaneCounters; 4],
    /// All paced direct-edge lanes together.
    paced: OriginQuotaLaneCounters,
}

impl OriginQuotaCounters {
    /// Return aggregate drop counters for the class lane `lane`.
    pub const fn lane(self, lane: MessageCategory) -> OriginQuotaLaneCounters {
        let [dht_control, storage, e2e, application] = self.lanes;
        match lane {
            MessageCategory::DhtControl => dht_control,
            MessageCategory::Storage => storage,
            MessageCategory::E2e => e2e,
            MessageCategory::Application => application,
        }
    }

    /// Return aggregate drop counters of every paced direct-edge lane.
    pub const fn paced(self) -> OriginQuotaLaneCounters {
        self.paced
    }
}

#[derive(Default)]
struct OriginQuotaLaneCounterState {
    message_rate_exhausted: AtomicU64,
    byte_rate_exhausted: AtomicU64,
    capacity_exhausted: AtomicU64,
    arithmetic_failure: AtomicU64,
}

impl OriginQuotaLaneCounterState {
    fn snapshot(&self) -> OriginQuotaLaneCounters {
        OriginQuotaLaneCounters {
            message_rate_exhausted: self.message_rate_exhausted.load(Ordering::Relaxed),
            byte_rate_exhausted: self.byte_rate_exhausted.load(Ordering::Relaxed),
            capacity_exhausted: self.capacity_exhausted.load(Ordering::Relaxed),
            arithmetic_failure: self.arithmetic_failure.load(Ordering::Relaxed),
        }
    }
}

pub(super) struct OriginQuotaCounterState {
    lanes: [OriginQuotaLaneCounterState; 4],
    paced: OriginQuotaLaneCounterState,
}

impl Default for OriginQuotaCounterState {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| OriginQuotaLaneCounterState::default()),
            paced: OriginQuotaLaneCounterState::default(),
        }
    }
}

impl OriginQuotaCounterState {
    pub(super) fn snapshot(&self) -> OriginQuotaCounters {
        OriginQuotaCounters {
            lanes: self
                .lanes
                .each_ref()
                .map(OriginQuotaLaneCounterState::snapshot),
            paced: self.paced.snapshot(),
        }
    }

    pub(super) fn record(&self, lane: OriginQuotaLaneId, error: &OriginQuotaAdmissionError) {
        let [dht_control, storage, e2e, application] = &self.lanes;
        let counters = match lane {
            OriginQuotaLaneId::Class(MessageCategory::DhtControl) => dht_control,
            OriginQuotaLaneId::Class(MessageCategory::Storage) => storage,
            OriginQuotaLaneId::Class(MessageCategory::E2e) => e2e,
            OriginQuotaLaneId::Class(MessageCategory::Application) => application,
            OriginQuotaLaneId::Paced(_) => &self.paced,
        };
        match error {
            OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::MessageRate) => {
                counters
                    .message_rate_exhausted
                    .fetch_add(1, Ordering::Relaxed);
            }
            OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::ByteRate) => {
                counters.byte_rate_exhausted.fetch_add(1, Ordering::Relaxed);
            }
            OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::Capacity { .. }) => {
                counters.capacity_exhausted.fetch_add(1, Ordering::Relaxed);
            }
            OriginQuotaAdmissionError::Arithmetic(_) => {
                counters.arithmetic_failure.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

pub(super) fn quota_admission_error(
    key: OriginQuotaKey,
    byte_cost: usize,
    error: OriginQuotaAdmissionError,
) -> crate::error::Error {
    match error {
        OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::MessageRate) => {
            OriginQuotaError::MessageRateExhausted { key }.into()
        }
        OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::ByteRate) => {
            OriginQuotaError::ByteRateExhausted {
                key,
                requested_bytes: byte_cost,
            }
            .into()
        }
        OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::Capacity { capacity }) => {
            OriginQuotaError::TableCapacityExhausted {
                lane: key.lane,
                capacity,
            }
            .into()
        }
        OriginQuotaAdmissionError::Arithmetic(source) => {
            OriginQuotaError::Arithmetic { key, source }.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::paced_lane::PacedLane;
    use crate::message::paced_lane::PacedLaneId;

    fn config(
        message_rate: u64,
        message_burst: u64,
        byte_rate: u64,
        byte_burst: u64,
        max_records: usize,
    ) -> OriginQuotaLaneConfig {
        OriginQuotaLaneConfig::new(
            message_rate,
            message_burst,
            byte_rate,
            byte_burst,
            max_records,
        )
        .expect("test quota configuration is valid")
    }

    fn key(origin: u32, lane: impl Into<OriginQuotaLane>) -> OriginQuotaKey {
        OriginQuotaKey::new(7, Did::from(origin), Did::from(99_u32), lane.into().id())
    }

    /// Reserve one admission of `byte_cost` for `origin` in `lane` under `table`'s limits.
    fn reserve(
        table: &mut OriginQuotaTable,
        origin: u32,
        lane: impl Into<OriginQuotaLane>,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<OriginQuotaReservation, OriginQuotaAdmissionError> {
        let lane = lane.into();
        let limits = table.limits(lane);
        table.reserve(key(origin, lane), limits, byte_cost, now)
    }

    #[test]
    fn message_and_byte_refill_preserve_fractional_tokens() {
        let config = config(2, 2, 10, 10, 4);
        let start = OriginQuota::full(config, OriginQuotaInstant::ZERO);
        let (depleted, admitted) = start
            .admit(config, 10, OriginQuotaInstant::ZERO)
            .expect("first transition is valid");
        assert_eq!(admitted, OriginQuotaVerdict::Admitted);

        let half_second = OriginQuotaInstant::from_nanos(NANOS_PER_SECOND / 2);
        let (refilled, admitted) = depleted
            .admit(config, 5, half_second)
            .expect("refill transition is valid");
        assert_eq!(admitted, OriginQuotaVerdict::Admitted);
        assert_eq!(refilled.whole_message_tokens(), 1);
        assert_eq!(refilled.whole_byte_tokens(), 0);
    }

    #[test]
    fn burst_edges_and_zero_elapsed_time_are_exact() {
        let config = config(1, 2, 4, 8, 4);
        let quota = OriginQuota::full(config, OriginQuotaInstant::ZERO);
        let (quota, first) = quota
            .admit(config, 4, OriginQuotaInstant::ZERO)
            .expect("first transition is valid");
        let (quota, second) = quota
            .admit(config, 4, OriginQuotaInstant::ZERO)
            .expect("second transition is valid");
        let (quota, exhausted) = quota
            .admit(config, 0, OriginQuotaInstant::ZERO)
            .expect("zero elapsed transition is valid");

        assert_eq!(first, OriginQuotaVerdict::Admitted);
        assert_eq!(second, OriginQuotaVerdict::Admitted);
        assert_eq!(exhausted, OriginQuotaVerdict::MessageRateExhausted);
        assert_eq!(quota.whole_message_tokens(), 0);
        assert_eq!(quota.whole_byte_tokens(), 0);
    }

    #[test]
    fn byte_exhaustion_is_distinct_and_does_not_consume_message_tokens() {
        let config = config(1, 3, 1, 8, 4);
        let quota = OriginQuota::full(config, OriginQuotaInstant::ZERO);
        let (quota, first) = quota
            .admit(config, 8, OriginQuotaInstant::ZERO)
            .expect("first transition is valid");
        let (quota, exhausted) = quota
            .admit(config, 1, OriginQuotaInstant::ZERO)
            .expect("byte rejection is valid");

        assert_eq!(first, OriginQuotaVerdict::Admitted);
        assert_eq!(exhausted, OriginQuotaVerdict::ByteRateExhausted);
        assert_eq!(quota.whole_message_tokens(), 2);
        assert_eq!(quota.whole_byte_tokens(), 0);
    }

    #[test]
    fn transition_is_deterministic_and_rejects_regressed_time() {
        let config = config(3, 3, 7, 7, 4);
        let quota = OriginQuota::full(config, OriginQuotaInstant::from_nanos(10));
        let input = OriginQuotaInstant::from_nanos(20);
        assert_eq!(quota.admit(config, 2, input), quota.admit(config, 2, input));
        assert_eq!(
            quota.admit(config, 2, OriginQuotaInstant::from_nanos(9)),
            Err(OriginQuotaArithmeticError::MonotonicTimeRegressed)
        );
    }

    #[test]
    fn long_idle_refill_saturates_without_wrapping() {
        let config = config(u64::MAX, 3, u64::MAX, 7, 4);
        let quota = OriginQuota {
            messages: TokenBucket { scaled_tokens: 0 },
            bytes: TokenBucket { scaled_tokens: 0 },
            last_refill: OriginQuotaInstant::ZERO,
        };
        let (refilled, verdict) = quota
            .admit(config, 7, OriginQuotaInstant::from_nanos(u128::MAX))
            .expect("saturating refill remains defined");

        assert_eq!(verdict, OriginQuotaVerdict::Admitted);
        assert_eq!(refilled.whole_message_tokens(), 2);
        assert_eq!(refilled.whole_byte_tokens(), 0);
    }

    #[test]
    fn two_origins_and_two_lanes_have_independent_allowances() {
        let lane = config(1, 1, 1, 1, 4);
        let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
        let now = OriginQuotaInstant::ZERO;

        reserve(&mut table, 1, MessageCategory::Application, 1, now)
            .expect("origin A uses its application allowance");
        assert!(matches!(
            reserve(&mut table, 1, MessageCategory::Application, 1, now),
            Err(OriginQuotaAdmissionError::Verdict(
                OriginQuotaRejection::MessageRate
            ))
        ));
        reserve(&mut table, 2, MessageCategory::Application, 1, now)
            .expect("origin B keeps its application allowance");
        reserve(&mut table, 1, MessageCategory::Storage, 1, now)
            .expect("origin A keeps its storage allowance");
    }

    #[test]
    fn table_reuses_only_a_fully_replenished_oldest_record() {
        let lane = config(1, 1, 1, 1, 2);
        let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
        reserve(
            &mut table,
            2,
            MessageCategory::Application,
            1,
            OriginQuotaInstant::ZERO,
        )
        .expect("first record is admitted");
        reserve(
            &mut table,
            1,
            MessageCategory::Application,
            1,
            OriginQuotaInstant::from_nanos(1),
        )
        .expect("second record is admitted");
        assert!(matches!(
            reserve(
                &mut table,
                3,
                MessageCategory::Application,
                1,
                OriginQuotaInstant::from_nanos(2)
            ),
            Err(OriginQuotaAdmissionError::Verdict(
                OriginQuotaRejection::Capacity { capacity: 2 }
            ))
        ));

        reserve(
            &mut table,
            3,
            MessageCategory::Application,
            1,
            OriginQuotaInstant::from_nanos(NANOS_PER_SECOND + 1),
        )
        .expect("oldest fully replenished record is reusable");
        assert_eq!(table.len(), 2);
        assert!(table.get(key(1, MessageCategory::Application)).is_some());
        assert!(table.get(key(2, MessageCategory::Application)).is_none());
        assert!(table.get(key(3, MessageCategory::Application)).is_some());
    }

    /// A lane whose protocol admits each neighbour through a window of `budget = 16384`
    /// messages per `period = 150 s`.
    fn window_admission_lane() -> OriginQuotaLane {
        OriginQuotaLane::Paced(PacedLane::new(
            PacedLaneId::new(1),
            PacedRate::new(
                NonZeroU64::new(16_384).expect("non-zero budget"),
                NonZeroU64::new(150).expect("non-zero period"),
            ),
        ))
    }

    /// Admit one message of `byte_cost` every `interval_nanos` for `count` messages under
    /// `limits`, returning how many were refused.
    fn paced_refusals(
        limits: OriginQuotaLimits,
        interval_nanos: u128,
        count: u128,
        byte_cost: usize,
    ) -> usize {
        let mut quota = OriginQuota::full(limits, OriginQuotaInstant::ZERO);
        let mut refused = 0;
        for index in 0..count {
            let now = OriginQuotaInstant::from_nanos(index * interval_nanos);
            let (next, verdict) = quota
                .admit(limits, byte_cost, now)
                .expect("monotonic schedule");
            quota = next;
            refused += usize::from(verdict != OriginQuotaVerdict::Admitted);
        }
        refused
    }

    /// A sender paced at `r = 98` messages/s, below its protocol's `budget / period ≈ 109`, is
    /// never refused in its lane for half an hour, while the same schedule in the default
    /// Application lane is refused beyond its burst and 8 msg/s.
    #[test]
    fn paced_sender_at_the_protocol_rate_is_never_refused_but_the_default_lane_is() {
        let config = OriginQuotaConfig::default();
        let interval = NANOS_PER_SECOND / 98;
        let count = 98 * 1_800;
        let message_bytes = 13_442;

        assert_eq!(
            paced_refusals(
                config.limits(window_admission_lane()),
                interval,
                count,
                message_bytes
            ),
            0
        );
        let default_refusals = paced_refusals(
            config.limits(MessageCategory::Application.into()),
            interval,
            count,
            message_bytes,
        );
        // At most `burst + 8·T + 1` of the schedule fits the default lane.
        let default_admitted = usize::try_from(count).expect("small count") - default_refusals;
        assert!(default_admitted <= 32 + 8 * 1_800 + 1);
    }

    /// The paced lane bounds its origin by the supplied rate: over `T` seconds a flooding
    /// neighbour gets at most `budget + ⌈budget·T/period⌉` messages, the bound of the
    /// protocol's own window admission.
    #[test]
    fn paced_lane_bounds_a_flooding_neighbour_by_the_supplied_budget() {
        let limits = OriginQuotaConfig::default().limits(window_admission_lane());
        let seconds: u128 = 600;
        let per_second = 1_000;
        let count = per_second * seconds;
        let refused = paced_refusals(limits, NANOS_PER_SECOND / per_second, count, 1);
        let admitted = usize::try_from(count).expect("small count") - refused;
        let bound = 16_384 + usize::try_from(16_384 * seconds / 150).expect("small bound") + 1;
        assert!(admitted <= bound, "{admitted} > {bound}");
        assert!(refused > 0);
    }

    /// Distinct paced lanes, and a paced lane and its class lane, keep distinct records.
    #[test]
    fn paced_and_class_lanes_of_one_origin_are_independent_records() {
        let lane = config(1, 1, 1_000, 1_000, 4);
        let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
        let now = OriginQuotaInstant::ZERO;
        reserve(&mut table, 1, MessageCategory::Application, 1, now).expect("class allowance");
        assert!(reserve(&mut table, 1, MessageCategory::Application, 1, now).is_err());
        reserve(&mut table, 1, window_admission_lane(), 1, now)
            .expect("the paced lane keeps its own allowance");
        let other = OriginQuotaLane::Paced(PacedLane::new(
            PacedLaneId::new(2),
            PacedRate::new(NonZeroU64::MIN, NonZeroU64::MIN),
        ));
        reserve(&mut table, 1, other, 1, now).expect("a second paced lane keeps its own allowance");
        assert!(reserve(&mut table, 1, other, 1, now).is_err());
    }

    #[test]
    fn invalid_configuration_is_typed_and_deserialization_revalidates() {
        assert_eq!(
            OriginQuotaLaneConfig::new(0, 1, 1, 1, 1),
            Err(OriginQuotaConfigError::ZeroMessageRate)
        );
        let invalid = r#"{
            "message_rate_per_second": 1,
            "message_burst": 1,
            "byte_rate_per_second": 1,
            "byte_burst": 1,
            "max_records": 0
        }"#;
        assert!(serde_json::from_str::<OriginQuotaLaneConfig>(invalid).is_err());
    }

    #[test]
    fn counters_are_bounded_by_lane_and_reason() {
        let counters = OriginQuotaCounterState::default();
        let lane = MessageCategory::Application;
        counters.record(
            OriginQuotaLane::from(lane).id(),
            &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::MessageRate),
        );
        counters.record(
            OriginQuotaLane::from(lane).id(),
            &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::ByteRate),
        );
        counters.record(
            OriginQuotaLane::from(lane).id(),
            &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::Capacity { capacity: 1 }),
        );
        counters.record(
            OriginQuotaLane::from(MessageCategory::Storage).id(),
            &OriginQuotaAdmissionError::Arithmetic(
                OriginQuotaArithmeticError::MonotonicTimeRegressed,
            ),
        );

        assert_eq!(counters.snapshot().lane(lane), OriginQuotaLaneCounters {
            message_rate_exhausted: 1,
            byte_rate_exhausted: 1,
            capacity_exhausted: 1,
            arithmetic_failure: 0,
        });
        assert_eq!(
            counters
                .snapshot()
                .lane(MessageCategory::Storage)
                .arithmetic_failure,
            1
        );
    }
}
