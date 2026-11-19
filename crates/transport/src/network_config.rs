//! Model-complete network parameter and transport medium kinematics algebra (#1291).
//!
//! A transport medium is defined purely by its algebraic and physical properties
//! ([`TransportKinematics`] and [`NetworkParameters`]) rather than by what protocol it is:
//! - **Duplexity** ([`DuplexMode`]): `FullDuplex` (simultaneous TX/RX) vs `HalfDuplex`
//!   (turn-taking shared wave medium such as HF/LoRa packet radio or ultrasonic acoustic links).
//! - **Addressing / Discovery Scope** ([`AddressingScope`]): `ProximityAdHoc` (Bluetooth BLE/L2CAP,
//!   ultrasonic acoustic ad-hoc, line-of-sight packet radio), `DirectRouted` (TCP/IP, QUIC,
//!   HTTPS/WebSocket), or `AssistedTraversal` (NAT/firewall relay traversal).
//! - **Handshake Semantic** ([`HandshakeSemantic`]): how the 3-step `create_offer -> answer_offer
//!   -> accept_answer` morphism maps onto the medium (`OfferAnswer`, `DirectDial`, or
//!   `BroadcastBeacon`).
//! - **Frame & Lane Capacity**: `max_frame_payload_bytes` ($1 \le M_{\max} \le 65536$) and
//!   `ordered_lanes` ($1 \le L \le \text{CHANNEL\_POOL\_SIZE}$, with $\text{HalfDuplex} \implies L = 1$).
//! - **Temporal Monotonicity** ([`TransportTimingBudget`]):
//!   $0 < \text{close} \le \text{send\_accept} \le \text{delivery}$ and
//!   $0 < \text{channel\_open} \le \text{handshake}$.

use std::collections::BTreeMap;
use std::fmt;
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
use std::sync::Arc;
use std::time::Duration;

use rings_runtime::MaybeSendSync;

use crate::core::pool::CHANNEL_POOL_SIZE;
#[cfg(any(
    feature = "dummy",
    feature = "native-webrtc",
    all(feature = "web-sys-webrtc", target_family = "wasm")
))]
use crate::core::transport::SharedTransport;
use crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE;

/// Physical transmission duplexity of a transport medium.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DuplexMode {
    /// Simultaneous bidirectional transmission without physical medium collision
    /// (e.g. TCP/IP, HTTPS/WebSocket, SCTP, Bluetooth L2CAP CoC).
    FullDuplex,
    /// Shared wave/carrier medium requiring turn-taking or CSMA/TDMA arbitration
    /// (e.g. HF/VHF/LoRa packet radio, ultrasonic acoustic ad-hoc).
    ///
    /// Invariant: `HalfDuplex` media must serialize traffic over a single ordered lane
    /// (`ordered_lanes == 1`) so concurrent lane transmissions cannot collide on the carrier.
    HalfDuplex,
}

/// Reachability and peer-discovery scope of a transport medium.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AddressingScope {
    /// Physical neighborhood discovery without IP routing (e.g. Bluetooth ad-hoc, ultrasonic
    /// acoustic ad-hoc, line-of-sight packet radio).
    ProximityAdHoc,
    /// Directly dialable network locator (e.g. traditional TCP/IP, QUIC, HTTPS/WebSocket).
    DirectRouted,
    /// Requires rendezvous or relay assistance to traverse NATs/firewalls.
    AssistedTraversal,
}

/// How the abstract 3-step `create_offer -> answer_offer -> accept_answer` handshake maps onto
/// the underlying medium.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum HandshakeSemantic {
    /// Asymmetric session-description negotiation (e.g. SDP or cryptographic session offer/answer).
    OfferAnswer,
    /// Initiator advertises dialable locator/ephemeral key; responder confirms socket/stream bind.
    DirectDial,
    /// Proximity beacon/channel parameter exchange (e.g. ultrasonic carrier frequency & hopping
    /// seed, Bluetooth PSM/UUID, or packet-radio frequency & callsign).
    BroadcastBeacon,
}

