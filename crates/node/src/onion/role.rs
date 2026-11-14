//! Onion roles: the symbols one node process registers (#834 D2).
//!
//! A node process is a partial Σ-algebra, and registering any symbol registers `relay`:
//!
//! ```text
//! Reg_n ∈ { ∅,      {relay}, {relay} ⊎ Reg_I, {relay} ⊎ Reg_W, {relay} ⊎ Reg_W ⊎ Reg_I }
//!           Client  Relay    Invoke(i)        Exit(x)          Both { exit: x, invoke: i }
//! ```
//!
//! [`OnionRole<X, I>`] encodes that lattice so neither "exit without relay" nor "invoke without
//! relay" has an inhabitant (`Reg_n ≠ ∅ ⇒ relay ∈ Reg_n`). `OnionRole` is functorial in `X`
//! ([`OnionRole::map`]) and defaults `I = OnionInvokeOffer`. The processor holds one
//! `OnionRole<OnionExitOffer>`, and every layer of the node reads that value:
//!
//! - online-node relay capability (`registers_relay()`),
//! - world-facing exit descriptors (`exit()`),
//! - intermediate `invoke` descriptors (`invoke()`), and
//! - the data plane's admission and Σ-algebra.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::invoke_descriptor::OnionInvokePolicy;
use super::signature::OnionBackendName;
use super::signature::OnionInvokeShape;
use super::OnionExitPolicy;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;

/// The registration role of one node process over `Σ = {relay} ⊎ Σ_I ⊎ Σ_W` (#834 D2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionRole<X, I = OnionInvokeOffer> {
    /// Registers no symbol: builds loops, evaluates no position.
    Client,
    /// Registers `relay` only.
    Relay,
    /// Registers `relay` and the intermediate `(shape, backend)` pairs of `I`.
    Invoke(I),
    /// Registers `relay` and the world-facing symbols of `X`.
    Exit(X),
    /// Registers `relay`, the world-facing symbols of `X`, and the intermediate pairs of `I`.
    Both {
        /// World-facing exit offer.
        exit: X,
        /// Intermediate `invoke` offer.
        invoke: I,
    },
}

impl<X, I> OnionRole<X, I> {
    /// Return whether this role registers `relay`: every rung above [`OnionRole::Client`].
    pub const fn registers_relay(&self) -> bool {
        !matches!(self, Self::Client)
    }

    /// Return the exit's data when this role registers world-facing symbols.
    pub const fn exit(&self) -> Option<&X> {
        match self {
            Self::Exit(exit) | Self::Both { exit, .. } => Some(exit),
            Self::Client | Self::Relay | Self::Invoke(_) => None,
        }
    }

    /// Return the `invoke` offer when this role registers intermediate `(shape, backend)` pairs.
    pub const fn invoke(&self) -> Option<&I> {
        match self {
            Self::Invoke(invoke) | Self::Both { invoke, .. } => Some(invoke),
            Self::Client | Self::Relay | Self::Exit(_) => None,
        }
    }

    /// Borrow the role's offers: `OnionRole<X, I> → OnionRole<&X, &I>`.
    pub const fn as_ref(&self) -> OnionRole<&X, &I> {
        match self {
            Self::Client => OnionRole::Client,
            Self::Relay => OnionRole::Relay,
            Self::Invoke(invoke) => OnionRole::Invoke(invoke),
            Self::Exit(exit) => OnionRole::Exit(exit),
            Self::Both { exit, invoke } => OnionRole::Both { exit, invoke },
        }
    }

    /// Relabel the exit's data, preserving the rung: the functor action in `X`.
    pub fn map<Y>(self, exit: impl FnOnce(X) -> Y) -> OnionRole<Y, I> {
        match self {
            Self::Client => OnionRole::Client,
            Self::Relay => OnionRole::Relay,
            Self::Invoke(invoke) => OnionRole::Invoke(invoke),
            Self::Exit(data) => OnionRole::Exit(exit(data)),
            Self::Both { exit: data, invoke } => OnionRole::Both {
                exit: exit(data),
                invoke,
            },
        }
    }

    /// Relabel the exit's data by a fallible function, preserving the rung.
    pub fn try_map<Y, E>(
        self,
        exit: impl FnOnce(X) -> std::result::Result<Y, E>,
    ) -> std::result::Result<OnionRole<Y, I>, E> {
        match self {
            Self::Client => Ok(OnionRole::Client),
            Self::Relay => Ok(OnionRole::Relay),
            Self::Invoke(invoke) => Ok(OnionRole::Invoke(invoke)),
            Self::Exit(data) => exit(data).map(OnionRole::Exit),
            Self::Both { exit: data, invoke } => Ok(OnionRole::Both {
                exit: exit(data)?,
                invoke,
            }),
        }
    }

