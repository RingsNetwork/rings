//! Transport network configuration and UDP port range invariants (#1291).

use std::fmt;
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
use std::sync::Arc;

#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
use crate::core::transport::SharedTransport;

/// A validated inclusive local UDP port range `[min, max]`.
///
/// `None` at the caller boundary means "use the OS/default ephemeral UDP port range".
/// A present [`UdpPortRange`] always carries two non-zero bounds with `min <= max`.
///
/// # Invariant
///
/// `1 <= min <= max <= u16::MAX`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UdpPortRange {
    /// Inclusive lower UDP port bound (`>= 1`).
    min: u16,
    /// Inclusive upper UDP port bound (`>= min`).
    max: u16,
}

impl UdpPortRange {
    /// Build a validated inclusive UDP port range `[min, max]`.
    ///
    /// # Errors
    ///
    /// - [`UdpPortRangeError::ZeroBound`] if either `min` or `max` is `0`.
    /// - [`UdpPortRangeError::Inverted`] if `min > max`.
    pub const fn new(min: u16, max: u16) -> Result<Self, UdpPortRangeError> {
        if min == 0 || max == 0 {
            return Err(UdpPortRangeError::ZeroBound { min, max });
        }
        if min > max {
            return Err(UdpPortRangeError::Inverted { min, max });
        }

        Ok(Self { min, max })
    }

    /// Parse an optional `(min, max)` pair into a validated [`UdpPortRange`], rejecting
    /// half-specified bounds where only one of `min` or `max` is present.
    ///
    /// # Errors
    ///
    /// - [`UdpPortRangeError::IncompleteBounds`] if only one of `min` or `max` is `Some`.
    /// - [`UdpPortRangeError::ZeroBound`] or [`UdpPortRangeError::Inverted`] if both bounds are
    ///   present but violate `1 <= min <= max`.
    pub const fn from_optional_bounds(
        min: Option<u16>,
        max: Option<u16>,
    ) -> Result<Option<Self>, UdpPortRangeError> {
        match (min, max) {
            (None, None) => Ok(None),
            (Some(min), Some(max)) => match Self::new(min, max) {
                Ok(range) => Ok(Some(range)),
                Err(err) => Err(err),
            },
            (min, max) => Err(UdpPortRangeError::IncompleteBounds { min, max }),
        }
    }

    /// The inclusive lower UDP port bound.
    pub const fn min(self) -> u16 {
        self.min
    }

    /// The inclusive upper UDP port bound.
    pub const fn max(self) -> u16 {
        self.max
    }
}

/// Error returned when a UDP port range cannot witness the range invariant `1 <= min <= max`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UdpPortRangeError {
    /// At least one bound is zero. Zero is reserved for the absent/default case.
    ZeroBound {
        /// Rejected lower bound.
        min: u16,
        /// Rejected upper bound.
        max: u16,
    },
    /// The lower bound is greater than the upper bound.
    Inverted {
        /// Rejected lower bound.
        min: u16,
        /// Rejected upper bound.
        max: u16,
    },
    /// Only one side of the port range was provided.
    IncompleteBounds {
        /// Provided lower bound, if any.
        min: Option<u16>,
        /// Provided upper bound, if any.
        max: Option<u16>,
    },
}

impl fmt::Display for UdpPortRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBound { min, max } => write!(
                f,
                "UDP port range bounds must be non-zero: min={min}, max={max}"
            ),
            Self::Inverted { min, max } => {
                write!(f, "UDP port range min must be <= max: min={min}, max={max}")
            }
            Self::IncompleteBounds { min, max } => write!(
                f,
                "Both UDP port range bounds must be set together: min={min:?}, max={max:?}"
            ),
        }
    }
}

impl std::error::Error for UdpPortRangeError {}

/// Default network configuration for the built-in traversal-assisted transport backend.
///
/// Holds the traversal/relay server specification (`traversal_servers`), optional advertised
/// external endpoint locator (`endpoint_hint`), and optional local UDP ephemeral port range
/// (`udp_port_range`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TransportNetworkConfig {
    /// Traversal/relay server specification string (used as the session credential template).
    traversal_servers: String,
    /// Optional external endpoint locator advertised by this node.
    endpoint_hint: Option<String>,
    /// Optional validated inclusive local UDP ephemeral port range `[min, max]`.
    udp_port_range: Option<UdpPortRange>,
}