/// Per-stage timeout budget of a transport medium.
///
/// Invariant (Temporal Monotonicity):
/// - Every timeout is strictly positive (`> Duration::ZERO`).
/// - `close_timeout <= send_accept_timeout <= delivery_timeout`.
/// - `channel_open_timeout <= handshake_timeout`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TransportTimingBudget {
    /// Maximum duration to complete an offer/answer handshake exchange.
    pub handshake_timeout: Duration,
    /// Maximum duration after handshake for the payload channel to become open.
    pub channel_open_timeout: Duration,
    /// Maximum duration for a single frame to be accepted into the local outbound channel.
    pub send_accept_timeout: Duration,
    /// Maximum duration for an accepted frame to be delivered across the medium.
    pub delivery_timeout: Duration,
    /// Maximum duration to close and release a physical connection generation.
    pub close_timeout: Duration,
}

impl TransportTimingBudget {
    /// Validate the temporal monotonicity invariant of this timing budget.
    pub const fn validate(&self) -> Result<(), TransportKinematicsError> {
        if self.handshake_timeout.is_zero()
            || self.channel_open_timeout.is_zero()
            || self.send_accept_timeout.is_zero()
            || self.delivery_timeout.is_zero()
            || self.close_timeout.is_zero()
        {
            return Err(TransportKinematicsError::ZeroTimeout);
        }
        if self.close_timeout.as_nanos() > self.send_accept_timeout.as_nanos()
            || self.send_accept_timeout.as_nanos() > self.delivery_timeout.as_nanos()
            || self.channel_open_timeout.as_nanos() > self.handshake_timeout.as_nanos()
        {
            return Err(TransportKinematicsError::NonMonotoneTimeouts);
        }
        Ok(())
    }
}

/// Complete physical and link-layer kinematic profile of a transport medium.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TransportKinematics {
    /// Transmission duplexity (`FullDuplex` or `HalfDuplex`).
    pub duplexity: DuplexMode,
    /// Addressing and reachability scope (`ProximityAdHoc`, `DirectRouted`, `AssistedTraversal`).
    pub scope: AddressingScope,
    /// Handshake interpretation on this medium.
    pub handshake: HandshakeSemantic,
    /// Maximum unfragmented payload bytes per transport frame ($1 \le M_{\max} \le 65536$).
    pub max_frame_payload_bytes: usize,
    /// Number of independent ordered lanes multiplexed over a connection ($1 \le L \le 4$).
    pub ordered_lanes: u8,
    /// Medium-specific timeout budget.
    pub timing: TransportTimingBudget,
}

impl TransportKinematics {
    /// Kinematic profile for low-latency directly routed transports (traditional TCP/IP,
    /// HTTPS/WebSocket, local loopback).
    pub const fn low_latency_routed() -> Self {
        Self {
            duplexity: DuplexMode::FullDuplex,
            scope: AddressingScope::DirectRouted,
            handshake: HandshakeSemantic::DirectDial,
            max_frame_payload_bytes: MAX_CHANNEL_MESSAGE_SIZE,
            ordered_lanes: CHANNEL_POOL_SIZE,
            timing: TransportTimingBudget {
                handshake_timeout: Duration::from_secs(15),
                channel_open_timeout: Duration::from_secs(5),
                send_accept_timeout: Duration::from_secs(5),
                delivery_timeout: Duration::from_secs(15),
                close_timeout: Duration::from_secs(2),
            },
        }
    }

    /// Kinematic profile for NAT/firewall traversal-assisted transports.
    pub const fn assisted_traversal() -> Self {
        Self {
            duplexity: DuplexMode::FullDuplex,
            scope: AddressingScope::AssistedTraversal,
            handshake: HandshakeSemantic::OfferAnswer,
            max_frame_payload_bytes: MAX_CHANNEL_MESSAGE_SIZE,
            ordered_lanes: CHANNEL_POOL_SIZE,
            timing: TransportTimingBudget {
                handshake_timeout: Duration::from_secs(30),
                channel_open_timeout: Duration::from_secs(8),
                send_accept_timeout: Duration::from_secs(8),
                delivery_timeout: Duration::from_secs(20),
                close_timeout: Duration::from_secs(3),
            },
        }
    }

