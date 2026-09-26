//! The exit's accounting: how many sessions it serves, per previous hop and in total, and how
//! many world bytes per minute (#834 D2′).
//!
//! ```text
//! admit(policy, ς, p):  total < min(policy.max_sessions, 1024) ∧ share(p) < 64  ⇒ lease
//! drop(lease):          total ← total − 1;  share(p) ← share(p) − 1
//! record(policy, n):    window of 60 s;  bytes + n ≤ policy.max_bytes_per_minute ⇒ bytes += n
//! ```
//!
//! A session is admitted once, when its first loop arrives, and holds its lease until its
//! driver ends, so a session that never binds a target still counts against its previous hop's
//! share.

use std::sync::Arc;
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::utils::get_epoch_ms;

use super::OnionExitPolicy;
use crate::error::Error;
use crate::error::Result;
use crate::peer_quota::PeerQuota;
use crate::sync_lock::lock;

/// The byte policy's window.
const EXIT_LIMIT_WINDOW_MS: u128 = 60_000;
/// The most sessions an exit serves at once, whatever its policy asks.
const HARD_MAX_ACTIVE_SESSIONS: u32 = 1_024;
/// The most sessions whose opening loops arrived from one previous hop.
const HARD_MAX_SESSIONS_PER_PREVIOUS_HOP: u32 = 64;

/// Shared accounting gate for onion exits (see the module diagram).
///
/// Invariants:
/// - `session_quota.total()` is the number of live leases, at most
///   `min(policy.max_sessions, HARD_MAX_ACTIVE_SESSIONS)` at admission;
/// - `session_quota.peer_total(p)` is the number of live leases of previous hop `p`, at most
///   [`HARD_MAX_SESSIONS_PER_PREVIOUS_HOP`];
/// - `bytes_this_window ≤ policy.max_bytes_per_minute` whenever that field is non-zero.
#[derive(Clone, Default)]
pub(crate) struct OnionExitAccounting {
    /// The counters, shared by every lease.
    limiter: Arc<Mutex<ExitLimiter>>,
}

/// The counters of an exit's accounting.
struct ExitLimiter {
    /// Live sessions, in total and per previous hop.
    session_quota: PeerQuota,
    /// The start of the byte window.
    window_start_ms: u128,
    /// World bytes recorded in the window.
    bytes_this_window: u64,
}

impl Default for ExitLimiter {
    fn default() -> Self {
        Self {
            session_quota: PeerQuota::new(
                HARD_MAX_ACTIVE_SESSIONS as usize,
                HARD_MAX_SESSIONS_PER_PREVIOUS_HOP as usize,
            ),
            window_start_ms: 0,
            bytes_this_window: 0,
        }
    }
}

/// The lease of one admitted session; dropping it releases the session's slot.
pub(crate) struct OnionExitLease {
    /// The counters it releases into.
    limiter: Arc<Mutex<ExitLimiter>>,
    /// The previous hop the session is counted against.
    previous_hop: Did,
}

impl Drop for OnionExitLease {
    fn drop(&mut self) {
        if let Ok(mut limiter) = self.limiter.lock() {
            let released = limiter.session_quota.release(self.previous_hop);
            debug_assert!(released);
        }
    }
}

impl OnionExitAccounting {
    /// Admit one session whose first loop arrived from `previous_hop`, under `policy`.
    ///
    /// # Errors
    ///
    /// [`Error::NoPermission`] if the exit's session bound or the hop's share is full.
    pub(crate) fn admit(
        &self,
        policy: &OnionExitPolicy,
        previous_hop: Did,
    ) -> Result<OnionExitLease> {
        let mut limiter = lock(&self.limiter)?;
        let max_sessions = effective_limit(policy.max_sessions, HARD_MAX_ACTIVE_SESSIONS);
        if limiter.session_quota.total() >= max_sessions as usize {
            return Err(Error::NoPermission);
        }
        limiter
            .session_quota
            .reserve(previous_hop)
            .map_err(|_| Error::NoPermission)?;
        Ok(OnionExitLease {
            limiter: Arc::clone(&self.limiter),
            previous_hop,
        })
    }

    /// Record `bytes` world bytes under the per-minute policy window.
    ///
    /// # Errors
    ///
    /// [`Error::NoPermission`] once the window's budget is spent.
    pub(crate) fn record_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<()> {
        if policy.max_bytes_per_minute == 0 || bytes == 0 {
            return Ok(());
        }
        let mut limiter = lock(&self.limiter)?;
        limiter.refresh_byte_window(get_epoch_ms());
        let next = limiter
            .bytes_this_window
            .checked_add(bytes)
            .filter(|next| *next <= policy.max_bytes_per_minute)
            .ok_or(Error::NoPermission)?;
        limiter.bytes_this_window = next;
        Ok(())
    }

