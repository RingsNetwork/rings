use serde::Deserialize;
use serde::Serialize;

use crate::MeasureError;
use crate::Metric;
use crate::PolicyError;
use crate::UnixTime;

const BYTES_PER_MEBIBYTE: f64 = 1_048_576.0;
const AMULE_ACTIVATION_BYTES: u64 = 1_000_000;
const AMULE_RETENTION_SECONDS: u64 = 150 * 24 * 60 * 60;
const MINIMUM_SCORE: f64 = 1.0;
const MAXIMUM_SCORE: f64 = 10.0;

/// Configuration of the local byte-credit relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditPolicy {
    activation_bytes: u64,
    retention_seconds: u64,
}

impl CreditPolicy {
    /// Construct a credit policy.
    pub const fn new(activation_bytes: u64, retention_seconds: u64) -> Result<Self, PolicyError> {
        if retention_seconds == 0 {
            return Err(PolicyError::ZeroCreditRetention);
        }
        Ok(Self {
            activation_bytes,
            retention_seconds,
        })
    }

    /// Return the aMule-compatible default policy.
    pub const fn amule() -> Self {
        Self {
            activation_bytes: AMULE_ACTIVATION_BYTES,
            retention_seconds: AMULE_RETENTION_SECONDS,
        }
    }

    /// Bytes received from a peer before credits become active.
    pub const fn activation_bytes(self) -> u64 {
        self.activation_bytes
    }

    /// Seconds without observation after which a peer record expires.
    pub const fn retention_seconds(self) -> u64 {
        self.retention_seconds
    }
}

impl Default for CreditPolicy {
    fn default() -> Self {
        Self::amule()
    }
}

/// Opaque finite aMule-compatible credit multiplier in the closed interval `[1, 10]`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct CreditScore(f64);

impl CreditScore {
    /// Neutral credit with no priority multiplier.
    pub const NEUTRAL: Self = Self(MINIMUM_SCORE);

    fn from_formula(value: f64) -> Self {
        let finite = if value.is_finite() {
            value
        } else {
            MINIMUM_SCORE
        };
        Self(finite.clamp(MINIMUM_SCORE, MAXIMUM_SCORE))
    }

    /// Return the multiplier as `f64`.
    pub const fn as_f64(self) -> f64 {
        self.0
    }
}

/// Persistent useful-byte totals for one authenticated peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditRecord {
    bytes_sent_to_peer: u64,
    bytes_received_from_peer: u64,
    last_seen: UnixTime,
}

impl CreditRecord {
    /// Construct a record from validated persistent totals.
    pub const fn new(
        bytes_sent_to_peer: u64,
        bytes_received_from_peer: u64,
        last_seen: UnixTime,
    ) -> Self {
        Self {
            bytes_sent_to_peer,
            bytes_received_from_peer,
            last_seen,
        }
    }

    /// Construct an empty record first observed at `last_seen`.
    pub const fn empty(last_seen: UnixTime) -> Self {
        Self::new(0, 0, last_seen)
    }

    /// Useful payload bytes sent by the local node to the peer.
    pub const fn bytes_sent_to_peer(self) -> u64 {
        self.bytes_sent_to_peer
    }

    /// Useful payload bytes received and verified from the peer.
    pub const fn bytes_received_from_peer(self) -> u64 {
        self.bytes_received_from_peer
    }

    /// Most recent authenticated local observation.
    pub const fn last_seen(self) -> UnixTime {
        self.last_seen
    }

    /// Compute the aMule-compatible local credit multiplier.
    pub fn score(self, policy: CreditPolicy) -> CreditScore {
        if self.bytes_received_from_peer < policy.activation_bytes() {
            return CreditScore::NEUTRAL;
        }

        let ratio = if self.bytes_sent_to_peer == 0 {
            MAXIMUM_SCORE
        } else {
            (self.bytes_received_from_peer as f64 * 2.0) / self.bytes_sent_to_peer as f64
        };
        let volume_cap = (self.bytes_received_from_peer as f64 / BYTES_PER_MEBIBYTE + 2.0).sqrt();
        CreditScore::from_formula(ratio.min(volume_cap))
    }

    pub(crate) fn record_sent(
        &mut self,
        useful_bytes: u64,
        at: UnixTime,
    ) -> Result<(), MeasureError> {
        self.ensure_not_before_last_seen(at)?;
        self.bytes_sent_to_peer = self.bytes_sent_to_peer.checked_add(useful_bytes).ok_or(
            MeasureError::CounterOverflow {
                metric: Metric::BytesSent,
            },
        )?;
        self.last_seen = at;
        Ok(())
    }

