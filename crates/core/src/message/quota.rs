//! Runtime-local origin quotas for final-destination transactions.
//!
//! A quota key is `(network_id, origin account DID, destination DID, logical lane)`. Each key
//! owns independent fixed-point message and byte token buckets, and quota records in each lane's
//! bounded table are charged to their introducing [`Principal`] under [`SlotAccounting`]
//! ([`ORIGIN_QUOTA_SHARES`]), preserving each connection slot's floor, [`Principal::Local`], and
//! [`Principal::Inbox`] (#1178). [`OriginQuota::admit`] is the pure transition; the destination
//! replay runtime owns the bounded table and supplies monotonic time. Quota records are
//! intentionally absent from the durable replay store.
//!
//! An admission either enforces the message limit or, for traffic whose namespace delegated its
//! admission, skips it ([`MessageLimit`]); the byte limit and the record bound always apply. A
//! delegated admission pays at least [`DELEGATED_MIN_CHARGE`] bytes, so the byte bucket also
//! bounds how many messages core admits for it.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::fair_admission::Principal;
use crate::fair_admission::Shares;
use crate::fair_admission::SlotAccounting;
use crate::message::admission_delegation::MessageLimit;
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
/// Minimum byte charge of a delegated admission.
///
/// Every admitted final-destination message costs core a fixed amount of work: the
/// replay-record persist under the replay lock, then its logical lane, validation and dispatch.
/// A delegated admission skips the message limit, so without a floor only the byte bucket would
/// bound how many messages are admitted, and tiny messages would multiply that work. Charging
/// `max(len, DELEGATED_MIN_CHARGE)` bounds a neighbour's delegated admissions to
/// `byte_rate / 16 KiB` per second (256/s under the default 4 MiB/s), with a burst of
/// `byte_burst / 16 KiB` (4096 by default). Messages of at least 16 KiB pay exactly their
/// length. The decode and signature checks run before the quota and are not bounded by it.
pub const DELEGATED_MIN_CHARGE: usize = 16 * 1024;
/// Default number of runtime-local origin records retained per logical lane.
pub const DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE: usize = 1024;

/// The shares of one logical lane's origin-quota record table under
/// [`DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE`] (load-bounds paper, Law 1, #1178): `f = 1` record
/// per connection slot, a shared pool of `128`, a local share of `288` and an inbox share of
/// `256`, so `B(s) = R(s)·1 + 672`: `1000` at `s = 3` and `1024` at `s = 15`.
pub(crate) const ORIGIN_QUOTA_SHARES: Shares = Shares {
    floor: 1,
    pool: 128,
    local: 288,
    inbox: 256,
    custody: 0,
};

const ORIGIN_QUOTA_SUCCESSORS_MAX: usize = 15;
const _: () = assert!(
    ORIGIN_QUOTA_SHARES.bound(crate::dht::topology::edge_slots(
        ORIGIN_QUOTA_SUCCESSORS_MAX
    )) <= DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE
);
const _: () = assert!(
    ORIGIN_QUOTA_SHARES.bound(crate::dht::topology::edge_slots(
        ORIGIN_QUOTA_SUCCESSORS_MAX + 1
    )) > DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE
);