    /// Attach an intermediate `invoke` offer to this role, promoting `Client`/`Relay` to `Invoke`
    /// and `Exit` to `Both` (`Reg_n ≠ ∅ ⇒ relay ∈ Reg_n`).
    pub fn with_invoke(self, invoke: I) -> Self {
        match self {
            Self::Client | Self::Relay | Self::Invoke(_) => Self::Invoke(invoke),
            Self::Exit(exit) | Self::Both { exit, .. } => Self::Both { exit, invoke },
        }
    }
}

/// What a node offers for intermediate `invoke` evaluation: a non-empty map from `(shape, backend)`
/// pairs in `Σ_I × Ω` to their signed `quota(n, f, ω)` policies (#834 D2).
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(
    try_from = "Vec<SerializedOnionInvokeEntry>",
    into = "Vec<SerializedOnionInvokeEntry>"
)]
pub struct OnionInvokeOffer {
    entries: BTreeMap<(OnionInvokeShape, OnionBackendName), OnionInvokePolicy>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct SerializedOnionInvokeEntry {
    shape: OnionInvokeShape,
    backend: OnionBackendName,
    #[serde(default)]
    policy: OnionInvokePolicy,
}

impl From<OnionInvokeOffer> for Vec<SerializedOnionInvokeEntry> {
    fn from(offer: OnionInvokeOffer) -> Self {
        offer
            .entries
            .into_iter()
            .map(|((shape, backend), policy)| SerializedOnionInvokeEntry {
                shape,
                backend,
                policy,
            })
            .collect()
    }
}

impl TryFrom<Vec<SerializedOnionInvokeEntry>> for OnionInvokeOffer {
    type Error = Error;

    fn try_from(entries: Vec<SerializedOnionInvokeEntry>) -> Result<Self> {
        let mut collected = BTreeMap::new();
        for entry in entries {
            if collected
                .insert((entry.shape, entry.backend.clone()), entry.policy)
                .is_some()
            {
                return Err(Error::InvalidConfig(format!(
                    "duplicate onion invoke entry for ({}, {:?})",
                    entry.shape,
                    entry.backend.as_str()
                )));
            }
        }
        if collected.is_empty() {
            return Err(Error::InvalidConfig(
                "an onion invoke offer requires at least one (shape, backend) entry".to_string(),
            ));
        }
        Ok(Self { entries: collected })
    }
}

impl OnionInvokeOffer {
    /// Build an offer from `(shape, backend, policy)` entries, rejecting an empty set.
    pub fn new(
        entries: impl IntoIterator<Item = (OnionInvokeShape, OnionBackendName, OnionInvokePolicy)>,
    ) -> Result<Self> {
        let entries = entries
            .into_iter()
            .map(|(shape, backend, policy)| ((shape, backend), policy))
            .collect::<BTreeMap<_, _>>();
        if entries.is_empty() {
            return Err(Error::InvalidConfig(
                "an onion invoke offer requires at least one (shape, backend) entry".to_string(),
            ));
        }
        Ok(Self { entries })
    }

    /// Iterate over offered `((shape, backend), policy)` entries in canonical order.
    pub fn entries(
        &self,
    ) -> impl Iterator<Item = (&(OnionInvokeShape, OnionBackendName), &OnionInvokePolicy)> {
        self.entries.iter()
    }

    /// Return the signed quota policy of `(shape, backend)`, if offered.
    pub fn policy_for(
        &self,
        shape: OnionInvokeShape,
        backend: &OnionBackendName,
    ) -> Option<&OnionInvokePolicy> {
        self.entries.get(&(shape, backend.clone()))
    }
}

/// What an exit offers: a non-empty set of world-facing services under an open policy.
///
/// Invariant: `services` is non-empty and `policy` admits at least one target, so every value
/// is an exit a client can route to. The services are a set, so each is registered once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionExitOffer {
    services: BTreeSet<OnionServiceName>,
    policy: OnionExitPolicy,
}

impl OnionExitOffer {
    /// Build an offer, rejecting an empty service set and a closed policy.
    pub fn new(
        services: impl IntoIterator<Item = OnionServiceName>,
        policy: OnionExitPolicy,
    ) -> Result<Self> {
        let services = services.into_iter().collect::<BTreeSet<_>>();
        if services.is_empty() {
            return Err(Error::InvalidConfig(
                "an onion exit requires at least one onion_exit_services entry".to_string(),
            ));
        }
        policy.validate_targets()?;
        Ok(Self { services, policy })
    }

    /// Return the offered services, in name order.
    pub const fn services(&self) -> &BTreeSet<OnionServiceName> {
        &self.services
    }

    /// Return the policy shared by every offered service.
    pub const fn policy(&self) -> &OnionExitPolicy {
        &self.policy
    }