    /// Kinematic profile for Bluetooth Ad-Hoc (BLE / L2CAP Connection-Oriented Channels).
    pub const fn bluetooth_adhoc() -> Self {
        Self {
            duplexity: DuplexMode::FullDuplex,
            scope: AddressingScope::ProximityAdHoc,
            handshake: HandshakeSemantic::BroadcastBeacon,
            max_frame_payload_bytes: 1024,
            ordered_lanes: 2,
            timing: TransportTimingBudget {
                handshake_timeout: Duration::from_secs(20),
                channel_open_timeout: Duration::from_secs(10),
                send_accept_timeout: Duration::from_secs(10),
                delivery_timeout: Duration::from_secs(30),
                close_timeout: Duration::from_secs(3),
            },
        }
    }

    /// Kinematic profile for narrowband Packet Radio (HF/VHF AX.25, LoRa ad-hoc mesh).
    pub const fn packet_radio() -> Self {
        Self {
            duplexity: DuplexMode::HalfDuplex,
            scope: AddressingScope::ProximityAdHoc,
            handshake: HandshakeSemantic::BroadcastBeacon,
            max_frame_payload_bytes: 255,
            ordered_lanes: 1,
            timing: TransportTimingBudget {
                handshake_timeout: Duration::from_secs(60),
                channel_open_timeout: Duration::from_secs(30),
                send_accept_timeout: Duration::from_secs(30),
                delivery_timeout: Duration::from_secs(120),
                close_timeout: Duration::from_secs(5),
            },
        }
    }

    /// Kinematic profile for Ultrasonic Ad-Hoc (half-duplex acoustic air medium).
    pub const fn ultrasonic_adhoc() -> Self {
        Self {
            duplexity: DuplexMode::HalfDuplex,
            scope: AddressingScope::ProximityAdHoc,
            handshake: HandshakeSemantic::BroadcastBeacon,
            max_frame_payload_bytes: 128,
            ordered_lanes: 1,
            timing: TransportTimingBudget {
                handshake_timeout: Duration::from_secs(45),
                channel_open_timeout: Duration::from_secs(20),
                send_accept_timeout: Duration::from_secs(25),
                delivery_timeout: Duration::from_secs(90),
                close_timeout: Duration::from_secs(5),
            },
        }
    }

    /// Validate all algebraic invariants of this kinematic profile:
    /// - **K1 (Frame Capacity Bound)**: `1 <= max_frame_payload_bytes <= MAX_CHANNEL_MESSAGE_SIZE`.
    /// - **K2 (Lane Multiplexing & Half-Duplex Serialization)**:
    ///   `1 <= ordered_lanes <= CHANNEL_POOL_SIZE` and `HalfDuplex => ordered_lanes == 1`.
    /// - **K3 (Temporal Monotonicity)**: `timing.validate()`.
    pub const fn validate(&self) -> Result<(), TransportKinematicsError> {
        if self.max_frame_payload_bytes == 0 {
            return Err(TransportKinematicsError::ZeroFramePayload);
        }
        if self.max_frame_payload_bytes > MAX_CHANNEL_MESSAGE_SIZE {
            return Err(TransportKinematicsError::FramePayloadExceedsCeiling {
                requested: self.max_frame_payload_bytes,
                ceiling: MAX_CHANNEL_MESSAGE_SIZE,
            });
        }
        if self.ordered_lanes == 0 {
            return Err(TransportKinematicsError::ZeroOrderedLanes);
        }
        if self.ordered_lanes > CHANNEL_POOL_SIZE {
            return Err(TransportKinematicsError::OrderedLanesExceedPool {
                requested: self.ordered_lanes,
                pool_size: CHANNEL_POOL_SIZE,
            });
        }
        if matches!(self.duplexity, DuplexMode::HalfDuplex) && self.ordered_lanes > 1 {
            return Err(TransportKinematicsError::HalfDuplexMultilaneUnsupported {
                ordered_lanes: self.ordered_lanes,
            });
        }
        self.timing.validate()
    }
}