const fn shares_for_capacity(max_records: usize) -> Shares {
    if max_records >= DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE {
        Shares {
            floor: ORIGIN_QUOTA_SHARES.floor,
            pool: ORIGIN_QUOTA_SHARES
                .pool
                .saturating_add(max_records - DEFAULT_ORIGIN_QUOTA_RECORDS_PER_LANE),
            local: ORIGIN_QUOTA_SHARES.local,
            inbox: ORIGIN_QUOTA_SHARES.inbox,
            custody: ORIGIN_QUOTA_SHARES.custody,
        }
    } else {
        Shares {
            floor: 1,
            pool: max_records.saturating_sub(2),
            local: max_records,
            inbox: max_records,
            custody: 0,
        }
    }
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
    pub const fn lane(self, lane: MessageCategory) -> OriginQuotaLaneConfig {
        match lane {
            MessageCategory::DhtControl => self.dht_control,
            MessageCategory::Storage => self.storage,
            MessageCategory::E2e => self.e2e,
            MessageCategory::Application => self.application,
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
    pub lane: MessageCategory,
}

impl OriginQuotaKey {
    /// Name one origin-to-destination lane inside an overlay.
    pub const fn new(
        network_id: u32,
        origin_account: Did,
        destination: Did,
        lane: MessageCategory,
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
    /// this transition. Under [`MessageLimit::Delegated`] the message cost is zero: the message
    /// bucket is neither checked nor consumed, while the byte bucket is charged at least
    /// [`DELEGATED_MIN_CHARGE`].
    pub fn admit(
        self,
        config: OriginQuotaLaneConfig,
        message_limit: MessageLimit,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<(Self, OriginQuotaVerdict), OriginQuotaArithmeticError> {
        let mut next = self.refilled(config, now)?;
        let message_cost = scaled_message_cost(message_limit);
        if !next.messages.has(message_cost) {
            return Ok((next, OriginQuotaVerdict::MessageRateExhausted));
        }
        let byte_cost = charged_bytes(message_limit, byte_cost);
        let byte_cost =
            u128::try_from(byte_cost).map_err(|_| OriginQuotaArithmeticError::ByteCostOverflow)?;
        let scaled_byte_cost = byte_cost
            .checked_mul(NANOS_PER_SECOND)
            .ok_or(OriginQuotaArithmeticError::ByteCostOverflow)?;
        if !next.bytes.has(scaled_byte_cost) {
            return Ok((next, OriginQuotaVerdict::ByteRateExhausted));
        }
        next.messages.consume(message_cost);
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

/// Scaled message tokens one admission costs: one token, or none when admission is delegated.
const fn scaled_message_cost(message_limit: MessageLimit) -> u128 {
    match message_limit {
        MessageLimit::Enforced => NANOS_PER_SECOND,
        MessageLimit::Delegated => 0,
    }
}

/// Bytes one admission of `byte_cost` logical bytes is charged: its length, raised to
/// [`DELEGATED_MIN_CHARGE`] when admission is delegated.
fn charged_bytes(message_limit: MessageLimit, byte_cost: usize) -> usize {
    match message_limit {
        MessageLimit::Enforced => byte_cost,
        MessageLimit::Delegated => byte_cost.max(DELEGATED_MIN_CHARGE),
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
        lane: MessageCategory,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OriginQuotaRecord {
    quota: OriginQuota,
    introducer: Principal,
}

#[derive(Debug)]
pub(super) struct OriginQuotaReservation {
    key: OriginQuotaKey,
    introducer: Principal,
    previous: Option<OriginQuotaRecord>,
    evicted: Option<(OriginQuotaKey, OriginQuotaRecord)>,
}

impl OriginQuotaReservation {
    pub(super) fn rollback(self, table: &mut OriginQuotaTable) {
        match self.previous {
            Some(previous) => {
                table.records.insert(self.key, previous);
            }
            None => {
                table.records.remove(&self.key);
                table
                    .lane_accounting_mut(self.key.lane)
                    .release(self.introducer, 1);
            }
        }
        if let Some((victim_key, victim_record)) = self.evicted {
            table
                .lane_accounting_mut(victim_key.lane)
                .hold(victim_record.introducer, 1);
            table.records.insert(victim_key, victim_record);
        }
    }
}

pub(super) struct OriginQuotaTable {
    config: OriginQuotaConfig,
    records: BTreeMap<OriginQuotaKey, OriginQuotaRecord>,
    accounting: [SlotAccounting; MessageCategory::COUNT],
}

impl OriginQuotaTable {
    pub(super) fn new(config: OriginQuotaConfig) -> Self {
        let accounting = [
            MessageCategory::DhtControl,
            MessageCategory::Storage,
            MessageCategory::E2e,
            MessageCategory::Application,
        ]
        .map(|lane| SlotAccounting::new(shares_for_capacity(config.lane(lane).max_records)));
        Self {
            config,
            records: BTreeMap::new(),
            accounting,
        }
    }

    fn lane_accounting(&self, lane: MessageCategory) -> &SlotAccounting {
        let [dht_control, storage, e2e, application] = &self.accounting;
        match lane {
            MessageCategory::DhtControl => dht_control,
            MessageCategory::Storage => storage,
            MessageCategory::E2e => e2e,
            MessageCategory::Application => application,
        }
    }

    fn lane_accounting_mut(&mut self, lane: MessageCategory) -> &mut SlotAccounting {
        let [dht_control, storage, e2e, application] = &mut self.accounting;
        match lane {
            MessageCategory::DhtControl => dht_control,
            MessageCategory::Storage => storage,
            MessageCategory::E2e => e2e,
            MessageCategory::Application => application,
        }
    }

    pub(super) fn reserve(
        &mut self,
        key: OriginQuotaKey,
        introducer: Principal,
        message_limit: MessageLimit,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<OriginQuotaReservation, OriginQuotaAdmissionError> {
        let lane_config = self.config.lane(key.lane);
        if let Some(previous) = self.records.get(&key).copied() {
            let (next_quota, origin_verdict) = previous
                .quota
                .admit(lane_config, message_limit, byte_cost, now)
                .map_err(OriginQuotaAdmissionError::Arithmetic)?;
            self.records.insert(key, OriginQuotaRecord {
                quota: next_quota,
                introducer: previous.introducer,
            });
            if let Some(rejection) = origin_verdict.rejection() {
                return Err(OriginQuotaAdmissionError::Verdict(rejection));
            }
            return Ok(OriginQuotaReservation {
                key,
                introducer: previous.introducer,
                previous: Some(previous),
                evicted: None,
            });
        }

        let (next_quota, origin_verdict) = OriginQuota::full(lane_config, now)
            .admit(lane_config, message_limit, byte_cost, now)
            .map_err(OriginQuotaAdmissionError::Arithmetic)?;
        if let Some(rejection) = origin_verdict.rejection() {
            return Err(OriginQuotaAdmissionError::Verdict(rejection));
        }

        let evicted = self.allocate_record(key.lane, introducer, lane_config, now)?;
        self.records.insert(key, OriginQuotaRecord {
            quota: next_quota,
            introducer,
        });
        Ok(OriginQuotaReservation {
            key,
            introducer,
            previous: None,
            evicted,
        })
    }

    fn allocate_record(
        &mut self,
        lane: MessageCategory,
        introducer: Principal,
        config: OriginQuotaLaneConfig,
        now: OriginQuotaInstant,
    ) -> Result<Option<(OriginQuotaKey, OriginQuotaRecord)>, OriginQuotaAdmissionError> {
        let floor = shares_for_capacity(config.max_records).floor;
        let held = self.lane_accounting(lane).held(introducer);
        if matches!(introducer, Principal::Slot(_) | Principal::Unbound) && held >= floor {
            if let Some(victim_key) =
                self.safe_victim(lane, config, now, |record| record.introducer == introducer)?
            {
                let evicted = self
                    .records
                    .remove(&victim_key)
                    .map(|record| (victim_key, record));
                if let Some((_, ref victim_record)) = evicted {
                    self.lane_accounting_mut(lane)
                        .release(victim_record.introducer, 1);
                }
                self.lane_accounting_mut(lane).hold(introducer, 1);
                return Ok(evicted);
            }
        }

        let lane_records = self
            .records
            .keys()
            .filter(|candidate| candidate.lane == lane)
            .count();
        if lane_records < config.max_records && self.lane_accounting(lane).admit(introducer).is_ok()
        {
            self.lane_accounting_mut(lane).hold(introducer, 1);
            return Ok(None);
        }

        if held > 0 {
            if let Some(victim_key) =
                self.safe_victim(lane, config, now, |record| record.introducer == introducer)?
            {
                let evicted = self
                    .records
                    .remove(&victim_key)
                    .map(|record| (victim_key, record));
                if let Some((_, ref victim_record)) = evicted {
                    self.lane_accounting_mut(lane)
                        .release(victim_record.introducer, 1);
                }
                self.lane_accounting_mut(lane).hold(introducer, 1);
                return Ok(evicted);
            }
        }

        let accounting = self.lane_accounting(lane);
        let pool_victim = self.safe_victim(lane, config, now, |record| {
            accounting.pool_items(record.introducer) > 0
        })?;
        if let Some(victim_key) = pool_victim {
            if let Some(victim_record) = self.records.remove(&victim_key) {
                self.lane_accounting_mut(lane)
                    .release(victim_record.introducer, 1);
                if self.lane_accounting(lane).admit(introducer).is_ok() {
                    self.lane_accounting_mut(lane).hold(introducer, 1);
                    return Ok(Some((victim_key, victim_record)));
                }
                self.lane_accounting_mut(lane)
                    .hold(victim_record.introducer, 1);
                self.records.insert(victim_key, victim_record);
            }
        }

        Err(OriginQuotaAdmissionError::Verdict(
            OriginQuotaRejection::Capacity {
                capacity: config.max_records,
            },
        ))
    }

    fn safe_victim(
        &self,
        lane: MessageCategory,
        config: OriginQuotaLaneConfig,
        now: OriginQuotaInstant,
        eligible: impl Fn(&OriginQuotaRecord) -> bool,
    ) -> Result<Option<OriginQuotaKey>, OriginQuotaAdmissionError> {
        let mut victim = None;
        for (key, record) in self
            .records
            .iter()
            .filter(|(key, record)| key.lane == lane && eligible(record))
        {
            if !record
                .quota
                .fully_replenished_at(config, now)
                .map_err(OriginQuotaAdmissionError::Arithmetic)?
            {
                continue;
            }
            let candidate = (record.quota.last_refill, *key);
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

    #[cfg(test)]
    pub(super) fn held(&self, lane: MessageCategory, principal: Principal) -> usize {
        self.lane_accounting(lane).held(principal)
    }

    /// The whole `(message, byte)` tokens `key`'s record held after its last admission.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn whole_tokens(&self, key: OriginQuotaKey) -> Option<(u128, u128)> {
        self.records.get(&key).map(|record| {
            (
                record.quota.whole_message_tokens(),
                record.quota.whole_byte_tokens(),
            )
        })
    }

    #[cfg(test)]
    pub(super) fn get(&self, key: OriginQuotaKey) -> Option<OriginQuota> {
        self.records.get(&key).map(|record| record.quota)
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
    pub const fn lane(self, lane: MessageCategory) -> OriginQuotaLaneCounters {
        let [dht_control, storage, e2e, application] = self.lanes;
        match lane {
            MessageCategory::DhtControl => dht_control,
            MessageCategory::Storage => storage,
            MessageCategory::E2e => e2e,
            MessageCategory::Application => application,
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

    pub(super) fn record(&self, lane: MessageCategory, error: &OriginQuotaAdmissionError) {
        let [dht_control, storage, e2e, application] = &self.lanes;
        let counters = match lane {
            MessageCategory::DhtControl => dht_control,
            MessageCategory::Storage => storage,
            MessageCategory::E2e => e2e,
            MessageCategory::Application => application,
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
mod test_quota;
