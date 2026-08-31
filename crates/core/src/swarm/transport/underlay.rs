//! Native direct-underlay authorization-policy wiring.

#[cfg(not(target_family = "wasm"))]
use std::net::IpAddr;
#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;

#[cfg(not(target_family = "wasm"))]
use rings_transport::connections::UnderlayCandidateAdmission;
#[cfg(not(target_family = "wasm"))]
use rings_transport::connections::UnderlayCandidateAdmissionError;

use super::SwarmTransport;

impl SwarmTransport {
    /// Install the native policy that enables relay-only ICE and gates explicit targets.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) async fn enable_underlay_candidate_admission(
        &self,
        admission: Arc<dyn UnderlayCandidateAdmission>,
    ) -> Result<(), UnderlayCandidateAdmissionError> {
        #[cfg(not(feature = "dummy"))]
        self.transport
            .enable_underlay_candidate_admission(admission)
            .await?;
        #[cfg(feature = "dummy")]
        let _ = admission;
        Ok(())
    }

    /// Clear the native direct-underlay policy after capture is removed.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) async fn clear_underlay_candidate_admission(&self) {
        #[cfg(not(feature = "dummy"))]
        self.transport.clear_underlay_candidate_admission().await;
    }

    /// Return whether the native gateway's explicit-underlay gate is installed.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) async fn underlay_candidate_admission_enabled(&self) -> bool {
        #[cfg(not(feature = "dummy"))]
        {
            self.transport.underlay_candidate_admission_enabled().await
        }
        #[cfg(feature = "dummy")]
        {
            false
        }
    }

    /// Admit direct underlay targets through the installed native gateway policy.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) async fn admit_underlay_targets(
        &self,
        targets: &[IpAddr],
    ) -> Result<(), UnderlayCandidateAdmissionError> {
        #[cfg(not(feature = "dummy"))]
        self.transport.admit_underlay_targets(targets).await?;
        #[cfg(feature = "dummy")]
        let _ = targets;
        Ok(())
    }
}