/// Invariant violation in a [`TransportKinematics`] profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportKinematicsError {
    /// `max_frame_payload_bytes` must be at least 1 byte.
    ZeroFramePayload,
    /// `max_frame_payload_bytes` exceeds [`MAX_CHANNEL_MESSAGE_SIZE`].
    FramePayloadExceedsCeiling {
        /// Configured frame payload size.
        requested: usize,
        /// Hard protocol ceiling (`MAX_CHANNEL_MESSAGE_SIZE`).
        ceiling: usize,
    },
    /// `ordered_lanes` must be at least 1.
    ZeroOrderedLanes,
    /// `ordered_lanes` exceeds [`CHANNEL_POOL_SIZE`].
    OrderedLanesExceedPool {
        /// Configured lane count.
        requested: u8,
        /// Maximum supported lane pool size (`CHANNEL_POOL_SIZE`).
        pool_size: u8,
    },
    /// A `HalfDuplex` medium cannot multiplex multiple simultaneous lanes without carrier collision.
    HalfDuplexMultilaneUnsupported {
        /// Rejected lane count (`> 1`).
        ordered_lanes: u8,
    },
    /// At least one timeout in [`TransportTimingBudget`] is zero.
    ZeroTimeout,
    /// Timeouts violate `close <= send_accept <= delivery` or `channel_open <= handshake`.
    NonMonotoneTimeouts,
}

impl fmt::Display for TransportKinematicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroFramePayload => {
                write!(f, "transport max_frame_payload_bytes must be non-zero")
            }
            Self::FramePayloadExceedsCeiling { requested, ceiling } => write!(
                f,
                "transport max_frame_payload_bytes {requested} exceeds ceiling {ceiling}"
            ),
            Self::ZeroOrderedLanes => write!(f, "transport ordered_lanes must be non-zero"),
            Self::OrderedLanesExceedPool {
                requested,
                pool_size,
            } => write!(
                f,
                "transport ordered_lanes {requested} exceeds pool size {pool_size}"
            ),
            Self::HalfDuplexMultilaneUnsupported { ordered_lanes } => write!(
                f,
                "half-duplex transport medium requires ordered_lanes == 1, got {ordered_lanes}"
            ),
            Self::ZeroTimeout => write!(f, "transport timing budget timeouts must be non-zero"),
            Self::NonMonotoneTimeouts => write!(
                f,
                "transport timing budget violates close <= send_accept <= delivery or channel_open <= handshake"
            ),
        }
    }
}

impl std::error::Error for TransportKinematicsError {}

/// Error returned when validating or instantiating [`NetworkParameters`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportParameterError {
    /// Medium kinematic profile violated an algebraic invariant.
    Kinematics(TransportKinematicsError),
    /// Medium-specific UDP port range constraint is invalid.
    UdpPortRange(UdpPortRangeError),
    /// Custom medium parameter validation failed.
    InvalidParameter(String),
}

impl From<TransportKinematicsError> for TransportParameterError {
    fn from(err: TransportKinematicsError) -> Self {
        Self::Kinematics(err)
    }
}

impl From<UdpPortRangeError> for TransportParameterError {
    fn from(err: UdpPortRangeError) -> Self {
        Self::UdpPortRange(err)
    }
}

impl fmt::Display for TransportParameterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kinematics(err) => Display::fmt(err, f),
            Self::UdpPortRange(err) => Display::fmt(err, f),
            Self::InvalidParameter(msg) => write!(f, "invalid transport network parameter: {msg}"),
        }
    }
}

use std::fmt::Display;
impl std::error::Error for TransportParameterError {}

/// A validated inclusive local UDP port range `[min, max]`.
///
/// `None` at the caller boundary means "use the OS/default ephemeral UDP port range".
/// A present [`UdpPortRange`] always carries two non-zero bounds with `min <= max`.
///
/// Invariant: `1 <= min <= max <= u16::MAX`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UdpPortRange {
    min: u16,
    max: u16,
}

impl UdpPortRange {
    /// Build a validated inclusive UDP port range `[min, max]`.
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

/// Universal network parameters trait used to customize and instantiate any transport medium.
///
/// Defined purely by universal transport properties (`kind`, `kinematics`, optional
/// `endpoint_hint`, optional `session_credential_template`, and `parameter_map`) so heterogeneous
/// media — Packet Radio, Bluetooth Ad-Hoc, Ultrasonic Ad-Hoc, Traditional TCP/IP, HTTPS/WebSocket,
/// and Assisted Traversal — share a single model-complete interface without forcing IP/UDP or
/// WebRTC concepts onto non-IP transports.
pub trait NetworkParameters: fmt::Debug + MaybeSendSync {
    /// Canonical transport medium identifier (e.g. `"assisted-traversal"`, `"tcp-ip"`,
    /// `"https-ws"`, `"bluetooth-adhoc"`, `"ultrasonic-adhoc"`, `"packet-radio"`).
    fn kind(&self) -> &'static str {
        "assisted-traversal"
    }

