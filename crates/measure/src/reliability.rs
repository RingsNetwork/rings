use std::num::NonZeroU64;

use serde::Deserialize;
use serde::Serialize;

use crate::MeasureError;
use crate::MeasurementBatch;
use crate::MeasurementEvent;
use crate::Metric;
use crate::PolicyError;
use crate::UnixTime;

/// Absolute failure limits used to classify recent local evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliabilityThresholds {
    disconnected: u64,
    failed_to_send: u64,
    failed_to_receive: u64,
}

impl ReliabilityThresholds {
    /// Construct failure limits.
    pub const fn new(disconnected: u64, failed_to_send: u64, failed_to_receive: u64) -> Self {
        Self {
            disconnected,
            failed_to_send,
            failed_to_receive,
        }
    }
}

/// Policy for recent local transport reliability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliabilityPolicy {
    window_seconds: NonZeroU64,
    minimum_positive_observations: u64,
    thresholds: ReliabilityThresholds,
}

impl ReliabilityPolicy {
    /// Construct a policy from a type-proven non-zero aligned window.
    pub const fn from_nonzero_window(
        window_seconds: NonZeroU64,
        minimum_positive_observations: u64,
        thresholds: ReliabilityThresholds,
    ) -> Self {
        Self {
            window_seconds,
            minimum_positive_observations,
            thresholds,
        }
    }

    /// Construct a reliability policy with an aligned non-zero window.
    pub const fn new(
        window_seconds: u64,
        minimum_positive_observations: u64,
        thresholds: ReliabilityThresholds,
    ) -> Result<Self, PolicyError> {
        if window_seconds == 0 {
            return Err(PolicyError::ZeroReliabilityWindow);
        }
        let Some(window_seconds) = NonZeroU64::new(window_seconds) else {
            return Err(PolicyError::ZeroReliabilityWindow);
        };
        Ok(Self::from_nonzero_window(
            window_seconds,
            minimum_positive_observations,
            thresholds,
        ))
    }

    /// Window size in seconds.
    pub const fn window_seconds(self) -> u64 {
        self.window_seconds.get()
    }

    /// Minimum successful observations required before a peer becomes healthy.
    pub const fn minimum_positive_observations(self) -> u64 {
        self.minimum_positive_observations
    }

    /// Failure limits used by this policy.
    pub const fn thresholds(self) -> ReliabilityThresholds {
        self.thresholds
    }

    fn epoch_start(self, at: UnixTime) -> UnixTime {
        UnixTime::from_secs(at.as_secs() - at.as_secs() % self.window_seconds.get())
    }
}

/// Advisory local reliability class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReliabilityClass {
    /// The peer has enough positive evidence and remains below failure limits.
    Healthy,
    /// The local node has insufficient recent evidence.
    Unknown,
    /// Recent local evidence reached a configured failure limit.
    Degraded,
}

impl ReliabilityClass {
    /// Stable advisory priority rank; smaller values are attempted first.
    pub const fn connection_rank(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Unknown => 1,
            Self::Degraded => 2,
        }
    }
}

/// Recent logical transport outcomes for one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReliabilityEvidence {
    /// Successful connection observations.
    pub connected: u64,
    /// Disconnection observations.
    pub disconnected: u64,
    /// Successfully delivered logical messages.
    pub sent: u64,
    /// Logical messages that failed before delivery.
    pub failed_to_send: u64,
    /// Successfully received and verified logical messages.
    pub received: u64,
    /// Logical messages that failed reassembly, decoding, or verification.
    pub failed_to_receive: u64,
}

impl ReliabilityEvidence {
    /// Construct explicit evidence values.
    pub const fn new(
        connected: u64,
        disconnected: u64,
        sent: u64,
        failed_to_send: u64,
        received: u64,
        failed_to_receive: u64,
    ) -> Self {
        Self {
            connected,
            disconnected,
            sent,
            failed_to_send,
            received,
            failed_to_receive,
        }
    }

    /// Return whether every evidence counter is zero.
    pub const fn is_unobserved(self) -> bool {
        self.connected == 0
            && self.disconnected == 0
            && self.sent == 0
            && self.failed_to_send == 0
            && self.received == 0
            && self.failed_to_receive == 0
    }

    /// Classify evidence with the legacy one-positive-observation rule.
    pub const fn classify(self, thresholds: ReliabilityThresholds) -> ReliabilityClass {
        if self.reaches_failure_limit(thresholds) {
            ReliabilityClass::Degraded
        } else if self.positive_observations() >= 1 {
            ReliabilityClass::Healthy
        } else {
            ReliabilityClass::Unknown
        }
    }

    /// Classify this evidence under a complete reliability policy.
    pub const fn classify_with_policy(self, policy: ReliabilityPolicy) -> ReliabilityClass {
        if self.reaches_failure_limit(policy.thresholds()) {
            ReliabilityClass::Degraded
        } else if self.positive_observations() >= policy.minimum_positive_observations() {
            ReliabilityClass::Healthy
        } else {
            ReliabilityClass::Unknown
        }
    }

