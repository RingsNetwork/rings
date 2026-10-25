//! The memo of verified delegator authorizations (#1089).
//!
//! Every message carries two delegations (the origin's and the hop's), and verifying one
//! recovers the delegator's ECDSA key over the delegation's packed fields. A delegation is
//! long-lived and signed once, so the #843 loop spent 8.4% of its CPU recovering the same few
//! (#1087). The memo remembers which exact delegations verified.
//!
//! ```text
//! key(s)      = keccak256(encode(s))                 the whole delegation, signature included
//! good(s)     = the delegator's signature over pack(s) verifies     time-independent
//! until(s)    = expires_at(s) − Δ                    Δ = LEASE_MARGIN_MS, one delivery bound
//!
//! Law (Memo)    key(s) ∈ memo with u  ⟹  good(s) ∧ u = until(s)
//! Law (Expiry)  a hit at `at` requires at < until(s), and `is_expired_at` runs before any
//!               lookup, so a hit never accepts a delegation its own check would refuse, and
//!               never outlives the delegation's expiry minus its margin
//! Law (Bound)   |memo| ≤ VERIFIED_DELEGATIONS_CAPACITY; eviction drops a cached proof only,
//!               so it costs a recovery and never changes a verdict
//! ```
//!
//! The key is the full 32-byte digest of the canonical encoding, not the 20-byte reference
//! address of [`DelegationDigest`](super::DelegationDigest): a key collision would let a
//! delegation skip its signature check, so the memo keeps the hash's full collision resistance.
//! There is no revocation path for a delegation; it ends only at its expiry, which the memo
//! honours. The per-transaction signature by the delegatee key is not memoized: it differs for
//! every message and is always checked.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use super::Delegation;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::session_link::LEASE_MARGIN_MS;

/// The verified delegations kept at once: one per current neighbour edge of the load-bounds law
/// (`R = 2(F + s + 1) = 328`, #1088) and one for the local principal. A relayed origin's
/// delegation beyond the bound only costs a recovery when it is evicted.
pub(crate) const VERIFIED_DELEGATIONS_CAPACITY: usize = 328 + 1;

/// The memo key of a delegation: Keccak-256 of its canonical encoding.
type MemoKey = [u8; 32];

/// The verified delegations, with the instant each stops being served, in insertion order for
/// eviction.
struct VerifiedDelegations {
    /// `key(s) ↦ until(s)`.
    until: BTreeMap<MemoKey, u128>,
    /// The keys, oldest first.
    order: VecDeque<MemoKey>,
}

/// The process-wide memo: a cache of a pure predicate, so sharing it changes no verdict.
static VERIFIED: Mutex<VerifiedDelegations> = Mutex::new(VerifiedDelegations {
    until: BTreeMap::new(),
    order: VecDeque::new(),
});

/// The memo; every critical section leaves it consistent, so a poisoned lock is still valid.
fn memo() -> MutexGuard<'static, VerifiedDelegations> {
    VERIFIED.lock().unwrap_or_else(PoisonError::into_inner)
}

impl VerifiedDelegations {
    /// Whether `key` is memoized as verified and still served at `at_ms`.
    fn serves(&self, key: &MemoKey, at_ms: u128) -> bool {
        self.until.get(key).is_some_and(|until| at_ms < *until)
    }

    /// Memoize `key` as verified until `until`, dropping the oldest entries past the bound.
    fn record(&mut self, key: MemoKey, until: u128) {
        if self.until.insert(key, until).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > VERIFIED_DELEGATIONS_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.until.remove(&oldest);
            }
        }
    }
}

impl Delegation {
    /// The memo key of this delegation.
    fn memo_key(&self) -> Result<MemoKey> {
        let encoded = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        Ok(keccak256(encoded.as_slice()))
    }

    /// The instant from which the memo no longer serves this delegation: its expiry minus `Δ`.
    fn memo_until(&self) -> u128 {
        self.expires_at_ms().saturating_sub(LEASE_MARGIN_MS)
    }

    /// Whether the memo serves this delegation as verified at `at_ms` (`Law (Expiry)`). The key
    /// is returned for [`Self::memoize_verified`], so the delegation is encoded once.
    pub(super) fn memoized_at(&self, at_ms: u128) -> Result<(bool, MemoKey)> {
        let key = self.memo_key()?;
        Ok((memo().serves(&key, at_ms), key))
    }

