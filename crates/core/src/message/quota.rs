//! Runtime-local origin quotas for final-destination transactions.
//!
//! A quota key is `(network_id, origin account DID, destination DID, logical lane)`. Each key
//! owns independent fixed-point message and byte token buckets. [`OriginQuota::admit`] is the
//! pure transition; the destination replay runtime owns the bounded table and supplies monotonic
//! time. Quota records are intentionally absent from the durable replay snapshot.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;

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

/// A final-destination logical admission lane.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OriginQuotaLane {
    /// Chord maintenance, connection negotiation, and topology queries.
    DhtControl,
    /// DHT entry lookup, mutation, and synchronization.
    Storage,
    /// End-to-end handshake and encrypted stream messages.
    E2e,
    /// Application-owned custom messages.
    Application,
}

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
    pub const fn lane(self, lane: OriginQuotaLane) -> OriginQuotaLaneConfig {
        match lane {
            OriginQuotaLane::DhtControl => self.dht_control,
            OriginQuotaLane::Storage => self.storage,
            OriginQuotaLane::E2e => self.e2e,
            OriginQuotaLane::Application => self.application,
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
    /// Logical inbound lane selected from the verified message.
    pub lane: OriginQuotaLane,
}

impl OriginQuotaKey {
    /// Name one origin-to-destination lane inside an overlay.
    pub const fn new(
        network_id: u32,
        origin_account: Did,
        destination: Did,
        lane: OriginQuotaLane,
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

    fn refill(self, rate: u64, burst: u64, elapsed_nanos: u128) -> Self {
        let capacity = u128::from(burst) * NANOS_PER_SECOND;
        let replenished = elapsed_nanos.saturating_mul(u128::from(rate));
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
    pub fn full(config: OriginQuotaLaneConfig, now: OriginQuotaInstant) -> Self {
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
        config: OriginQuotaLaneConfig,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<(Self, OriginQuotaVerdict), OriginQuotaArithmeticError> {
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
        config: OriginQuotaLaneConfig,
        now: OriginQuotaInstant,
    ) -> Result<Self, OriginQuotaArithmeticError> {
        let elapsed = now
            .0
            .checked_sub(self.last_refill.0)
            .ok_or(OriginQuotaArithmeticError::MonotonicTimeRegressed)?;
        Ok(Self {
            messages: self.messages.refill(
                config.message_rate_per_second,
                config.message_burst,
                elapsed,
            ),
            bytes: self
                .bytes
                .refill(config.byte_rate_per_second, config.byte_burst, elapsed),
            last_refill: now,
        })
    }

    fn fully_replenished_at(
        self,
        config: OriginQuotaLaneConfig,
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

    pub(super) fn reserve(
        &mut self,
        key: OriginQuotaKey,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<OriginQuotaReservation, OriginQuotaAdmissionError> {
        let lane_config = self.config.lane(key.lane);
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
            let Some((victim_key, _)) = victim else {
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
        lane: OriginQuotaLane,
        config: OriginQuotaLaneConfig,
        now: OriginQuotaInstant,
    ) -> Result<Option<(OriginQuotaKey, OriginQuotaInstant)>, OriginQuotaAdmissionError> {
        let mut victim = None;
        for (key, quota) in self.records.iter().filter(|(key, _)| key.lane == lane) {
            if !quota
                .fully_replenished_at(config, now)
                .map_err(OriginQuotaAdmissionError::Arithmetic)?
            {
                continue;
            }
            let candidate = (*key, quota.last_refill);
            if victim.is_none_or(|current| candidate < current) {
                victim = Some(candidate);
            }
        }
        Ok(victim)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.records.len()
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OriginQuotaCounters {
    lanes: [OriginQuotaLaneCounters; 4],
}

impl OriginQuotaCounters {
    /// Return aggregate drop counters for `lane`.
    pub const fn lane(self, lane: OriginQuotaLane) -> OriginQuotaLaneCounters {
        let [dht_control, storage, e2e, application] = self.lanes;
        match lane {
            OriginQuotaLane::DhtControl => dht_control,
            OriginQuotaLane::Storage => storage,
            OriginQuotaLane::E2e => e2e,
            OriginQuotaLane::Application => application,
        }
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
}

impl Default for OriginQuotaCounterState {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| OriginQuotaLaneCounterState::default()),
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
        }
    }

    pub(super) fn record(&self, lane: OriginQuotaLane, error: &OriginQuotaAdmissionError) {
        let [dht_control, storage, e2e, application] = &self.lanes;
        let counters = match lane {
            OriginQuotaLane::DhtControl => dht_control,
            OriginQuotaLane::Storage => storage,
            OriginQuotaLane::E2e => e2e,
            OriginQuotaLane::Application => application,
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
            crate::error::Error::OriginQuotaMessageRateExhausted { key }
        }
        OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::ByteRate) => {
            crate::error::Error::OriginQuotaByteRateExhausted {
                key,
                requested_bytes: byte_cost,
            }
        }
        OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::Capacity { capacity }) => {
            crate::error::Error::OriginQuotaTableCapacityExhausted {
                lane: key.lane,
                capacity,
            }
        }
        OriginQuotaAdmissionError::Arithmetic(source) => {
            crate::error::Error::OriginQuotaArithmetic { key, source }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn key(origin: u32, lane: OriginQuotaLane) -> OriginQuotaKey {
        OriginQuotaKey::new(7, Did::from(origin), Did::from(99_u32), lane)
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

        table
            .reserve(key(1, OriginQuotaLane::Application), 1, now)
            .expect("origin A uses its application allowance");
        assert!(matches!(
            table.reserve(key(1, OriginQuotaLane::Application), 1, now),
            Err(OriginQuotaAdmissionError::Verdict(
                OriginQuotaRejection::MessageRate
            ))
        ));
        table
            .reserve(key(2, OriginQuotaLane::Application), 1, now)
            .expect("origin B keeps its application allowance");
        table
            .reserve(key(1, OriginQuotaLane::Storage), 1, now)
            .expect("origin A keeps its storage allowance");
    }

    #[test]
    fn table_reuses_only_a_fully_replenished_oldest_record() {
        let lane = config(1, 1, 1, 1, 2);
        let mut table = OriginQuotaTable::new(OriginQuotaConfig::new(lane, lane, lane, lane));
        table
            .reserve(
                key(1, OriginQuotaLane::Application),
                1,
                OriginQuotaInstant::ZERO,
            )
            .expect("first record is admitted");
        table
            .reserve(
                key(2, OriginQuotaLane::Application),
                1,
                OriginQuotaInstant::from_nanos(1),
            )
            .expect("second record is admitted");
        assert!(matches!(
            table.reserve(
                key(3, OriginQuotaLane::Application),
                1,
                OriginQuotaInstant::from_nanos(2)
            ),
            Err(OriginQuotaAdmissionError::Verdict(
                OriginQuotaRejection::Capacity { capacity: 2 }
            ))
        ));

        table
            .reserve(
                key(3, OriginQuotaLane::Application),
                1,
                OriginQuotaInstant::from_nanos(NANOS_PER_SECOND),
            )
            .expect("oldest fully replenished record is reusable");
        assert_eq!(table.len(), 2);
        assert!(table.get(key(1, OriginQuotaLane::Application)).is_none());
        assert!(table.get(key(2, OriginQuotaLane::Application)).is_some());
        assert!(table.get(key(3, OriginQuotaLane::Application)).is_some());
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
        let lane = OriginQuotaLane::Application;
        counters.record(
            lane,
            &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::MessageRate),
        );
        counters.record(
            lane,
            &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::ByteRate),
        );
        counters.record(
            lane,
            &OriginQuotaAdmissionError::Verdict(OriginQuotaRejection::Capacity { capacity: 1 }),
        );
        counters.record(
            OriginQuotaLane::Storage,
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
                .lane(OriginQuotaLane::Storage)
                .arithmetic_failure,
            1
        );
    }
}
