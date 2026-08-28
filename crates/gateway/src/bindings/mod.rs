//! Native operating-system effect boundaries.

use std::net::IpAddr;

use crate::GatewayError;
use crate::GatewayPlan;
use crate::PacketIo;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod native;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod route;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod routes;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use native::NativePacketIo;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use native::NativeTunnelControl;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use native::NativeTunnelLease;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use native::NativeTunnelOptions;

/// Resources returned after a platform tunnel is established.
///
/// Dropping this value does not promise route cleanup. Callers must pass its lease back to
/// [`TunnelControl::teardown`] so cleanup failures remain observable.
pub struct EstablishedTunnel<D, L> {
    /// Packet device owned by the gateway data plane.
    pub device: D,
    /// Platform capability required for deterministic cleanup.
    pub lease: L,
    /// Interface name reported by the platform.
    pub interface_name: String,
}

/// Platform boundary for establishing and reconciling tunnel resources.
#[async_trait::async_trait]
pub trait TunnelControl: Send {
    /// Packet device produced by this platform.
    type Device: PacketIo;
    /// Linear cleanup capability produced by a successful establish operation.
    type Lease: Send;

    /// Reconcile stale state, install exclusions, then establish the capture interface and routes.
    async fn establish(
        &mut self,
        plan: &GatewayPlan,
    ) -> Result<EstablishedTunnel<Self::Device, Self::Lease>, GatewayError>;

    /// Tear down exactly the resources represented by `lease`.
    async fn teardown(&mut self, lease: Self::Lease) -> Result<(), GatewayError>;
}

/// Platform boundary that keeps Rings underlay traffic outside capture routes.
#[async_trait::async_trait]
pub trait UnderlayPolicy: Send {
    /// Replace the current bypass set before packet admission or after peer topology changes.
    async fn replace_bypass_targets(&mut self, targets: &[IpAddr]) -> Result<(), GatewayError>;
}

/// Fail-closed packet boundary for native targets whose VPN integration is deferred.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub struct UnsupportedPacketIo;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[async_trait::async_trait]
impl PacketIo for UnsupportedPacketIo {
    async fn read_packet(&mut self, _packet: &mut [u8]) -> Result<usize, crate::PacketIoError> {
        Err(crate::PacketIoError::Closed)
    }

    async fn write_packet(&mut self, _packet: &[u8]) -> Result<(), crate::PacketIoError> {
        Err(crate::PacketIoError::Closed)
    }
}

/// Linear placeholder lease for an unsupported native gateway platform.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub struct UnsupportedTunnelLease;

/// Explicitly unsupported controller used to preserve non-desktop native node builds.
///
/// iOS NetworkExtension and Android VpnService bindings are deferred. This controller performs no
/// system calls and fails before packet admission rather than silently selecting a direct path.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub struct UnsupportedTunnelControl;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
impl UnsupportedTunnelControl {
    /// Construct a controller that reports the deferred platform boundary.
    pub const fn new() -> Self {
        Self
    }

    fn unsupported() -> GatewayError {
        GatewayError::Platform {
            operation: "native-gateway-platform",
            message: "native gateway bindings are available only on Linux, macOS, and Windows"
                .to_string(),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
impl Default for UnsupportedTunnelControl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[async_trait::async_trait]
impl TunnelControl for UnsupportedTunnelControl {
    type Device = UnsupportedPacketIo;
    type Lease = UnsupportedTunnelLease;

    async fn establish(
        &mut self,
        _plan: &GatewayPlan,
    ) -> Result<EstablishedTunnel<Self::Device, Self::Lease>, GatewayError> {
        Err(Self::unsupported())
    }

    async fn teardown(&mut self, _lease: Self::Lease) -> Result<(), GatewayError> {
        Err(Self::unsupported())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[async_trait::async_trait]
impl UnderlayPolicy for UnsupportedTunnelControl {
    async fn replace_bypass_targets(&mut self, _targets: &[IpAddr]) -> Result<(), GatewayError> {
        Err(Self::unsupported())
    }
}
