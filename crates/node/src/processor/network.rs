//! Model-complete network parameters trait consumed by [`super::ProcessorConfig`],
//! [`super::ProcessorBuilder`], and [`super::Processor`] (#1291).
//!
//! `rings-node` interacts with transport customization solely through [`NodeNetworkParameters`]
//! and [`SharedTransport`]. From first principles, every interaction between a node and its
//! network parameters reduces to three orthogonal operations:
//! 1. **Transport instantiation** ([`NodeNetworkParameters::build_transport`]): validates the
//!    medium's parameters and constructs a [`SharedTransport`] bound to the node's initial
//!    [`DelegateeKey`].
//! 2. **Advertised locator hint** ([`NodeNetworkParameters::endpoint_hint`]): returns an optional
//!    external locator published in the node's online presence descriptor (`None` by default for
//!    proximity ad-hoc media such as packet radio, Bluetooth ad-hoc, ultrasonic ad-hoc, or
//!    unaddressed client nodes).
//! 3. **Session-key rotation reaction** ([`NodeNetworkParameters::on_session_key_rotated`]):
//!    synchronizes the live [`SharedTransport`] when the node rotates its [`DelegateeKey`] in
//!    process (a no-op `Ok(())` by default for media whose link layer does not bind credentials
//!    to the session key).

use std::fmt;

use rings_core::delegation::DelegateeKey;
use rings_runtime::MaybeSendSync;
use rings_transport::core::transport::SharedTransport;
pub use rings_transport::network_config::TransportNetworkConfig;
pub use rings_transport::network_config::UdpPortRange;
pub use rings_transport::network_config::UdpPortRangeError;
use rings_turn::TurnCredentialSigner;

use crate::error::Error;
use crate::error::Result;

/// Trait defining the network parameters that customize and instantiate a node's transport (#1291).
///
/// Only [`build_transport`](Self::build_transport) is required; [`endpoint_hint`](Self::endpoint_hint)
/// and [`on_session_key_rotated`](Self::on_session_key_rotated) carry first-principles defaults
/// (`None` and `Ok(())`) so non-IP or unauthenticated transports (Packet Radio, Bluetooth Ad-Hoc,
/// Ultrasonic Ad-Hoc, direct TCP/IP, HTTPS/WebSocket) never have to stub out relay or UDP concepts.
pub trait NodeNetworkParameters: fmt::Debug + MaybeSendSync {
    /// Validate these network parameters and instantiate a [`SharedTransport`] bound to
    /// `delegatee_key`.
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport>;

    /// Return the external endpoint or locator hint advertised in the node's online presence
    /// descriptor, if any.
    ///
    /// Defaults to `None` (appropriate for proximity ad-hoc media or nodes without a public locator).
    fn endpoint_hint(&self) -> Option<&str> {
        None
    }

    /// Synchronize the live `transport` when the node rotates its active session key to `next_key`.
    ///
    /// Defaults to `Ok(())` for media whose link layer does not carry session-signed credentials.
    fn on_session_key_rotated(
        &self,
        _transport: &SharedTransport,
        _next_key: &DelegateeKey,
    ) -> Result<()> {
        Ok(())
    }
}

/// Implementation of [`NodeNetworkParameters`] for the built-in [`TransportNetworkConfig`].
impl NodeNetworkParameters for TransportNetworkConfig {
    /// Derive initial session-signed traversal credentials from `delegatee_key` (falling back to
    /// the raw server specification if signing is unavailable for a synthetic test key) and
    /// construct the platform's [`SharedTransport`].
    fn build_transport(&self, delegatee_key: &DelegateeKey) -> Result<SharedTransport> {
        let signed = delegatee_key.sign_ice_servers(self.traversal_servers());
        Ok(self.build_shared_transport(signed.as_deref().ok()))
    }

    /// Return the configured external endpoint hint, if any.
    fn endpoint_hint(&self) -> Option<&str> {
        TransportNetworkConfig::endpoint_hint(self)
    }

    /// Re-sign the configured traversal server template with `next_key` and atomically update the
    /// live `transport`'s session credentials.
    fn on_session_key_rotated(
        &self,
        transport: &SharedTransport,
        next_key: &DelegateeKey,
    ) -> Result<()> {
        let signed = next_key
            .sign_ice_servers(self.traversal_servers())
            .map_err(Error::CoreError)?;
        transport.update_session_credentials(&signed);
        Ok(())
    }
}
