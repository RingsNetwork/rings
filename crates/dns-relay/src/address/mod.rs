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
//!   B : Address ⇀ (Owner, answered_at)        |B| ≤ capacity
//!   bind(B, a, o, t) = B[a ↦ (o, t)]        if a ∉ dom B  ∨  owner(B a) = o
//!                    = Collision             otherwise
//!   Invariant (injective):  B a = (o, _)  ⇒  addr(o) = a, and no other owner is bound to a
//! ```
//!
//! A collision keeps the first owner's binding: the second owner gets no address from this
//! relay. With a /64 prefix a collision needs `2^32` owners before it is likely, and a chosen one
//! needs the key. A full book first forgets the bindings unanswered for its retention, then the
//! one answered longest ago, so its size stays bounded whatever number of owners asks.
//!
//! The book is pure: the caller supplies the time and persists the [`BookChange`]s each
//! operation returns.

use std::collections::BTreeMap;
use std::net::Ipv6Addr;

use hmac::digest::Key;
use hmac::digest::KeyInit;
use hmac::Hmac;
use hmac::Mac;
use ipnet::Ipv6Net;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroize;
use zeroize::Zeroizing;

use crate::owner::Owner;

/// Length of the address key: one SHA-256 block, so HMAC uses the key as is. The conversion in
/// [`AddressKey::from_bytes`] compiles only while this equals the block length.
pub const ADDRESS_KEY_LEN: usize = 64;

/// The relay's secret address key `k`, typed as an HMAC-SHA256 key, so its length is a fact of
/// its type.
///
/// It is neither `Clone` nor `Debug`: one value per relay, zeroized on drop, never printed.
pub struct AddressKey(Key<Hmac<Sha256>>);

impl AddressKey {
    /// The key with the bytes `bytes`.
    pub fn from_bytes(bytes: Zeroizing<[u8; ADDRESS_KEY_LEN]>) -> Self {
        Self(Key::<Hmac<Sha256>>::from(*bytes))
    }

    /// The key's bytes, for persisting it.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl Drop for AddressKey {
    fn drop(&mut self) {
        self.0.as_mut_slice().zeroize();
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
        let mut mac = <Hmac<Sha256> as KeyInit>::new(&self.key.0);
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

/// A change of the book that its persisted form must follow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BookChange<O> {
    /// `address` is bound to `owner`, last answered at `answered_at_ms`.
    Bind {
        /// The address.
        address: Ipv6Addr,
        /// Its owner.
        owner: O,
        /// When the relay last answered with it, in milliseconds since the Unix epoch.
        answered_at_ms: u64,
    },
    /// `address` is bound to no one any more.
    Unbind(Ipv6Addr),
}

/// One binding of the book: the owner an address belongs to, when the relay last answered with
/// it, and the answer time its persisted form holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Binding<O> {
    /// The owner the address belongs to.
    owner: O,
    /// When the relay last answered with this address, in milliseconds since the Unix epoch.
    answered_at_ms: u64,
    /// The answer time the persisted book holds for this binding.
    persisted_at_ms: u64,
}

/// The inverse of the address map over the owners the relay answered for (`B` in the module
/// doc), with its retention and capacity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddressBook<O> {
    bindings: BTreeMap<Ipv6Addr, Binding<O>>,
    retention_ms: u64,
    capacity: usize,
}

