//! Quota accounting `quota(n, f, ω)` per `(node, shape, backend)` for intermediate `invoke`
//! evaluations (#834 D2, Phase 2b item 2).
//!
//! ```text
//! admit((f, ω), policy, p, now):
//!   concurrent(f, ω) < min(policy.max_concurrent, 256) ∧ share(f, ω, p) < 32
//!   ∧ window_60s(f, ω, now) + 1 ≤ policy.max_invocations_per_minute   ⇒ lease
//! drop(lease):
//!   concurrent(f, ω) ← concurrent(f, ω) − 1;  share(f, ω, p) ← share(f, ω, p) − 1
//! ```
//!
//! Each `(shape, backend)` pair registered on this node keeps its own isolated concurrency and
//! per-minute rate counters, so heavy load on one backend never starves another backend, and no
//! single previous hop `p` can pin all concurrent slots of `(shape, backend)`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use rings_core::dht::Did;

use super::invoke_descriptor::OnionInvokePolicy;
use super::signature::OnionBackendName;
use super::signature::OnionInvokeShape;
use crate::error::Error;
use crate::error::Result;
use crate::peer_quota::PeerQuota;
use crate::sync_lock::lock;

/// Per-minute rate window in milliseconds.
const INVOKE_LIMIT_WINDOW_MS: u128 = 60_000;
/// Hard ceiling on concurrent in-flight invocations of one `(shape, backend)` on this node.
const HARD_MAX_CONCURRENT_INVOCATIONS: u32 = 256;
/// Hard ceiling on concurrent in-flight invocations of one `(shape, backend)` from one previous hop.
const HARD_MAX_INVOCATIONS_PER_PREVIOUS_HOP: u32 = 32;

/// Key of one per-`(shape, backend)` quota bucket on this node.
type SymbolKey = (OnionInvokeShape, OnionBackendName);

/// Shared quota gate for all `(shape, backend)` registrations on one node (#834 D2).
#[derive(Clone, Default)]
pub(crate) struct OnionInvokeAccounting {
    buckets: Arc<Mutex<BTreeMap<SymbolKey, Arc<Mutex<InvokeBucket>>>>>,
}

struct InvokeBucket {
    concurrency: PeerQuota,
    window_start_ms: u128,
    invocations_this_window: u64,
}

impl Default for InvokeBucket {
    fn default() -> Self {
        Self {
            concurrency: PeerQuota::new(
                HARD_MAX_CONCURRENT_INVOCATIONS as usize,
                HARD_MAX_INVOCATIONS_PER_PREVIOUS_HOP as usize,
            ),
            window_start_ms: 0,
            invocations_this_window: 0,
        }
    }
}

impl InvokeBucket {
    fn refresh_window(&mut self, now_ms: u128) {
        if now_ms.saturating_sub(self.window_start_ms) >= INVOKE_LIMIT_WINDOW_MS {
            self.window_start_ms = now_ms;
            self.invocations_this_window = 0;
        }
    }
}

/// RAII lease held for the lifetime of one admitted `invoke` evaluation (including its `L(f)`
/// release wait); dropping it frees its concurrency slot.
pub(crate) struct OnionInvokeLease {
    bucket: Arc<Mutex<InvokeBucket>>,
    previous_hop: Did,
}

impl Drop for OnionInvokeLease {
    fn drop(&mut self) {
        if let Ok(mut bucket) = self.bucket.lock() {
            let released = bucket.concurrency.release(self.previous_hop);
            debug_assert!(released);
        }
    }
}

impl OnionInvokeAccounting {
    /// Admit one invocation of `(shape, backend)` arriving from `previous_hop` at `now_ms` under
    /// `policy`.
    ///
    /// # Errors
    ///
    /// [`Error::NoPermission`] when `(shape, backend)`'s concurrency bound, previous-hop share, or
    /// per-minute rate limit is exhausted.
    pub(crate) fn admit(
        &self,
        shape: OnionInvokeShape,
        backend: &OnionBackendName,
        policy: &OnionInvokePolicy,
        previous_hop: Did,
        now_ms: u128,
    ) -> Result<OnionInvokeLease> {
        let bucket = {
            let mut buckets = lock(&self.buckets)?;
            Arc::clone(
                buckets
                    .entry((shape, backend.clone()))
                    .or_insert_with(|| Arc::new(Mutex::new(InvokeBucket::default()))),
            )
        };
        let mut guard = lock(&bucket)?;
        let max_concurrent =
            effective_limit(policy.max_concurrent, HARD_MAX_CONCURRENT_INVOCATIONS);
        if guard.concurrency.total() >= max_concurrent as usize {
            return Err(Error::NoPermission);
        }
        guard.refresh_window(now_ms);
        let next_count = if policy.max_invocations_per_minute == 0 {
            guard.invocations_this_window.saturating_add(1)
        } else {
            guard
                .invocations_this_window
                .checked_add(1)
                .filter(|next| *next <= policy.max_invocations_per_minute)
                .ok_or(Error::NoPermission)?
        };
        guard
            .concurrency
            .reserve(previous_hop)
            .map_err(|_| Error::NoPermission)?;
        guard.invocations_this_window = next_count;
        drop(guard);
        Ok(OnionInvokeLease {
            bucket,
            previous_hop,
        })
    }
}

