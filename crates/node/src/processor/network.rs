//! Model-complete network parameters trait consumed by [`super::ProcessorConfig`],
//! [`super::ProcessorBuilder`], and [`super::Processor`] (#1291).
//!
//! `rings-node` interacts with transport customization solely through [`NodeNetworkParameters`]
//! and [`SharedTransport`]:
//! - Canonical transport medium kind (`kind`)
//! - Physical and link-layer medium kinematics (`kinematics`: duplexity, addressing scope,
//!   handshake semantic, frame capacity, ordered lane count, and monotone timing budget)
//! - Optional external endpoint/locator hint (`endpoint_hint`)
//! - Canonical key-value parameter projection (`parameter_map`)
//! - Optional session-bound credential derivation (`derive_session_credential`, returning
//!   `Ok(None)` for proximity/ad-hoc media such as Packet Radio, Bluetooth Ad-Hoc, or
//!   Ultrasonic Ad-Hoc that need no external relay credentials)
//! - Trait-based transport instantiation (`build_transport`)

use std::collections::BTreeMap;
use std::fmt;

use rings_core::delegation::DelegateeKey;
use rings_runtime::MaybeSendSync;
use rings_transport::core::transport::SharedTransport;
pub use rings_transport::network_config::AddressingScope;
pub use rings_transport::network_config::DuplexMode;
pub use rings_transport::network_config::HandshakeSemantic;
pub use rings_transport::network_config::NetworkParameters;
pub use rings_transport::network_config::TransportKinematics;
pub use rings_transport::network_config::TransportKinematicsError;
pub use rings_transport::network_config::TransportNetworkConfig;
pub use rings_transport::network_config::TransportParameterError;
pub use rings_transport::network_config::TransportTimingBudget;
pub use rings_transport::network_config::UdpPortRange;
pub use rings_transport::network_config::UdpPortRangeError;
use rings_turn::TurnCredentialSigner;

use crate::error::Error;
use crate::error::Result;

/// Universal trait defining the network parameters that customize a node's transport layer (#1291).
///
/// Defined purely by universal transport properties (`kind`, `kinematics`, `endpoint_hint`,
/// `parameter_map`, `derive_session_credential`, and `build_transport`) so `Processor`,
/// `ProcessorBuilder`, and `ProcessorConfig` can drive any transport medium — Packet Radio,
/// Bluetooth Ad-Hoc, Ultrasonic Ad-Hoc, Traditional TCP/IP, HTTPS/WebSocket, or Assisted
/// Traversal — without forcing IP/UDP or WebRTC concepts onto non-IP transports.
pub trait NodeNetworkParameters: fmt::Debug + MaybeSendSync {
    /// Canonical transport medium identifier (e.g. `"assisted-traversal"`, `"tcp-ip"`,
    /// `"https-ws"`, `"bluetooth-adhoc"`, `"ultrasonic-adhoc"`, `"packet-radio"`).
    fn kind(&self) -> &'static str;

    /// Physical and link-layer kinematic properties of this transport medium.
    fn kinematics(&self) -> TransportKinematics;

    /// Return the external endpoint or locator hint advertised by this node, if any.
    fn endpoint_hint(&self) -> Option<&str>;

    /// Export medium-specific parameters as a canonical key-value dictionary.
    fn parameter_map(&self) -> BTreeMap<String, String>;

    /// Validate all kinematic and medium-specific invariants.
    fn validate_parameters(&self) -> Result<()>;

    /// Derive the session-bound transport credential for `delegatee_key`, if this medium
    /// uses session-bound credentials (`Ok(None)` for proximity/ad-hoc or unauthenticated media).
    fn derive_session_credential(&self, delegatee_key: &DelegateeKey) -> Result<Option<String>>;

    /// Instantiate a [`SharedTransport`] customized by these network parameters and bound to
    /// `delegatee_key`.
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport>;
}

/// Blanket adapter bridging any [`rings_transport::network_config::NetworkParameters`]
/// implementation into [`NodeNetworkParameters`], wiring node-level [`DelegateeKey`] session
/// credential derivation into transport construction and live key rotation.
impl<T> NodeNetworkParameters for T
where T: NetworkParameters
{
    /// Delegate canonical medium kind to [`NetworkParameters::kind`].
    fn kind(&self) -> &'static str {
        NetworkParameters::kind(self)
    }

    /// Delegate physical and link-layer kinematic properties to [`NetworkParameters::kinematics`].
    fn kinematics(&self) -> TransportKinematics {
        NetworkParameters::kinematics(self)
    }

    /// Delegate external endpoint locator hint to [`NetworkParameters::endpoint_hint`].
    fn endpoint_hint(&self) -> Option<&str> {
        NetworkParameters::endpoint_hint(self)
    }

    /// Delegate key-value parameter projection to [`NetworkParameters::parameter_map`].
    fn parameter_map(&self) -> BTreeMap<String, String> {
        NetworkParameters::parameter_map(self)
    }

    /// Validate all kinematic and medium-specific parameters via
    /// [`NetworkParameters::validate_parameters`], mapping any violation to [`Error::InvalidConfig`].
    fn validate_parameters(&self) -> Result<()> {
        NetworkParameters::validate_parameters(self)
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }

    /// Derive a session-bound transport credential for `delegatee_key` when
    /// [`NetworkParameters::session_credential_template`] is present, or return `Ok(None)` for
    /// proximity/ad-hoc or unauthenticated media that require no external relay credentials.
    fn derive_session_credential(&self, delegatee_key: &DelegateeKey) -> Result<Option<String>> {
        let Some(template) = self.session_credential_template() else {
            return Ok(None);
        };
        delegatee_key
            .sign_ice_servers(template)
            .map(Some)
            .map_err(Error::CoreError)
    }

    /// Validate all medium parameters, derive the initial session-bound credential for
    /// `delegatee_key` (falling back to the raw template if signing is unavailable for a synthetic
    /// test key), and instantiate the [`SharedTransport`].
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
        NodeNetworkParameters::validate_parameters(self)?;
        let initial_credential = match self.derive_session_credential(delegatee_key) {
            Ok(cred) => cred,
            Err(_) => self.session_credential_template().map(str::to_owned),
        };
        NetworkParameters::build_transport(self, initial_credential.as_deref())
            .map_err(|err| Error::InvalidConfig(err.to_string()))
    }
}
