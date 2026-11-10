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
use hmac::Hmac;
use hmac::KeyInit;
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
    /// The `Hmac` state (`k ⊕ ipad` and `k ⊕ opad` chaining values) is wiped on drop by `sha2`'s
    /// `zeroize`. HMAC's padded key block `k ⊕ opad` and SHA-256's message schedule are the
    /// transient stack copies `SECURITY.md` lists (#852, #1122).
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

/// A validated binding transition staged before persistence: its [`BookChange`]s are written and
/// flushed to the journal before [`AddressBook::commit`] mutates the in-memory book.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlannedBind<O> {
    address: Ipv6Addr,
    owner: O,
    answered_at_ms: u64,
    persisted_at_ms: u64,
    forgotten: Vec<Ipv6Addr>,
    changes: Vec<BookChange<O>>,
}

impl<O> PlannedBind<O> {
    /// The journal changes this transition requires before commit.
    pub(crate) fn changes(&self) -> &[BookChange<O>] {
        &self.changes
    }
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

    /// Stage binding `address` to `owner` at `now_ms` without mutating `self`.
    ///
    /// Pre: `address = addr(owner)` under the relay's plan.
    pub(crate) fn plan_bind(
        &self,
        address: Ipv6Addr,
        owner: O,
        now_ms: u64,
    ) -> Result<PlannedBind<O>, AddressCollision> {
        let refresh = self.refresh_ms();
        if let Some(binding) = self.bindings.get(&address) {
            if binding.owner != owner {
                return Err(AddressCollision { address });
            }
            let answered_at_ms = binding.answered_at_ms.max(now_ms);
            if answered_at_ms.saturating_sub(binding.persisted_at_ms) <= refresh {
                return Ok(PlannedBind {
                    address,
                    owner,
                    answered_at_ms,
                    persisted_at_ms: binding.persisted_at_ms,
                    forgotten: Vec::new(),
                    changes: Vec::new(),
                });
            }
            return Ok(PlannedBind {
                address,
                owner,
                answered_at_ms,
                persisted_at_ms: answered_at_ms,
                forgotten: Vec::new(),
                changes: vec![BookChange::Bind {
                    address,
                    owner,
                    answered_at_ms,
                }],
            });
        }
        let forgotten = self.victims(now_ms);
        let mut changes = Vec::with_capacity(forgotten.len().saturating_add(1));
        changes.extend(forgotten.iter().copied().map(BookChange::Unbind));
        changes.push(BookChange::Bind {
            address,
            owner,
            answered_at_ms: now_ms,
        });
        Ok(PlannedBind {
            address,
            owner,
            answered_at_ms: now_ms,
            persisted_at_ms: now_ms,
            forgotten,
            changes,
        })
    }

    /// Commit a staged [`PlannedBind`] to `self` and return the changes it carried.
    pub(crate) fn commit(&mut self, planned: PlannedBind<O>) -> Vec<BookChange<O>> {
        for address in &planned.forgotten {
            self.bindings.remove(address);
        }
        self.bindings.insert(planned.address, Binding {
            owner: planned.owner,
            answered_at_ms: planned.answered_at_ms,
            persisted_at_ms: planned.persisted_at_ms,
        });
        planned.changes
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
        let planned = self.plan_bind(address, owner, now_ms)?;
        Ok(self.commit(planned))
    }

    /// Addresses to evict to make room for one new binding at `now_ms`: bindings past their
    /// retention first, then, while the book is full, the ones answered longest ago.
    fn victims(&self, now_ms: u64) -> Vec<Ipv6Addr> {
        if self.bindings.len() < self.capacity {
            return Vec::new();
        }
        let horizon = now_ms.saturating_sub(self.retention_ms);
        let mut forgotten = Vec::new();
        let mut remaining = Vec::new();
        for (address, binding) in &self.bindings {
            if binding.answered_at_ms < horizon {
                forgotten.push(*address);
            } else {
                remaining.push((binding.answered_at_ms, *address));
            }
        }
        if remaining.len() >= self.capacity {
            // Stable sort preserves BTreeMap address order on equal `answered_at_ms`, matching
            // repeated `min_by_key` over `self.bindings`.
            remaining.sort_by_key(|(answered_at_ms, _address)| *answered_at_ms);
            let evict = if self.capacity == 0 {
                remaining.len()
            } else {
                remaining
                    .len()
                    .saturating_sub(self.capacity.saturating_sub(1))
            };
            forgotten.extend(
                remaining
                    .into_iter()
                    .take(evict)
                    .map(|(_answered_at_ms, address)| address),
            );
        }
        forgotten
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
