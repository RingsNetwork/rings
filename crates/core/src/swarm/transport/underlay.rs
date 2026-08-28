//! Native underlay candidate projections and admission-policy wiring.

use std::net::IpAddr;
#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;

#[cfg(not(target_family = "wasm"))]
use rings_transport::connections::UnderlayCandidateAdmission;
#[cfg(not(target_family = "wasm"))]
use rings_transport::connections::UnderlayCandidateAdmissionError;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;

use super::SwarmTransport;

impl SwarmTransport {
    /// Return remote underlay candidate IPs from every physical transport in the pool.
    ///
    /// This deliberately does not use the logical connection-lifecycle projections: ICE needs
    /// its bypass routes while a connection is still pending, before it can become routable and
    /// enter the active DHT projection.
    pub(crate) fn underlay_remote_ips(&self) -> Vec<IpAddr> {
        let mut addresses = self
            .transport
            .connections()
            .into_iter()
            .flat_map(|(_, connection)| connection.underlay_remote_ips())
            .collect::<Vec<_>>();
        addresses.sort_unstable();
        addresses.dedup();
        addresses
    }

    /// Install or clear the native policy that gates remote SDP application.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) async fn set_underlay_candidate_admission(
        &self,
        admission: Option<Arc<dyn UnderlayCandidateAdmission>>,
    ) {
        #[cfg(not(feature = "dummy"))]
        self.transport
            .set_underlay_candidate_admission(admission)
            .await;
        #[cfg(feature = "dummy")]
        let _ = admission;
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