const fn effective_limit(requested: u32, hard_limit: u32) -> u32 {
    if requested == 0 || requested > hard_limit {
        hard_limit
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use rings_core::dht::Did;

    use super::OnionInvokeAccounting;
    use super::HARD_MAX_INVOCATIONS_PER_PREVIOUS_HOP;
    use super::INVOKE_LIMIT_WINDOW_MS;
    use crate::onion::invoke_descriptor::OnionInvokePolicy;
    use crate::onion::signature::OnionBackendName;
    use crate::onion::signature::OnionInputType;
    use crate::onion::signature::OnionInvokeShape;
    use crate::onion::signature::OnionLatencyClass;
    use crate::onion::signature::OnionWidthClass;

    fn shape_a() -> OnionInvokeShape {
        OnionInvokeShape::new(
            OnionInputType::Fixed(OnionWidthClass::B32),
            OnionWidthClass::B64,
            OnionLatencyClass::Ms10,
        )
    }

    /// Concurrency and per-minute rate limits are isolated per `(shape, backend)`, and dropping a
    /// lease releases its slot.
    #[test]
    fn test_quota_is_isolated_per_shape_and_backend() {
        let accounting = OnionInvokeAccounting::default();
        let add = OnionBackendName::parse("add").expect("add");
        let mul = OnionBackendName::parse("mul").expect("mul");
        let policy = OnionInvokePolicy {
            max_concurrent: 2,
            max_invocations_per_minute: 3,
        };
        let peer = Did::from(1_u32);

        let lease1 = accounting
            .admit(shape_a(), &add, &policy, peer, 1_000)
            .expect("1st add");
        let lease2 = accounting
            .admit(shape_a(), &add, &policy, peer, 1_000)
            .expect("2nd add");
        // Concurrency cap 2 reached on `add`, while `mul` on the same shape is unaffected.
        assert!(accounting
            .admit(shape_a(), &add, &policy, peer, 1_000)
            .is_err());
        let _mul_lease = accounting
            .admit(shape_a(), &mul, &policy, peer, 1_000)
            .expect("mul unaffected");

        // Releasing one `add` lease admits the 3rd invocation in the minute, then hits rate cap 3.
        drop(lease1);
        let _lease3 = accounting
            .admit(shape_a(), &add, &policy, peer, 1_500)
            .expect("3rd add");
        drop(lease2);
        assert!(accounting
            .admit(shape_a(), &add, &policy, peer, 2_000)
            .is_err());

        // Advancing past the 60s window refreshes the rate budget.
        assert!(accounting
            .admit(
                shape_a(),
                &add,
                &policy,
                peer,
                1_000 + INVOKE_LIMIT_WINDOW_MS
            )
            .is_ok());
    }

    /// One previous hop cannot pin more than `HARD_MAX_INVOCATIONS_PER_PREVIOUS_HOP` concurrent
    /// invocations of `(shape, backend)`.
    #[test]
    fn test_previous_hop_cannot_pin_global_concurrency_budget() {
        let accounting = OnionInvokeAccounting::default();
        let backend = OnionBackendName::parse("jev.typesafe").expect("backend");
        let policy = OnionInvokePolicy::default();
        let noisy_peer = Did::from(10_u32);
        let other_peer = Did::from(11_u32);

        let mut leases = (0..HARD_MAX_INVOCATIONS_PER_PREVIOUS_HOP)
            .map(|_| {
                accounting
                    .admit(shape_a(), &backend, &policy, noisy_peer, 5_000)
                    .expect("within per-hop share")
            })
            .collect::<Vec<_>>();
        assert!(accounting
            .admit(shape_a(), &backend, &policy, noisy_peer, 5_000)
            .is_err());
        assert!(accounting
            .admit(shape_a(), &backend, &policy, other_peer, 5_000)
            .is_ok());
        drop(leases.pop());
        assert!(accounting
            .admit(shape_a(), &backend, &policy, noisy_peer, 5_000)
            .is_ok());
    }
}