impl<O> AddressBook<O>
where O: Owner
{
    /// An empty book keeping a binding for `retention_ms` after its last answer, and at most
    /// `capacity` bindings.
    pub fn new(retention_ms: u64, capacity: usize) -> Self {
        Self {
            bindings: BTreeMap::new(),
            retention_ms,
            capacity,
        }
    }

    /// The book `bindings` persisted, as `(address, owner, answered_at_ms)`: of two bindings at
    /// one address the first stands, and beyond the capacity the least recently answered are
    /// dropped, as a full book forgets them.
    pub fn restore(
        retention_ms: u64,
        capacity: usize,
        bindings: impl IntoIterator<Item = (Ipv6Addr, O, u64)>,
    ) -> Self {
        let mut book = Self::new(retention_ms, capacity);
        for (address, owner, answered_at_ms) in bindings {
            book.bindings.entry(address).or_insert(Binding {
                owner,
                answered_at_ms,
                persisted_at_ms: answered_at_ms,
            });
        }
        let excess = book.bindings.len().saturating_sub(capacity);
        let mut by_age = book
            .bindings
            .iter()
            .map(|(address, binding)| (binding.answered_at_ms, *address))
            .collect::<Vec<_>>();
        by_age.sort_unstable();
        for (_answered_at_ms, address) in by_age.into_iter().take(excess) {
            book.bindings.remove(&address);
        }
        book
    }

    /// How far an answer time may move before its binding is persisted again: an eighth of the
    /// retention, so a restarted relay keeps a binding at least seven eighths of it.
    const fn refresh_ms(&self) -> u64 {
        self.retention_ms / 8
    }

    /// Bind `address` to `owner`, answered at `now_ms` (`bind` in the module doc); return the
    /// changes to persist.
    ///
    /// Pre: `address = addr(owner)` under the relay's plan.
    /// Post: on `Ok`, `owner_at(address) = Some(owner)` and the book holds at most its capacity;
    /// on `Err`, the book is unchanged.
    pub fn bind(
        &mut self,
        address: Ipv6Addr,
        owner: O,
        now_ms: u64,
    ) -> Result<Vec<BookChange<O>>, AddressCollision> {
        let refresh = self.refresh_ms();
        if let Some(binding) = self.bindings.get_mut(&address) {
            if binding.owner != owner {
                return Err(AddressCollision { address });
            }
            binding.answered_at_ms = binding.answered_at_ms.max(now_ms);
            if binding
                .answered_at_ms
                .saturating_sub(binding.persisted_at_ms)
                <= refresh
            {
                return Ok(Vec::new());
            }
            binding.persisted_at_ms = binding.answered_at_ms;
            return Ok(vec![BookChange::Bind {
                address,
                owner,
                answered_at_ms: binding.answered_at_ms,
            }]);
        }
        let mut changes = self.make_room(now_ms);
        self.bindings.insert(address, Binding {
            owner,
            answered_at_ms: now_ms,
            persisted_at_ms: now_ms,
        });
        changes.push(BookChange::Bind {
            address,
            owner,
            answered_at_ms: now_ms,
        });
        Ok(changes)
    }

    /// Make room for one more binding at `now_ms`: forget the bindings past their retention,
    /// then, while the book is full, the one answered longest ago.
    ///
    /// Post: `|B| < capacity`, unless the capacity is 0.
    fn make_room(&mut self, now_ms: u64) -> Vec<BookChange<O>> {
        if self.bindings.len() < self.capacity {
            return Vec::new();
        }
        let horizon = now_ms.saturating_sub(self.retention_ms);
        let mut forgotten = self
            .bindings
            .iter()
            .filter(|(_address, binding)| binding.answered_at_ms < horizon)
            .map(|(address, _binding)| *address)
            .collect::<Vec<_>>();
        forgotten.iter().for_each(|address| {
            self.bindings.remove(address);
        });
        while !self.bindings.is_empty() && self.bindings.len() >= self.capacity {
            let oldest = self
                .bindings
                .iter()
                .min_by_key(|(_address, binding)| binding.answered_at_ms)
                .map(|(address, _binding)| *address);
            let Some(oldest) = oldest else {
                break;
            };
            self.bindings.remove(&oldest);
            forgotten.push(oldest);
        }
        forgotten.into_iter().map(BookChange::Unbind).collect()
    }

    /// The owner `address` is bound to, if any.
    pub fn owner_at(&self, address: &Ipv6Addr) -> Option<O> {
        self.bindings.get(address).map(|binding| binding.owner)
    }

    /// The bindings, as `(address, owner, answered_at_ms)`.
    pub fn bindings(&self) -> impl Iterator<Item = (Ipv6Addr, O, u64)> + '_ {
        self.bindings
            .iter()
            .map(|(address, binding)| (*address, binding.owner, binding.answered_at_ms))
    }

    /// How many bindings the book holds.
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether the book holds no binding.
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

#[cfg(test)]
mod test_address;