    /// Record that the delegation of `key`, this one, verified (`Law (Memo)`).
    pub(super) fn memoize_verified(&self, key: MemoKey) {
        memo().record(key, self.memo_until());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::VerifiedDelegations;
    use super::LEASE_MARGIN_MS;
    use super::VERIFIED_DELEGATIONS_CAPACITY;
    use crate::delegation::DelegateeKey;
    use crate::ecc::SecretKey;
    use crate::error::Error;
    use crate::error::Result;

    /// A fresh delegation, so the shared memo holds nothing of it yet.
    fn fresh() -> Result<crate::delegation::Delegation> {
        Ok(DelegateeKey::new_with_seckey(&SecretKey::random())?.delegation())
    }

    /// `Law (Memo)`: a verified delegation is memoized under its exact encoding, so a delegation
    /// that differs in its signature alone is a different key and is still refused.
    #[test]
    fn test_a_tampered_delegation_is_refused_after_the_genuine_one_is_memoized() -> Result<()> {
        let genuine = fresh()?;
        let now_ms = genuine.created_at_ms();
        genuine.verify_delegator_authorization_at(now_ms)?;
        assert!(genuine.memoized_at(now_ms)?.0);

        let mut tampered = genuine.clone();
        if let Some(byte) = tampered.delegator_signature.first_mut() {
            *byte ^= 1;
        }
        assert!(!tampered.memoized_at(now_ms)?.0);
        assert!(matches!(
            tampered.verify_delegator_authorization_at(now_ms),
            Err(Error::VerifySignatureFailed)
        ));
        Ok(())
    }

    /// `Law (Expiry)`: the memo serves a delegation only before its expiry minus `Δ`; within the
    /// margin it is verified again, and once expired it is refused, memoized or not.
    #[test]
    fn test_the_memo_never_outlives_expiry_minus_its_margin() -> Result<()> {
        let delegation = fresh()?;
        let expiry = delegation.expires_at_ms();
        let until = expiry - LEASE_MARGIN_MS;
        delegation.verify_delegator_authorization_at(delegation.created_at_ms())?;

        assert!(delegation.memoized_at(until - 1)?.0);
        assert!(!delegation.memoized_at(until)?.0);
        delegation.verify_delegator_authorization_at(until)?;
        delegation.verify_delegator_authorization_at(expiry)?;
        assert!(matches!(
            delegation.verify_delegator_authorization_at(expiry + 1),
            Err(Error::DelegationExpired)
        ));
        Ok(())
    }

    /// `Law (Bound)`: past the capacity the oldest entries go, and the memo never grows past it.
    #[test]
    fn test_the_memo_keeps_at_most_its_capacity_dropping_the_oldest() {
        let mut memo = VerifiedDelegations {
            until: Default::default(),
            order: Default::default(),
        };
        for index in 0..=VERIFIED_DELEGATIONS_CAPACITY {
            let mut key = [0; 32];
            key[..8].copy_from_slice(&(index as u64).to_le_bytes());
            memo.record(key, u128::MAX);
        }
        assert_eq!(memo.until.len(), VERIFIED_DELEGATIONS_CAPACITY);
        assert_eq!(memo.order.len(), VERIFIED_DELEGATIONS_CAPACITY);
        assert!(!memo.serves(&[0; 32], 0));
    }

    /// The cost of one delegator authorization check, recovered (a fresh delegation) and served
    /// from the memo (the same one again). Run with `--release -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn bench_delegator_authorization_memo() -> Result<()> {
        const RUNS: u32 = 2000;
        let delegations = (0..RUNS).map(|_| fresh()).collect::<Result<Vec<_>>>()?;
        let at_ms = delegations[0].created_at_ms();
        let started = Instant::now();
        for delegation in &delegations {
            delegation.verify_delegator_authorization_at(at_ms)?;
        }
        let recovered = started.elapsed() / RUNS;
        let started = Instant::now();
        for _ in 0..RUNS {
            delegations[0].verify_delegator_authorization_at(at_ms)?;
        }
        let memoized = started.elapsed() / RUNS;
        println!("delegator authorization: recovered {recovered:?}, memoized {memoized:?}");
        Ok(())
    }
}
