//! Onion roles: the symbols one node process registers (#834 D2).
//!
//! A node process is a partial Σ-algebra, and registering any symbol registers `relay`:
//!
//! ```text
//! Σ_n ∈ { ∅, {relay}, {relay} ⊎ Σ_W,n }        Σ_W,n ≠ ∅
//!         Client  Relay   Exit(x)
//! ```
//!
//! [`OnionRole<X>`] is that ladder with the exit's data `X` in the last rung, so "exit without
//! relay" has no inhabitant. `OnionRole` is a functor in `X` ([`OnionRole::map`]); each layer of the
//! node reads the same role at the payload it needs:
//!
//! ```text
//! OnionRole<OnionExitOffer>             configuration: services and policy the exit offers
//!   ↦ OnionRole<NativeOnionTcpExitConfig>  native data plane: the installed exit runtime
//!   ↦ OnionRole<OnionProcessEpoch>         circuit reducer: the epoch exit layers must name
//! ```
//!
//! Every `map` preserves the rung, so the relay capability published in the online-node
//! descriptor, the exit descriptors, and the reducer's admission all agree by construction.

use super::OnionExitPolicy;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;

/// The rung of the registration ladder `∅ ⊂ {relay} ⊂ {relay} ⊎ Σ_W,n` of one node process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionRole<X> {
    /// Registers no symbol: builds circuits, evaluates no position.
    Client,
    /// Registers `relay` only.
    Relay,
    /// Registers `relay` and the world-facing symbols of `X`.
    Exit(X),
}

impl<X> OnionRole<X> {
    /// Return whether this role registers `relay`: every rung above [`OnionRole::Client`].
    pub const fn registers_relay(&self) -> bool {
        !matches!(self, Self::Client)
    }

    /// Return the exit's data when this role registers world-facing symbols.
    pub const fn exit(&self) -> Option<&X> {
        match self {
            Self::Exit(exit) => Some(exit),
            Self::Client | Self::Relay => None,
        }
    }

    /// Borrow the exit's data: `OnionRole<X> → OnionRole<&X>`.
    pub const fn as_ref(&self) -> OnionRole<&X> {
        match self {
            Self::Client => OnionRole::Client,
            Self::Relay => OnionRole::Relay,
            Self::Exit(exit) => OnionRole::Exit(exit),
        }
    }

    /// Relabel the exit's data, preserving the rung: the functor action `OnionRole(f)`.
    pub fn map<Y>(self, exit: impl FnOnce(X) -> Y) -> OnionRole<Y> {
        match self {
            Self::Client => OnionRole::Client,
            Self::Relay => OnionRole::Relay,
            Self::Exit(data) => OnionRole::Exit(exit(data)),
        }
    }
}

/// What an exit offers: a non-empty set of world-facing services under an open policy.
///
/// Invariant: `services` is non-empty and `policy` admits at least one target, so every value
/// is an exit a client can route to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionExitOffer {
    services: Vec<OnionServiceName>,
    policy: OnionExitPolicy,
}

impl OnionExitOffer {
    /// Build an offer, rejecting an empty service set and a closed policy.
    pub fn new(services: Vec<OnionServiceName>, policy: OnionExitPolicy) -> Result<Self> {
        if services.is_empty() {
            return Err(Error::InvalidConfig(
                "an onion exit requires at least one onion_exit_services entry".to_string(),
            ));
        }
        policy.validate_targets()?;
        Ok(Self { services, policy })
    }

    /// Return the offered services.
    pub fn services(&self) -> &[OnionServiceName] {
        self.services.as_slice()
    }

    /// Return the policy shared by every offered service.
    pub const fn policy(&self) -> &OnionExitPolicy {
        &self.policy
    }

    /// Return whether this offer includes `service`.
    pub fn offers(&self, service: &OnionServiceName) -> bool {
        self.services.contains(service)
    }
}

impl OnionRole<OnionExitOffer> {
    /// Parse the textual registration flags of a configuration file or command line.
    ///
    /// The flags are the only place where "exit without relay" can be written; it is rejected
    /// here, once, so no typed role can carry it. `services` and `policy` are read only for an
    /// exit.
    pub fn from_flags(
        advertise_relay: bool,
        advertise_exit: bool,
        services: Vec<OnionServiceName>,
        policy: OnionExitPolicy,
    ) -> Result<Self> {
        match (advertise_relay, advertise_exit) {
            (false, false) => Ok(Self::Client),
            (true, false) => Ok(Self::Relay),
            (true, true) => OnionExitOffer::new(services, policy).map(Self::Exit),
            (false, true) => Err(Error::InvalidConfig(
                "advertise_onion_exit requires advertise_onion_relay because registering any onion symbol registers relay (#834 D2)"
                    .to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnionExitOffer;
    use super::OnionRole;
    use crate::error::Error;
    use crate::error::Result;
    use crate::onion::OnionExitPolicy;
    use crate::onion::OnionServiceName;

    /// An open single-target policy.
    fn open_policy() -> Result<OnionExitPolicy> {
        OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], Vec::new())
    }

    /// The flags parse onto the ladder, and "exit without relay" is rejected.
    #[test]
    fn test_flags_parse_onto_the_registration_ladder() -> Result<()> {
        let services = vec![OnionServiceName::tcp()];

        assert_eq!(
            OnionRole::from_flags(false, false, services.clone(), open_policy()?)?,
            OnionRole::Client
        );
        assert_eq!(
            OnionRole::from_flags(true, false, services.clone(), open_policy()?)?,
            OnionRole::Relay
        );
        assert_eq!(
            OnionRole::from_flags(true, true, services.clone(), open_policy()?)?,
            OnionRole::Exit(OnionExitOffer::new(services.clone(), open_policy()?)?)
        );
        assert!(matches!(
            OnionRole::from_flags(false, true, services, open_policy()?),
            Err(Error::InvalidConfig(message))
                if message.contains("advertise_onion_exit")
                    && message.contains("advertise_onion_relay")
        ));
        Ok(())
    }

    /// An offer needs a service and an open policy.
    #[test]
    fn test_exit_offer_rejects_empty_services_and_closed_policy() -> Result<()> {
        assert!(OnionExitOffer::new(Vec::new(), open_policy()?).is_err());
        assert!(matches!(
            OnionExitOffer::new(vec![OnionServiceName::https()], OnionExitPolicy::default()),
            Err(Error::InvalidConfig(message)) if message.contains("allowed target")
        ));
        Ok(())
    }

    /// `map` preserves the rung, and every rung above `Client` registers `relay`.
    #[test]
    fn test_map_preserves_the_rung() {
        let roles = [OnionRole::Client, OnionRole::Relay, OnionRole::Exit(7_u8)];

        for role in roles {
            let mapped = role.map(u16::from);
            assert_eq!(mapped.registers_relay(), role.registers_relay());
            assert_eq!(mapped.exit().copied(), role.exit().copied().map(u16::from));
        }
        assert!(!OnionRole::<u8>::Client.registers_relay());
        assert!(OnionRole::<u8>::Relay.registers_relay());
    }
}