    /// Return whether this offer includes `service`.
    pub fn offers(&self, service: &OnionServiceName) -> bool {
        self.services.contains(service)
    }

    /// Return the first offered service this node's runtime cannot interpret, if any
    /// (`runtime_interprets`).
    pub fn uninterpretable_service(&self) -> Option<&OnionServiceName> {
        self.services
            .iter()
            .find(|service| !runtime_interprets(service))
    }
}

/// Return whether this node's runtime interprets the world-facing `service`: a native runtime
/// interprets every one, a browser runtime, which has `fetch` but no sockets, only `https`.
pub(crate) fn runtime_interprets(service: &OnionServiceName) -> bool {
    cfg!(not(rings_browser)) || *service == OnionServiceName::https()
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
                concat!(
                    "advertise_onion_exit requires advertise_onion_relay because registering any ",
                    "onion symbol registers relay (#834 D2)",
                )
                .to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnionExitOffer;
    use super::OnionInvokeOffer;
    use super::OnionRole;
    use crate::error::Error;
    use crate::error::Result;
    use crate::onion::invoke_descriptor::OnionInvokePolicy;
    use crate::onion::signature::OnionBackendName;
    use crate::onion::signature::OnionInputType;
    use crate::onion::signature::OnionInvokeShape;
    use crate::onion::signature::OnionLatencyClass;
    use crate::onion::signature::OnionWidthClass;
    use crate::onion::OnionExitPolicy;
    use crate::onion::OnionServiceName;

    /// An open single-target policy.
    fn open_policy() -> Result<OnionExitPolicy> {
        OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], Vec::new())
    }

    fn sample_invoke_offer() -> Result<OnionInvokeOffer> {
        OnionInvokeOffer::new([(
            OnionInvokeShape::new(
                OnionInputType::Fixed(OnionWidthClass::B32),
                OnionWidthClass::B64,
                OnionLatencyClass::Ms500,
            ),
            OnionBackendName::parse("jev.typesafe")?,
            OnionInvokePolicy::default(),
        )])
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

    /// `Invoke` and `Both` rungs enforce `registers_relay() == true` and reject empty offers (D2).
    #[test]
    fn test_invoke_offer_requires_entries_and_implies_relay() -> Result<()> {
        assert!(OnionInvokeOffer::new([]).is_err());
        let invoke = sample_invoke_offer()?;
        let role_invoke = OnionRole::<OnionExitOffer>::Invoke(invoke.clone());
        assert!(role_invoke.registers_relay());
        assert!(role_invoke.exit().is_none());
        assert_eq!(role_invoke.invoke(), Some(&invoke));

        let exit = OnionExitOffer::new([OnionServiceName::https()], open_policy()?)?;
        let both = OnionRole::Exit(exit.clone()).with_invoke(invoke.clone());
        assert!(both.registers_relay());
        assert_eq!(both.exit(), Some(&exit));
        assert_eq!(both.invoke(), Some(&invoke));
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

    /// `try_map` is the traversal of `map`: it preserves the rung and fails only on the exit.
    #[test]
    fn test_try_map_traverses_the_exit_rung() {
        let halve = |value: u8| value.is_multiple_of(2).then_some(value / 2).ok_or(value);

        assert_eq!(
            OnionRole::<u8, u8>::Exit(8).try_map(halve),
            Ok(OnionRole::Exit(4))
        );
        assert_eq!(OnionRole::<u8, u8>::Exit(7).try_map(halve), Err(7));
        assert_eq!(
            OnionRole::<u8, u8>::Invoke(3).try_map(halve),
            Ok(OnionRole::Invoke(3))
        );
        assert_eq!(
            OnionRole::<u8, u8>::Relay.try_map(halve),
            Ok(OnionRole::Relay)
        );
        assert_eq!(
            OnionRole::<u8, u8>::Client.try_map(halve),
            Ok(OnionRole::Client)
        );
    }

    /// `map` preserves the rung, and every rung above `Client` registers `relay`.
    #[test]
    fn test_map_preserves_the_rung() {
        let roles = [
            OnionRole::<u8, u8>::Client,
            OnionRole::Relay,
            OnionRole::Invoke(5),
            OnionRole::Exit(7),
            OnionRole::Both { exit: 7, invoke: 5 },
        ];

        for role in roles {
            let mapped = role.map(u16::from);
            assert_eq!(mapped.registers_relay(), role.registers_relay());
            assert_eq!(mapped.exit().copied(), role.exit().copied().map(u16::from));
            assert_eq!(mapped.invoke(), role.invoke());
        }
        assert!(!OnionRole::<u8, u8>::Client.registers_relay());
        assert!(OnionRole::<u8, u8>::Relay.registers_relay());
    }
}