    /// The bytes still available in the current window, or `None` for an unlimited policy.
    ///
    /// # Errors
    ///
    /// A poisoned lock.
    pub(crate) fn remaining_bytes(&self, policy: &OnionExitPolicy) -> Result<Option<u64>> {
        if policy.max_bytes_per_minute == 0 {
            return Ok(None);
        }
        let mut limiter = lock(&self.limiter)?;
        limiter.refresh_byte_window(get_epoch_ms());
        Ok(Some(
            policy
                .max_bytes_per_minute
                .saturating_sub(limiter.bytes_this_window),
        ))
    }
}

impl ExitLimiter {
    /// Start a new byte window at `now` once the current one has passed.
    fn refresh_byte_window(&mut self, now_ms: u128) {
        if now_ms.saturating_sub(self.window_start_ms) >= EXIT_LIMIT_WINDOW_MS {
            self.window_start_ms = now_ms;
            self.bytes_this_window = 0;
        }
    }
}

/// The session bound: `0` means the descriptor chose no smaller one; it never lifts the hard
/// bound.
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

    use super::effective_limit;
    use super::OnionExitAccounting;
    use super::HARD_MAX_ACTIVE_SESSIONS;
    use super::HARD_MAX_SESSIONS_PER_PREVIOUS_HOP;
    use crate::onion::OnionExitPolicy;

    #[test]
    fn test_unspecified_or_excessive_policy_uses_hard_resource_limits() {
        assert_eq!(
            effective_limit(0, HARD_MAX_ACTIVE_SESSIONS),
            HARD_MAX_ACTIVE_SESSIONS
        );
        assert_eq!(effective_limit(7, HARD_MAX_ACTIVE_SESSIONS), 7);
        assert_eq!(
            effective_limit(u32::MAX, HARD_MAX_ACTIVE_SESSIONS),
            HARD_MAX_ACTIVE_SESSIONS
        );
    }

    /// The policy's session bound holds, and a dropped lease frees its slot.
    #[test]
    fn test_the_session_bound_holds_and_leases_release() {
        let accounting = OnionExitAccounting::default();
        let policy = OnionExitPolicy {
            max_sessions: 2,
            ..OnionExitPolicy::default()
        };
        let first = accounting.admit(&policy, Did::from(1_u32)).expect("first");
        let _second = accounting.admit(&policy, Did::from(2_u32)).expect("second");

        assert!(accounting.admit(&policy, Did::from(3_u32)).is_err());
        drop(first);
        assert!(accounting.admit(&policy, Did::from(3_u32)).is_ok());
    }

    /// An unspecified policy is still bounded by the hard session limit.
    #[test]
    fn test_unspecified_session_limit_is_still_bounded() {
        let accounting = OnionExitAccounting::default();
        let policy = OnionExitPolicy::default();
        let leases = (0..HARD_MAX_ACTIVE_SESSIONS)
            .map(|index| {
                accounting
                    .admit(&policy, Did::from(index.saturating_add(1)))
                    .expect("within the hard bound")
            })
            .collect::<Vec<_>>();

        assert!(accounting.admit(&policy, Did::from(u32::MAX)).is_err());
        assert_eq!(leases.len(), HARD_MAX_ACTIVE_SESSIONS as usize);
    }

    /// One previous hop cannot take more than its share of the exit's sessions.
    #[test]
    fn test_one_previous_hop_cannot_pin_the_global_session_budget() {
        let accounting = OnionExitAccounting::default();
        let policy = OnionExitPolicy::default();
        let peer = Did::from(8_u32);
        let mut leases = (0..HARD_MAX_SESSIONS_PER_PREVIOUS_HOP)
            .map(|_| accounting.admit(&policy, peer).expect("inside its share"))
            .collect::<Vec<_>>();

        assert!(accounting.admit(&policy, peer).is_err());
        assert!(accounting.admit(&policy, Did::from(9_u32)).is_ok());
        drop(leases.pop());
        assert!(accounting.admit(&policy, peer).is_ok());
    }

    /// The byte window: bytes are recorded up to the budget, and the next byte is refused.
    #[test]
    fn test_the_byte_window_refuses_past_its_budget() {
        let accounting = OnionExitAccounting::default();
        let policy = OnionExitPolicy {
            max_bytes_per_minute: 10,
            ..OnionExitPolicy::default()
        };

        assert!(accounting.record_bytes(&policy, 6).is_ok());
        assert_eq!(accounting.remaining_bytes(&policy).expect("lock"), Some(4));
        assert!(accounting.record_bytes(&policy, 5).is_err());
        assert!(accounting.record_bytes(&policy, 4).is_ok());
        assert_eq!(accounting.remaining_bytes(&policy).expect("lock"), Some(0));
        assert_eq!(
            accounting
                .remaining_bytes(&OnionExitPolicy::default())
                .expect("lock"),
            None
        );
    }
}