    /// Physical and link-layer kinematic properties of this transport medium.
    fn kinematics(&self) -> TransportKinematics {
        TransportKinematics::assisted_traversal()
    }

    /// Return the external endpoint/locator hint advertised by this node, if any.
    fn endpoint_hint(&self) -> Option<&str> {
        None
    }

    /// Return the raw session-credential template string if this medium uses session-signed
    /// traversal or gateway credentials, or `None` for proximity/ad-hoc or unauthenticated media.
    fn session_credential_template(&self) -> Option<&str> {
        None
    }

    /// Export medium-specific parameters as a canonical key-value dictionary for
    /// transport-agnostic configuration serialization and inspection.
    fn parameter_map(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// Validate all kinematic and medium-specific invariants before transport construction.
    fn validate_parameters(&self) -> Result<(), TransportParameterError> {
        self.kinematics()
            .validate()
            .map_err(TransportParameterError::from)
    }

    /// Instantiate a [`SharedTransport`] customized with these network parameters and the
    /// given initial session-bound credential (if any).
    #[cfg(any(
        feature = "dummy",
        feature = "native-webrtc",
        all(feature = "web-sys-webrtc", target_family = "wasm")
    ))]
    fn build_transport(
        &self,
        initial_session_credential: Option<&str>,
    ) -> Result<SharedTransport, TransportParameterError>;
}

/// Network parameters for an assisted-traversal IP transport.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TransportNetworkConfig {
    traversal_servers: String,
    endpoint_hint: Option<String>,
    udp_port_range: Option<UdpPortRange>,
}

impl TransportNetworkConfig {
    /// Construct a new [`TransportNetworkConfig`].
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

    /// Return the configured local UDP port range constraint, if any.
    pub const fn udp_port_range(&self) -> Option<UdpPortRange> {
        self.udp_port_range
    }

    /// Return the configured traversal server specification string.
    pub fn traversal_servers(&self) -> &str {
        &self.traversal_servers
    }
}

