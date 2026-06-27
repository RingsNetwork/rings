//! WebRTC transport configuration values.

use std::fmt;

/// A validated inclusive UDP port range for native WebRTC ICE gathering.
///
/// `None` at the caller boundary means "use the OS/default WebRTC ephemeral
/// range". A present [`WebrtcUdpPortRange`] always carries two non-zero bounds
/// with `min <= max`.
///
/// Invariant: `1 <= min <= max <= u16::MAX`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebrtcUdpPortRange {
    min: u16,
    max: u16,
}

impl WebrtcUdpPortRange {
    /// Builds a validated inclusive UDP port range.
    pub fn new(min: u16, max: u16) -> Result<Self, WebrtcUdpPortRangeError> {
        if min == 0 || max == 0 {
            return Err(WebrtcUdpPortRangeError::ZeroBound { min, max });
        }
        if min > max {
            return Err(WebrtcUdpPortRangeError::Inverted { min, max });
        }

        Ok(Self { min, max })
    }

    /// The inclusive lower UDP port bound.
    pub fn min(self) -> u16 {
        self.min
    }

    /// The inclusive upper UDP port bound.
    pub fn max(self) -> u16 {
        self.max
    }
}

/// Error returned when a UDP port range cannot witness the range invariant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebrtcUdpPortRangeError {
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
}

impl fmt::Display for WebrtcUdpPortRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBound { min, max } => write!(
                f,
                "WebRTC UDP port range bounds must be non-zero: min={min}, max={max}"
            ),
            Self::Inverted { min, max } => write!(
                f,
                "WebRTC UDP port range min must be <= max: min={min}, max={max}"
            ),
        }
    }
}

impl std::error::Error for WebrtcUdpPortRangeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_port_range_preserves_valid_bounds() {
        let range = WebrtcUdpPortRange::new(49160, 49200);

        assert_eq!(
            range,
            Ok(WebrtcUdpPortRange {
                min: 49160,
                max: 49200
            })
        );
    }

    #[test]
    fn udp_port_range_rejects_zero_bound() {
        let range = WebrtcUdpPortRange::new(0, 49200);

        assert_eq!(
            range,
            Err(WebrtcUdpPortRangeError::ZeroBound { min: 0, max: 49200 })
        );
    }

    #[test]
    fn udp_port_range_rejects_inverted_bounds() {
        let range = WebrtcUdpPortRange::new(49200, 49160);

        assert_eq!(
            range,
            Err(WebrtcUdpPortRangeError::Inverted {
                min: 49200,
                max: 49160
            })
        );
    }
}