    /// Return whether any failure counter reached its configured limit.
    pub const fn reaches_failure_limit(self, thresholds: ReliabilityThresholds) -> bool {
        self.disconnected >= thresholds.disconnected
            || self.failed_to_send >= thresholds.failed_to_send
            || self.failed_to_receive >= thresholds.failed_to_receive
    }

    /// Total successful local observations.
    pub const fn positive_observations(self) -> u64 {
        self.connected
            .saturating_add(self.sent)
            .saturating_add(self.received)
    }

    fn increment_by(
        &mut self,
        event: MeasurementEvent,
        occurrences: NonZeroU64,
    ) -> Result<(), MeasureError> {
        let (counter, metric) = match event {
            MeasurementEvent::Connected => (&mut self.connected, Metric::Connected),
            MeasurementEvent::Disconnected => (&mut self.disconnected, Metric::Disconnected),
            MeasurementEvent::Sent { .. } => (&mut self.sent, Metric::Sent),
            MeasurementEvent::FailedToSend => (&mut self.failed_to_send, Metric::FailedToSend),
            MeasurementEvent::Received { .. } => (&mut self.received, Metric::Received),
            MeasurementEvent::FailedToReceive => {
                (&mut self.failed_to_receive, Metric::FailedToReceive)
            }
        };
        *counter = counter
            .checked_add(occurrences.get())
            .ok_or(MeasureError::CounterOverflow { metric })?;
        Ok(())
    }
}

/// One non-overlapping aligned reliability epoch for a peer.
///
/// Evidence belongs only to the represented epoch. Projecting into a later
/// epoch returns no evidence, so a previously degraded peer becomes unknown at
/// the next boundary until new observations arrive. This deliberate reset
/// prevents historical reliability from becoming a long-lived reputation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReliabilityWindow {
    epoch_start: Option<UnixTime>,
    window_seconds: Option<u64>,
    evidence: ReliabilityEvidence,
}

impl ReliabilityWindow {
    /// Construct an explicit persisted window.
    pub const fn new(
        epoch_start: Option<UnixTime>,
        window_seconds: Option<u64>,
        evidence: ReliabilityEvidence,
    ) -> Self {
        Self {
            epoch_start,
            window_seconds,
            evidence,
        }
    }

    /// Start of the represented aligned epoch, if any event was observed.
    pub const fn epoch_start(self) -> Option<UnixTime> {
        self.epoch_start
    }

    /// Aligned reliability window associated with the retained epoch.
    pub const fn window_seconds(self) -> Option<u64> {
        self.window_seconds
    }

    /// Stored evidence before time-based projection.
    pub const fn stored_evidence(self) -> ReliabilityEvidence {
        self.evidence
    }

    pub(crate) fn ensure_policy(self, policy: ReliabilityPolicy) -> Result<(), MeasureError> {
        match self.window_seconds {
            None if self.epoch_start.is_none() => Ok(()),
            Some(stored_seconds) if stored_seconds == policy.window_seconds() => Ok(()),
            Some(stored_seconds) => Err(MeasureError::ReliabilityWindowMismatch {
                stored_seconds,
                supplied_seconds: policy.window_seconds(),
            }),
            None => Err(MeasureError::SnapshotReliabilityWindowMissing),
        }
    }

    /// Return evidence live at `now`; later epochs project to no evidence.
    pub fn evidence_at(
        self,
        now: UnixTime,
        policy: ReliabilityPolicy,
    ) -> Result<ReliabilityEvidence, MeasureError> {
        let Some(current) = self.epoch_start else {
            return Ok(ReliabilityEvidence::default());
        };
        self.ensure_policy(policy)?;
        let observed = policy.epoch_start(now);
        if observed < current {
            return Err(MeasureError::ClockRegression { observed, current });
        }
        if observed == current {
            Ok(self.evidence)
        } else {
            Ok(ReliabilityEvidence::default())
        }
    }

    #[cfg(test)]
    pub(crate) fn observe(
        &mut self,
        event: MeasurementEvent,
        at: UnixTime,
        policy: ReliabilityPolicy,
    ) -> Result<(), MeasureError> {
        self.observe_batch(MeasurementBatch::single(event), at, policy)
    }

    pub(crate) fn observe_batch(
        &mut self,
        batch: MeasurementBatch,
        at: UnixTime,
        policy: ReliabilityPolicy,
    ) -> Result<(), MeasureError> {
        self.ensure_policy(policy)?;
        let observed = policy.epoch_start(at);
        match self.epoch_start {
            Some(current) if observed < current => {
                return Err(MeasureError::ClockRegression { observed, current });
            }
            Some(current) if observed > current => {
                self.epoch_start = Some(observed);
                self.evidence = ReliabilityEvidence::default();
            }
            None => {
                self.epoch_start = Some(observed);
                self.window_seconds = Some(policy.window_seconds());
            }
            Some(_) => {}
        }
        self.evidence
            .increment_by(batch.event(), batch.occurrences())
    }

    pub(crate) fn reconcile_clock(&mut self, now: UnixTime, policy: ReliabilityPolicy) -> bool {
        let observed = policy.epoch_start(now);
        let Some(current) = self.epoch_start else {
            return false;
        };
        if current <= observed {
            return false;
        }
        self.epoch_start = Some(observed);
        true
    }

