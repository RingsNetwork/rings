//! Authorization boundary for native underlay targets that need host-route exclusions.

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

/// Host policy that must authorize direct underlay IPs before native traffic reaches them.
///
/// Installing a policy also switches newly-created native WebRTC connections to relay-only ICE.
/// Remote SDP therefore cannot grant itself host-route exclusions; only explicit
/// signaling/bootstrap endpoints pass through this authorization boundary. A transport without an
/// installed policy preserves its ordinary direct-ICE behavior.
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

/// Hold this guard through connection creation or explicit-target authorization. A policy
/// replacement takes the write lock, so gateway activation cannot race an older direct-ICE
/// connection into the pool or an authorization decision.
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
    async fn installed_policy_authorizes_explicit_targets() {
        let shared = shared_admission();
        let observed = Arc::new(Mutex::new(Vec::new()));
        *shared.write().await = Some(Arc::new(RecordingAdmission {
            observed: Arc::clone(&observed),
        }));
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
    async fn policy_replacement_waits_for_in_flight_authorization_guard() {
        let shared = shared_admission();
        let guard = admission_policy(&shared).await;
        let replacement_shared = Arc::clone(&shared);
        let replacement = tokio::spawn(async move {
            *replacement_shared.write().await = None;
        });

        tokio::task::yield_now().await;
        assert!(!replacement.is_finished());
        drop(guard);
        replacement.await.expect("policy replacement task");
    }
}
