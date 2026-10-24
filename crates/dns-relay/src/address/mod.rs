//! The address map: each owner's own IPv6 address in the relay's prefix, and its inverse.
//!
//! A protocol that carries no host name before its first byte reaches the server (SMTP, SSH)
//! can only be routed by where it was sent. The relay therefore gives every owner its own
//! address in a prefix routed to the relay's host, and routes a connection by its destination.
//!
//! ```text
//!   P = (p, n)                     the prefix: network address p, length n
//!   k                              the relay's secret address key
//!   addr(o) = p ∨ (H_k(label(o)) ∧ hostmask(n))       H_k = the first 128 bits of HMAC-SHA256_k
//! ```
//!
//! The key keeps anyone from grinding an owner onto another owner's address: without `k`,
//! `addr` is a pseudorandom function of the label. `addr` is not injective (it maps labels onto
//! `128 − n` bits), so the inverse is kept explicitly in an [`AddressBook`], for the owners the
//! relay has answered for:
//!
//! ```text
//!   B : Address ⇀ (Owner, answered_at)
//!   bind(B, a, o, t) = B[a ↦ (o, t)]        if a ∉ dom B  ∨  owner(B a) = o
//!                    = Collision(owner(B a)) otherwise
//!   Invariant (injective):  B a = (o, _)  ⇒  addr(o) = a, and no other owner is bound to a
//! ```
//!
//! A collision keeps the first owner's binding: the second owner gets no address from this
//! relay. With a /64 prefix a collision needs `2^32` owners before it is likely, and a chosen one
//! needs the key.

use std::collections::BTreeMap;
use std::net::Ipv6Addr;

use hmac::Hmac;
use hmac::Mac;
use ipnet::Ipv6Net;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::owner::Owner;

/// Length of the address key: one SHA-256 block, so HMAC uses the key as is.
pub const ADDRESS_KEY_LEN: usize = 64;

/// The relay's secret address key `k`.
///
/// It is neither `Clone` nor `Debug`: one value per relay, zeroized on drop, never printed.
pub struct AddressKey(Zeroizing<[u8; ADDRESS_KEY_LEN]>);

impl AddressKey {
    /// The key with the bytes `bytes`.
    pub fn from_bytes(bytes: [u8; ADDRESS_KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The key's bytes, for persisting it.
    pub fn as_bytes(&self) -> &[u8; ADDRESS_KEY_LEN] {
        &self.0
    }
}

/// The address map `addr` of one relay: its prefix and its key.
pub struct AddressPlan {
    prefix: Ipv6Net,
    key: AddressKey,
}

impl AddressPlan {
    /// The map onto `prefix` under `key`. The prefix is taken as its network: host bits given in
    /// `prefix` are ignored.
    pub fn new(prefix: Ipv6Net, key: AddressKey) -> Self {
        Self {
            prefix: prefix.trunc(),
            key,
        }
    }

    /// The prefix the owners' addresses lie in.
    pub fn prefix(&self) -> Ipv6Net {
        self.prefix
    }

    /// `owner`'s address: `addr(owner)` in the module doc.
    ///
    /// Post: the address lies in [`Self::prefix`].
    pub fn address_of<O>(&self, owner: &O) -> Ipv6Addr
    where O: Owner {
        let mut mac = <Hmac<Sha256> as Mac>::new(self.key.0.as_slice().into());
        mac.update(owner.label().as_bytes());
        let digest = mac.finalize().into_bytes();
        let mut head = [0u8; 16];
        head.iter_mut()
            .zip(digest.iter())
            .for_each(|(out, byte)| *out = *byte);
        let host = u128::from_be_bytes(head) & u128::from(self.prefix.hostmask());
        Ipv6Addr::from(u128::from(self.prefix.network()) | host)
    }
}

/// An address is bound to another owner already, which keeps it.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("address {address} is bound to another owner")]
pub struct AddressCollision {
    /// The contested address.
    pub address: Ipv6Addr,
}

/// What a successful [`AddressBook::bind`] changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bound {
    /// The address was free and is now bound: the book grew.
    Fresh,
    /// The address was bound to the same owner already; only its answer time moved.
    Renewed,
}

/// One binding of the book: the owner an address belongs to, and when the relay last answered
/// with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Binding<O> {
    /// The owner the address belongs to.
    pub owner: O,
    /// When the relay last answered with this address, in milliseconds since the Unix epoch.
    pub answered_at_ms: u64,
}

/// The inverse of the address map over the owners the relay answered for (`B` in the module
/// doc). Pure: the caller supplies the time and persists the book.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddressBook<O> {
    bindings: BTreeMap<Ipv6Addr, Binding<O>>,
}

impl<O> Default for AddressBook<O> {
    fn default() -> Self {
        Self {
            bindings: BTreeMap::new(),
        }
    }
}

impl<O> AddressBook<O>
where O: Owner
{
    /// The book holding `bindings`, refusing a second owner at an address as [`Self::bind`]
    /// does: of two bindings at one address, the one listed first is kept.
    pub fn from_bindings(bindings: impl IntoIterator<Item = (Ipv6Addr, Binding<O>)>) -> Self {
        let mut book = Self::default();
        for (address, binding) in bindings {
            // A refused binding is a collision in a corrupted store; the first one stands.
            let _ = book.bind(address, binding.owner, binding.answered_at_ms);
        }
        book
    }

    /// Bind `address` to `owner`, answered at `now_ms` (`bind` in the module doc).
    ///
    /// Pre: `address = addr(owner)` under the relay's plan.
    /// Post: on `Ok`, `owner_at(address) = Some(owner)`; on `Err`, the book is unchanged.
    pub fn bind(
        &mut self,
        address: Ipv6Addr,
        owner: O,
        now_ms: u64,
    ) -> Result<Bound, AddressCollision> {
        match self.bindings.get_mut(&address) {
            Some(binding) if binding.owner == owner => {
                binding.answered_at_ms = binding.answered_at_ms.max(now_ms);
                Ok(Bound::Renewed)
            }
            Some(_) => Err(AddressCollision { address }),
            None => {
                self.bindings.insert(address, Binding {
                    owner,
                    answered_at_ms: now_ms,
                });
                Ok(Bound::Fresh)
            }
        }
    }

    /// The owner `address` is bound to, if any.
    pub fn owner_at(&self, address: &Ipv6Addr) -> Option<O> {
        self.bindings.get(address).map(|binding| binding.owner)
    }

    /// Forget every binding last answered before `horizon_ms`; return whether any was.
    ///
    /// A client resolves a name again once its answer's TTL passes, which renews the binding,
    /// so a binding unanswered for much longer than the TTL is no longer used by any client.
    pub fn forget_answered_before(&mut self, horizon_ms: u64) -> bool {
        let before = self.bindings.len();
        self.bindings
            .retain(|_address, binding| binding.answered_at_ms >= horizon_ms);
        self.bindings.len() != before
    }

    /// The bindings, by address.
    pub fn bindings(&self) -> impl Iterator<Item = (&Ipv6Addr, &Binding<O>)> {
        self.bindings.iter()
    }
}

#[cfg(test)]
mod test_address;