    pub(crate) fn reset_for_policy_change(&mut self, policy: ReliabilityPolicy) -> bool {
        let has_mismatched_epoch =
            self.epoch_start.is_some() && self.window_seconds != Some(policy.window_seconds());
        if !has_mismatched_epoch {
            return false;
        }
        *self = Self::default();
        true
    }
}

/// Stably order peers by advisory reliability without changing the candidate set.
///
/// Law: the output is a stable permutation of the input peers.
pub fn order_peers_by_reliability<P>(
    peers: impl IntoIterator<Item = (P, ReliabilityClass)>,
) -> Vec<P> {
    let mut ranked = peers
        .into_iter()
        .enumerate()
        .map(|(index, (peer, class))| (class.connection_rank(), index, peer))
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(rank, index, _)| (*rank, *index));
    ranked.into_iter().map(|(_, _, peer)| peer).collect()
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;

    fn policy() -> ReliabilityPolicy {
        ReliabilityPolicy::new(60, 1, ReliabilityThresholds::new(3, 4, 5))
            .unwrap_or_else(|error| unreachable_policy(error))
    }

    fn unreachable_policy(error: PolicyError) -> ! {
        panic!("test policy must be valid: {error}")
    }

    #[test]
    fn classification_witnesses_all_three_states() {
        let policy = policy();
        assert_eq!(
            ReliabilityEvidence::default().classify_with_policy(policy),
            ReliabilityClass::Unknown
        );
        assert_eq!(
            ReliabilityEvidence::new(1, 0, 0, 0, 0, 0).classify_with_policy(policy),
            ReliabilityClass::Healthy
        );
        assert_eq!(
            ReliabilityEvidence::new(1, 3, 0, 0, 0, 0).classify_with_policy(policy),
            ReliabilityClass::Degraded
        );
    }

    #[test]
    fn positive_evidence_below_policy_minimum_remains_unknown() {
        let policy = ReliabilityPolicy::new(60, 2, ReliabilityThresholds::new(3, 4, 5))
            .unwrap_or_else(|error| unreachable_policy(error));
        assert_eq!(
            ReliabilityEvidence::new(1, 0, 0, 0, 0, 0).classify_with_policy(policy),
            ReliabilityClass::Unknown
        );
        assert_eq!(
            ReliabilityEvidence::new(1, 0, 1, 0, 0, 0).classify_with_policy(policy),
            ReliabilityClass::Healthy
        );
    }

    #[test]
    fn epoch_rollover_discards_stale_evidence() {
        let mut window = ReliabilityWindow::default();
        let policy = policy();
        assert_eq!(
            window.observe(MeasurementEvent::Connected, UnixTime::from_secs(59), policy),
            Ok(())
        );
        assert_eq!(
            window
                .evidence_at(UnixTime::from_secs(59), policy)
                .map(|evidence| evidence.connected),
            Ok(1)
        );
        assert_eq!(
            window
                .evidence_at(UnixTime::from_secs(60), policy)
                .map(|evidence| evidence.connected),
            Ok(0)
        );
        assert_eq!(
            window.observe(
                MeasurementEvent::FailedToSend,
                UnixTime::from_secs(60),
                policy
            ),
            Ok(())
        );
        assert_eq!(window.stored_evidence().connected, 0);
        assert_eq!(window.stored_evidence().failed_to_send, 1);
    }

    #[test]
    fn clock_regression_is_rejected_without_mutation() {
        let mut window = ReliabilityWindow::default();
        let policy = policy();
        assert_eq!(
            window.observe(
                MeasurementEvent::Connected,
                UnixTime::from_secs(120),
                policy
            ),
            Ok(())
        );
        let before = window;
        assert!(matches!(
            window.observe(
                MeasurementEvent::Disconnected,
                UnixTime::from_secs(59),
                policy
            ),
            Err(MeasureError::ClockRegression { .. })
        ));
        assert_eq!(window, before);
    }

    #[test]
    fn changing_window_at_the_same_time_is_an_explicit_policy_error() {
        let mut window = ReliabilityWindow::default();
        let short = policy();
        let long = ReliabilityPolicy::new(3_600, 1, short.thresholds())
            .unwrap_or_else(|error| unreachable_policy(error));
        assert!(window
            .observe(MeasurementEvent::Connected, UnixTime::from_secs(120), short,)
            .is_ok());

        assert_eq!(
            window.evidence_at(UnixTime::from_secs(120), long),
            Err(MeasureError::ReliabilityWindowMismatch {
                stored_seconds: 60,
                supplied_seconds: 3_600,
            })
        );
    }

    #[test]
    fn ordering_is_a_stable_permutation() {
        let input = [
            (1, ReliabilityClass::Degraded),
            (2, ReliabilityClass::Unknown),
            (3, ReliabilityClass::Healthy),
            (4, ReliabilityClass::Unknown),
            (5, ReliabilityClass::Healthy),
        ];
        assert_eq!(order_peers_by_reliability(input), vec![3, 5, 2, 4, 1]);
    }
}
