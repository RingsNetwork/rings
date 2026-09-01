//! Native operating-system effect boundaries.

use crate::GatewayError;
use crate::GatewayPlan;
use crate::PacketIo;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod native;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod route;
pub(crate) mod routes;
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

/// Failed cleanup together with the still-live linear cleanup capability.
///
/// Callers may inspect the error and retry [`TunnelControl::teardown`] with the returned lease.
/// Dropping the failure does not promise cleanup.
pub struct TeardownFailure<L> {
    lease: L,
    error: GatewayError,
}

impl<L> std::fmt::Debug for TeardownFailure<L> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TeardownFailure")
            .field("lease", &"<retained>")
            .field("error", &self.error)
            .finish()
    }
}

impl<L> std::fmt::Display for TeardownFailure<L> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

impl<L: 'static> std::error::Error for TeardownFailure<L> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl<L> TeardownFailure<L> {
    /// Pair a failed cleanup with the lease required to retry it.
    pub const fn new(lease: L, error: GatewayError) -> Self {
        Self { lease, error }
    }

    /// Borrow the cleanup failure without consuming the retained lease.
    pub const fn error(&self) -> &GatewayError {
        &self.error
    }

    /// Recover both the linear cleanup capability and its failure.
    pub fn into_parts(self) -> (L, GatewayError) {
        (self.lease, self.error)
    }

    /// Consume the retained lease and return only the failure.
    pub fn into_error(self) -> GatewayError {
        self.error
    }
}

/// Platform boundary for establishing and reconciling tunnel resources.
#[async_trait::async_trait]
pub trait TunnelControl: Send {
    /// Packet device produced by this platform.
    type Device: PacketIo;
    /// Linear cleanup capability produced by a successful establish operation.
    type Lease: Send;

    /// Reconcile stale state, then establish the packet interface and explicit capture routes.
    async fn establish(
        &mut self,
        plan: &GatewayPlan,
    ) -> Result<EstablishedTunnel<Self::Device, Self::Lease>, GatewayError>;

    /// Tear down exactly the resources represented by `lease`.
    ///
    /// Failure returns the lease so a transient platform error remains retryable.
    async fn teardown(&mut self, lease: Self::Lease) -> Result<(), TeardownFailure<Self::Lease>>;
}

/// Fail-closed packet boundary for native targets whose packet binding is deferred.
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
/// iOS and Android packet bindings are deferred. This controller performs no system calls and
/// fails before packet admission rather than silently selecting a direct path.
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

    async fn teardown(&mut self, lease: Self::Lease) -> Result<(), TeardownFailure<Self::Lease>> {
        Err(TeardownFailure::new(lease, Self::unsupported()))
    }
}