impl NetworkParameters for TransportNetworkConfig {
    fn kind(&self) -> &'static str {
        "assisted-traversal"
    }

    fn kinematics(&self) -> TransportKinematics {
        TransportKinematics::assisted_traversal()
    }

    fn endpoint_hint(&self) -> Option<&str> {
        self.endpoint_hint.as_deref()
    }

    fn session_credential_template(&self) -> Option<&str> {
        Some(&self.traversal_servers)
    }

    fn parameter_map(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        map.insert(
            "traversal_servers".to_string(),
            self.traversal_servers.clone(),
        );
        if let Some(ref endpoint) = self.endpoint_hint {
            map.insert("endpoint_hint".to_string(), endpoint.clone());
        }
        if let Some(range) = self.udp_port_range {
            map.insert("udp_port_min".to_string(), range.min().to_string());
            map.insert("udp_port_max".to_string(), range.max().to_string());
        }
        map
    }

    #[cfg(any(
        feature = "dummy",
        feature = "native-webrtc",
        all(feature = "web-sys-webrtc", target_family = "wasm")
    ))]
    fn build_transport(
        &self,
        initial_session_credential: Option<&str>,
    ) -> Result<SharedTransport, TransportParameterError> {
        self.validate_parameters()?;
        let credentials = initial_session_credential.unwrap_or(&self.traversal_servers);
        #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
        {
            Ok(Arc::new(crate::connections::DummyTransport::new(
                credentials,
                self.endpoint_hint.clone(),
                self.udp_port_range,
            )))
        }
        #[cfg(all(
            feature = "native-webrtc",
            not(feature = "dummy"),
            not(target_family = "wasm")
        ))]
        {
            Ok(Arc::new(crate::connections::WebrtcTransport::new(
                credentials,
                self.endpoint_hint.clone(),
                self.udp_port_range,
            )))
        }
        #[cfg(all(feature = "web-sys-webrtc", target_family = "wasm"))]
        {
            Ok(Arc::new(crate::connections::WebSysWebrtcTransport::new(
                credentials,
                self.endpoint_hint.clone(),
                self.udp_port_range,
            )))
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

    /// Formal model completeness check for [`TransportKinematics`]:
    /// - All 5 canonical medium profiles (`low_latency_routed`, `assisted_traversal`,
    ///   `bluetooth_adhoc`, `packet_radio`, `ultrasonic_adhoc`) satisfy K1, K2, K3.
    /// - Every boundary violation (`ZeroFramePayload`, `FramePayloadExceedsCeiling`,
    ///   `ZeroOrderedLanes`, `OrderedLanesExceedPool`, `HalfDuplexMultilaneUnsupported`,
    ///   `ZeroTimeout`, `NonMonotoneTimeouts`) is soundness-checked and rejected.
    #[test]
    fn test_transport_kinematics_canonical_profiles_and_invariant_boundaries() {
        let canonical = [
            TransportKinematics::low_latency_routed(),
            TransportKinematics::assisted_traversal(),
            TransportKinematics::bluetooth_adhoc(),
            TransportKinematics::packet_radio(),
            TransportKinematics::ultrasonic_adhoc(),
        ];
        for profile in canonical {
            assert_eq!(
                profile.validate(),
                Ok(()),
                "profile {profile:?} must be valid"
            );
        }

        // K1 violations
        let mut zero_frame = TransportKinematics::low_latency_routed();
        zero_frame.max_frame_payload_bytes = 0;
        assert_eq!(
            zero_frame.validate(),
            Err(TransportKinematicsError::ZeroFramePayload)
        );

        let mut oversized_frame = TransportKinematics::low_latency_routed();
        oversized_frame.max_frame_payload_bytes = MAX_CHANNEL_MESSAGE_SIZE + 1;
        assert_eq!(
            oversized_frame.validate(),
            Err(TransportKinematicsError::FramePayloadExceedsCeiling {
                requested: MAX_CHANNEL_MESSAGE_SIZE + 1,
                ceiling: MAX_CHANNEL_MESSAGE_SIZE,
            })
        );

        // K2 violations
        let mut zero_lanes = TransportKinematics::low_latency_routed();
        zero_lanes.ordered_lanes = 0;
        assert_eq!(
            zero_lanes.validate(),
            Err(TransportKinematicsError::ZeroOrderedLanes)
        );

        let mut too_many_lanes = TransportKinematics::low_latency_routed();
        too_many_lanes.ordered_lanes = CHANNEL_POOL_SIZE + 1;
        assert_eq!(
            too_many_lanes.validate(),
            Err(TransportKinematicsError::OrderedLanesExceedPool {
                requested: CHANNEL_POOL_SIZE + 1,
                pool_size: CHANNEL_POOL_SIZE,
            })
        );

        let mut half_duplex_multilane = TransportKinematics::ultrasonic_adhoc();
        half_duplex_multilane.ordered_lanes = 2;
        assert_eq!(
            half_duplex_multilane.validate(),
            Err(TransportKinematicsError::HalfDuplexMultilaneUnsupported { ordered_lanes: 2 })
        );

        // K3 violations
        let mut zero_timeout = TransportKinematics::packet_radio();
        zero_timeout.timing.close_timeout = Duration::ZERO;
        assert_eq!(
            zero_timeout.validate(),
            Err(TransportKinematicsError::ZeroTimeout)
        );

        let mut inverted_delivery = TransportKinematics::bluetooth_adhoc();
        inverted_delivery.timing.send_accept_timeout = Duration::from_secs(40);
        inverted_delivery.timing.delivery_timeout = Duration::from_secs(30);
        assert_eq!(
            inverted_delivery.validate(),
            Err(TransportKinematicsError::NonMonotoneTimeouts)
        );

        let mut inverted_handshake = TransportKinematics::assisted_traversal();
        inverted_handshake.timing.channel_open_timeout = Duration::from_secs(31);
        inverted_handshake.timing.handshake_timeout = Duration::from_secs(30);
        assert_eq!(
            inverted_handshake.validate(),
            Err(TransportKinematicsError::NonMonotoneTimeouts)
        );
    }
}
