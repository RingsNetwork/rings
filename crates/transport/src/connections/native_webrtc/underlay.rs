//! Admission boundary for native underlay targets that need host-route exclusions.

use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::RwLock;

/// Failure returned when a native host cannot admit underlay targets safely.
#[derive(Debug, thiserror::Error)]
#[error("underlay target admission failed: {message}")]
pub struct UnderlayCandidateAdmissionError {
    message: String,
}

impl UnderlayCandidateAdmissionError {
    /// Preserve a platform-policy diagnostic at the transport boundary.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Host policy that must admit direct underlay IPs before native traffic reaches them.
///
/// Native packet gateways implement this by installing more-specific underlay routes. Callers use
/// the same policy for remote ICE candidates and signaling/bootstrap endpoints, avoiding separate
/// routing mechanisms. A transport without an installed policy preserves its ordinary behavior.
#[async_trait]
pub trait UnderlayCandidateAdmission: Send + Sync {
    /// Admit one complete target set before any direct traffic is allowed to reach it.
    async fn admit(&self, candidates: &[IpAddr]) -> Result<(), UnderlayCandidateAdmissionError>;
}

pub(super) type SharedUnderlayCandidateAdmission =
    Arc<RwLock<Option<Arc<dyn UnderlayCandidateAdmission>>>>;

pub(super) fn shared_admission() -> SharedUnderlayCandidateAdmission {
    Arc::new(RwLock::new(None))
}

pub(super) async fn replace_admission(
    shared: &SharedUnderlayCandidateAdmission,
    admission: Option<Arc<dyn UnderlayCandidateAdmission>>,
) {
    *shared.write().await = admission;
}

/// Hold this guard through remote-description application. A policy replacement takes the write
/// lock, so once gateway registration completes, every older handshake has either published its
/// candidates or failed and every newer handshake observes the gateway policy.
pub(super) async fn admission_policy(
    shared: &SharedUnderlayCandidateAdmission,
) -> OwnedRwLockReadGuard<Option<Arc<dyn UnderlayCandidateAdmission>>> {
    Arc::clone(shared).read_owned().await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct RecordingAdmission {
        observed: Arc<Mutex<Vec<Vec<IpAddr>>>>,
    }

    #[async_trait]
    impl UnderlayCandidateAdmission for RecordingAdmission {
        async fn admit(
            &self,
            candidates: &[IpAddr],
        ) -> Result<(), UnderlayCandidateAdmissionError> {
            self.observed
                .lock()
                .expect("recording admission lock")
                .push(candidates.to_vec());
            Ok(())
        }
    }

    #[tokio::test]
    async fn installed_policy_observes_candidates_before_transport_progress() {
        let shared = shared_admission();
        let observed = Arc::new(Mutex::new(Vec::new()));
        replace_admission(
            &shared,
            Some(Arc::new(RecordingAdmission {
                observed: Arc::clone(&observed),
            })),
        )
        .await;
        let candidates = vec!["203.0.113.9".parse().expect("test candidate")];

        let admission = admission_policy(&shared).await;
        admission
            .as_ref()
            .expect("installed candidate policy")
            .admit(&candidates)
            .await
            .expect("candidate admission");

        assert_eq!(
            observed
                .lock()
                .expect("recording admission lock")
                .as_slice(),
            &[candidates]
        );
    }

    #[tokio::test]
    async fn policy_replacement_waits_for_in_flight_remote_description_guard() {
        let shared = shared_admission();
        let guard = admission_policy(&shared).await;
        let replacement_shared = Arc::clone(&shared);
        let replacement = tokio::spawn(async move {
            replace_admission(&replacement_shared, None).await;
        });

        tokio::task::yield_now().await;
        assert!(!replacement.is_finished());
        drop(guard);
        replacement.await.expect("policy replacement task");
    }
}