    pub(crate) fn record_received(
        &mut self,
        useful_bytes: u64,
        at: UnixTime,
    ) -> Result<(), MeasureError> {
        self.ensure_not_before_last_seen(at)?;
        self.bytes_received_from_peer = self
            .bytes_received_from_peer
            .checked_add(useful_bytes)
            .ok_or(MeasureError::CounterOverflow {
                metric: Metric::BytesReceived,
            })?;
        self.last_seen = at;
        Ok(())
    }

    pub(crate) fn touch(&mut self, at: UnixTime) -> Result<(), MeasureError> {
        self.ensure_not_before_last_seen(at)?;
        self.last_seen = at;
        Ok(())
    }

    pub(crate) fn is_expired(
        self,
        now: UnixTime,
        policy: CreditPolicy,
    ) -> Result<bool, MeasureError> {
        let age = now.as_secs().checked_sub(self.last_seen.as_secs()).ok_or(
            MeasureError::ClockRegression {
                observed: now,
                current: self.last_seen,
            },
        )?;
        Ok(age >= policy.retention_seconds())
    }

    pub(crate) fn ensure_not_before_last_seen(self, at: UnixTime) -> Result<(), MeasureError> {
        if at < self.last_seen {
            return Err(MeasureError::ClockRegression {
                observed: at,
                current: self.last_seen,
            });
        }
        Ok(())
    }

    pub(crate) fn reconcile_clock(&mut self, now: UnixTime) -> bool {
        if self.last_seen <= now {
            return false;
        }
        self.last_seen = now;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_score(record: CreditRecord, expected: f64) {
        let actual = record.score(CreditPolicy::amule()).as_f64();
        assert!(
            (actual - expected).abs() < 1.0e-12,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn score_is_neutral_below_activation_threshold() {
        assert_score(
            CreditRecord::new(0, AMULE_ACTIVATION_BYTES - 1, UnixTime::EPOCH),
            1.0,
        );
    }

    #[test]
    fn score_uses_volume_cap_when_nothing_was_sent() {
        let received = AMULE_ACTIVATION_BYTES;
        let expected = (received as f64 / BYTES_PER_MEBIBYTE + 2.0).sqrt();
        assert_score(CreditRecord::new(0, received, UnixTime::EPOCH), expected);
    }

    #[test]
    fn score_clamps_ratio_to_neutral_lower_bound() {
        assert_score(
            CreditRecord::new(4_000_000, 1_000_000, UnixTime::EPOCH),
            1.0,
        );
    }

    #[test]
    fn score_clamps_large_volume_to_maximum() {
        assert_score(CreditRecord::new(0, 128 * 1_048_576, UnixTime::EPOCH), 10.0);
    }

    #[test]
    fn score_laws_hold_over_boundary_grid() {
        let received_values = [
            0,
            999_999,
            1_000_000,
            1_048_576,
            2_097_152,
            16_777_216,
            134_217_728,
        ];
        let sent_values = [0, 1, 500_000, 1_000_000, 4_000_000, 64_000_000];

        for sent in sent_values {
            let mut previous = 1.0;
            for received in received_values {
                let score = CreditRecord::new(sent, received, UnixTime::EPOCH)
                    .score(CreditPolicy::amule())
                    .as_f64();
                assert!(score.is_finite());
                assert!((1.0..=10.0).contains(&score));
                assert!(score >= previous, "received={received}, sent={sent}");
                previous = score;
            }
        }
    }

    #[test]
    fn sending_more_without_receiving_cannot_improve_score() {
        let received = 16 * 1_048_576;
        let sent_values = [0, 1, 1_048_576, 4_194_304, 16_777_216, 67_108_864];
        let mut previous = 10.0;
        for sent in sent_values {
            let score = CreditRecord::new(sent, received, UnixTime::EPOCH)
                .score(CreditPolicy::amule())
                .as_f64();
            assert!(score <= previous, "received={received}, sent={sent}");
            previous = score;
        }
    }

    #[test]
    fn expiry_boundary_is_exactly_retention() {
        let policy = CreditPolicy::amule();
        let record = CreditRecord::empty(UnixTime::from_secs(10));
        let before = UnixTime::from_secs(10 + policy.retention_seconds() - 1);
        let at = UnixTime::from_secs(10 + policy.retention_seconds());
        assert_eq!(record.is_expired(before, policy), Ok(false));
        assert_eq!(record.is_expired(at, policy), Ok(true));
    }
}