impl TransportNetworkConfig {
    /// Construct a new [`TransportNetworkConfig`] from a traversal server specification string,
    /// an optional advertised external endpoint locator, and an optional validated [`UdpPortRange`].
    pub fn new(
        traversal_servers: impl Into<String>,
        endpoint_hint: Option<String>,
        udp_port_range: Option<UdpPortRange>,
    ) -> Self {
        Self {
            traversal_servers: traversal_servers.into(),
            endpoint_hint,
            udp_port_range,
        }
    }

    /// Set the optional external endpoint hint advertised by this node.
    pub fn with_endpoint_hint(mut self, endpoint_hint: Option<String>) -> Self {
        self.endpoint_hint = endpoint_hint;
        self
    }

    /// Set the optional local UDP port range constraint `[min, max]`.
    pub const fn with_udp_port_range(mut self, udp_port_range: Option<UdpPortRange>) -> Self {
        self.udp_port_range = udp_port_range;
        self
    }

    /// Return the configured external endpoint hint, if any.
    pub fn endpoint_hint(&self) -> Option<&str> {
        self.endpoint_hint.as_deref()
    }

    /// Return the configured local UDP port range constraint, if any.
    pub const fn udp_port_range(&self) -> Option<UdpPortRange> {
        self.udp_port_range
    }

    /// Return the configured traversal server specification string.
    pub fn traversal_servers(&self) -> &str {
        &self.traversal_servers
    }

    /// Instantiate the active platform's [`SharedTransport`] using `initial_session_credential`
    /// (falling back to the raw `traversal_servers` specification when no signed credential is
    /// supplied).
    #[cfg(any(
        feature = "dummy",
        feature = "native-webrtc",
        all(feature = "web-sys-webrtc", target_family = "wasm")
    ))]
    pub fn build_shared_transport(
        &self,
        initial_session_credential: Option<&str>,
    ) -> SharedTransport {
        let credentials = initial_session_credential.unwrap_or(&self.traversal_servers);
        #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
        {
            Arc::new(crate::connections::DummyTransport::new(
                credentials,
                self.endpoint_hint.clone(),
                self.udp_port_range,
            ))
        }
        #[cfg(all(
            feature = "native-webrtc",
            not(feature = "dummy"),
            not(target_family = "wasm")
        ))]
        {
            Arc::new(crate::connections::WebrtcTransport::new(
                credentials,
                self.endpoint_hint.clone(),
                self.udp_port_range,
            ))
        }
        #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
        {
            // Browser wasm32 is single-threaded; `SharedTransport` unifies native and browser
            // behind `Arc<dyn DynTransportInterface>`.
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(crate::connections::WebSysWebrtcTransport::new(
                credentials,
                self.endpoint_hint.clone(),
                self.udp_port_range,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_udp_port_range_preserves_valid_bounds() {
        let range = UdpPortRange::new(49160, 49200);

        assert_eq!(
            range,
            Ok(UdpPortRange {
                min: 49160,
                max: 49200
            })
        );
    }

    #[test]
    fn test_udp_port_range_rejects_zero_bound() {
        let range = UdpPortRange::new(0, 49200);

        assert_eq!(
            range,
            Err(UdpPortRangeError::ZeroBound { min: 0, max: 49200 })
        );
    }

    #[test]
    fn test_udp_port_range_rejects_inverted_bounds() {
        let range = UdpPortRange::new(49200, 49160);

        assert_eq!(
            range,
            Err(UdpPortRangeError::Inverted {
                min: 49200,
                max: 49160
            })
        );
    }

    #[test]
    fn test_udp_port_range_rejects_incomplete_optional_bounds() {
        assert_eq!(
            UdpPortRange::from_optional_bounds(Some(49160), None),
            Err(UdpPortRangeError::IncompleteBounds {
                min: Some(49160),
                max: None,
            })
        );
        assert_eq!(UdpPortRange::from_optional_bounds(None, None), Ok(None));
    }
}
