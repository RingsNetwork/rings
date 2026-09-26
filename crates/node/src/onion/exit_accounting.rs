use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use rings_core::dht::Did;
use rings_core::utils::get_epoch_ms;

use super::session::OnionSessionId;
use super::OnionExitPolicy;
use crate::error::Error;
use crate::error::Result;
use crate::peer_quota::PeerQuota;
use crate::sync_lock::lock;

const EXIT_LIMIT_WINDOW_MS: u128 = 60_000;
const HARD_MAX_ACTIVE_SESSIONS: u32 = 1_024;
const HARD_MAX_SESSIONS_PER_PREVIOUS_HOP: u32 = 64;
const HARD_MAX_STREAMS_PER_SESSION: u32 = 64;

/// Shared accounting gate for onion exits: every session `ς` an exit opens, keyed with the
/// previous hop its opening loop arrived from.
///
/// Invariant: `session_quota.total() = |{ k | active_streams_by_session[k] > 0 }|`.
/// Invariant: for every `peer`, `session_quota.peer_total(peer)` equals the number of live
/// session keys whose `previous_hop = peer`, and never exceeds
/// [`HARD_MAX_SESSIONS_PER_PREVIOUS_HOP`].
/// Invariant: `bytes_this_window ≤ policy.max_bytes_per_minute` whenever that policy field is
/// non-zero.
/// Preservation: `admit` intersects advertised limits with hard implementation ceilings, then
/// checks active counters and byte budget under one lock before committing any stream or
/// session increment; dropping the returned lease decrements the same session key;
/// `record_bytes` resets stale windows before adding.
/// Post: `remaining_bytes` returns the exact bytes that may still be recorded in the current window,
/// or `None` when the byte policy is unlimited.
#[derive(Clone, Default)]
pub(crate) struct OnionExitAccounting {
    limiter: Arc<Mutex<ExitLimiter>>,
}

struct ExitLimiter {
    session_quota: PeerQuota,
    active_streams_by_session: HashMap<ExitSessionKey, u32>,
    window_start_ms: u128,
    bytes_this_window: u64,
}

impl Default for ExitLimiter {
    fn default() -> Self {
        Self {
            session_quota: PeerQuota::new(
                HARD_MAX_ACTIVE_SESSIONS as usize,
                HARD_MAX_SESSIONS_PER_PREVIOUS_HOP as usize,
            ),
            active_streams_by_session: HashMap::new(),
            window_start_ms: 0,
            bytes_this_window: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExitSessionKey {
    session: OnionSessionId,
    previous_hop: Did,
}

/// Pure successor selected by exit admission before any limiter state is mutated.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AdmitCommit {
    session_key: ExitSessionKey,
    reserve_circuit: bool,
    active_streams: u32,
    bytes_this_window: Option<u64>,
}

impl ExitSessionKey {
    const fn new(session: OnionSessionId, previous_hop: Did) -> Self {
        Self {
            session,
            previous_hop,
        }
    }
}

/// Lease for one admitted exit stream/request.
pub(crate) struct OnionExitLease {
    limiter: Arc<Mutex<ExitLimiter>>,
    session_key: ExitSessionKey,
}

impl Drop for OnionExitLease {
    fn drop(&mut self) {
        if let Ok(mut limiter) = self.limiter.lock() {
            if let Some(active_streams) =
                limiter.active_streams_by_session.get_mut(&self.session_key)
            {
                if *active_streams > 1 {
                    *active_streams -= 1;
                } else {
                    let released = limiter.session_quota.release(self.session_key.previous_hop);
                    debug_assert!(released);
                    if released {
                        limiter.active_streams_by_session.remove(&self.session_key);
                    }
                }
            }
        }
    }
}

impl OnionExitAccounting {
    /// Admit one exit stream or request under `policy`.
    pub(crate) fn admit(
        &self,
        policy: &OnionExitPolicy,
        session: OnionSessionId,
        previous_hop: Did,
        bytes: u64,
    ) -> Result<OnionExitLease> {
        let session_key = ExitSessionKey::new(session, previous_hop);
        let mut limiter = lock(&self.limiter)?;
        limiter.refresh_byte_window(get_epoch_ms());
        let commit = limiter.decide_admission(policy, session_key.clone(), bytes)?;
        limiter.apply_admission(commit)?;
        Ok(OnionExitLease {
            limiter: self.limiter.clone(),
            session_key,
        })
    }

    /// Record exit payload bytes under the per-minute policy window.
    pub(crate) fn record_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<()> {
        if policy.max_bytes_per_minute == 0 || bytes == 0 {
            return Ok(());
        }
        let mut limiter = lock(&self.limiter)?;
        limiter.refresh_byte_window(get_epoch_ms());
        if let Some(next) = limiter.next_recorded_bytes(policy, bytes)? {
            limiter.bytes_this_window = next;
        }
        Ok(())
    }

    /// Return bytes still available in the current per-minute window.
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
    /// Decide admission from an immutable snapshot.
    ///
    /// Post: `Err` leaves the snapshot unchanged; `Ok(commit)` contains every field needed by
    /// [`Self::apply_admission`] and cannot partially update the coupled resource counters.
    fn decide_admission(
        &self,
        policy: &OnionExitPolicy,
        session_key: ExitSessionKey,
        bytes: u64,
    ) -> Result<AdmitCommit> {
        let active_streams = self
            .active_streams_by_session
            .get(&session_key)
            .copied()
            .unwrap_or_default();
        let max_streams =
            effective_limit(policy.max_streams_per_circuit, HARD_MAX_STREAMS_PER_SESSION);
        if active_streams >= max_streams {
            return Err(Error::NoPermission);
        }
        let max_circuits = effective_limit(policy.max_circuits, HARD_MAX_ACTIVE_SESSIONS);
        if active_streams == 0 && self.session_quota.total() >= max_circuits as usize {
            return Err(Error::NoPermission);
        }
        if active_streams == 0 {
            self.session_quota
                .can_reserve(session_key.previous_hop)
                .map_err(|_| Error::NoPermission)?;
        }
        let active_streams = active_streams.checked_add(1).ok_or(Error::NoPermission)?;
        Ok(AdmitCommit {
            session_key,
            reserve_circuit: active_streams == 1,
            active_streams,
            bytes_this_window: self.next_recorded_bytes(policy, bytes)?,
        })
    }

    fn apply_admission(&mut self, commit: AdmitCommit) -> Result<()> {
        if commit.reserve_circuit {
            self.session_quota
                .reserve(commit.session_key.previous_hop)
                .map_err(|_| Error::NoPermission)?;
        }
        self.active_streams_by_session
            .insert(commit.session_key, commit.active_streams);
        if let Some(bytes_this_window) = commit.bytes_this_window {
            self.bytes_this_window = bytes_this_window;
        }
        Ok(())
    }

    fn refresh_byte_window(&mut self, now_ms: u128) {
        if now_ms.saturating_sub(self.window_start_ms) >= EXIT_LIMIT_WINDOW_MS {
            self.window_start_ms = now_ms;
            self.bytes_this_window = 0;
        }
    }

    fn next_recorded_bytes(&self, policy: &OnionExitPolicy, bytes: u64) -> Result<Option<u64>> {
        if policy.max_bytes_per_minute == 0 || bytes == 0 {
            return Ok(None);
        }
        let next = self
            .bytes_this_window
            .checked_add(bytes)
            .ok_or(Error::NoPermission)?;
        if next > policy.max_bytes_per_minute {
            return Err(Error::NoPermission);
        }
        Ok(Some(next))
    }
}

/// `0` means the descriptor did not choose a smaller limit; it never disables the hard bound.
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
    use super::ExitLimiter;
    use super::ExitSessionKey;
    use super::OnionExitAccounting;
    use super::HARD_MAX_ACTIVE_SESSIONS;
    use super::HARD_MAX_SESSIONS_PER_PREVIOUS_HOP;
    use super::HARD_MAX_STREAMS_PER_SESSION;
    use crate::onion::session::OnionSessionId;
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

    #[test]
    fn test_admission_decision_does_not_mutate_limiter_before_commit() {
        let mut limiter = ExitLimiter::default();
        let session_key = ExitSessionKey::new(OnionSessionId::new([1; 16]), Did::from(9_u32));
        let policy = OnionExitPolicy {
            max_circuits: 1,
            max_streams_per_circuit: 2,
            max_bytes_per_minute: 10,
            ..OnionExitPolicy::default()
        };

        let commit = limiter
            .decide_admission(&policy, session_key.clone(), 7)
            .expect("pure admission decision");
        assert_eq!(limiter.session_quota.total(), 0);
        assert!(limiter.active_streams_by_session.is_empty());
        assert_eq!(limiter.bytes_this_window, 0);

        limiter
            .apply_admission(commit)
            .expect("validated commit applies atomically");
        assert_eq!(limiter.session_quota.total(), 1);
        assert_eq!(
            limiter.session_quota.peer_total(session_key.previous_hop),
            1
        );
        assert_eq!(
            limiter.active_streams_by_session.get(&session_key),
            Some(&1)
        );
        assert_eq!(limiter.bytes_this_window, 7);
        assert!(limiter.decide_admission(&policy, session_key, 4).is_err());
    }

    #[test]
    fn test_unspecified_stream_limit_is_still_bounded() {
        let accounting = OnionExitAccounting::default();
        let policy = OnionExitPolicy::default();
        let session_key = OnionSessionId::random();
        let peer = Did::from(7_u32);
        let mut leases = Vec::new();
        for _ in 0..HARD_MAX_STREAMS_PER_SESSION {
            let admitted = accounting.admit(&policy, session_key, peer, 0);
            assert!(admitted.is_ok());
            if let Ok(lease) = admitted {
                leases.push(lease);
            }
        }
        assert!(accounting.admit(&policy, session_key, peer, 0).is_err());
        assert_eq!(leases.len(), HARD_MAX_STREAMS_PER_SESSION as usize);
    }

    #[test]
    fn test_unspecified_session_limit_is_still_bounded() {
        let accounting = OnionExitAccounting::default();
        let policy = OnionExitPolicy::default();
        let mut leases = Vec::new();
        for index in 0..HARD_MAX_ACTIVE_SESSIONS {
            let session_key = OnionSessionId::new(u128::from(index).to_be_bytes());
            let peer = Did::from(index.saturating_add(1));
            let admitted = accounting.admit(&policy, session_key, peer, 0);
            assert!(admitted.is_ok());
            if let Ok(lease) = admitted {
                leases.push(lease);
            }
        }
        let overflow = OnionSessionId::new(u128::from(HARD_MAX_ACTIVE_SESSIONS).to_be_bytes());
        assert!(accounting
            .admit(&policy, overflow, Did::from(u32::MAX), 0)
            .is_err());
        assert_eq!(leases.len(), HARD_MAX_ACTIVE_SESSIONS as usize);
    }

    #[test]
    fn test_one_previous_hop_cannot_pin_the_global_exit_circuit_budget() {
        let accounting = OnionExitAccounting::default();
        let policy = OnionExitPolicy::default();
        let peer = Did::from(8_u32);
        let mut leases = Vec::new();

        for index in 0..HARD_MAX_SESSIONS_PER_PREVIOUS_HOP {
            let session_key = OnionSessionId::new(u128::from(index).to_be_bytes());
            leases.push(
                accounting
                    .admit(&policy, session_key, peer, 0)
                    .expect("peer session inside its share"),
            );
        }
        let overflow =
            OnionSessionId::new(u128::from(HARD_MAX_SESSIONS_PER_PREVIOUS_HOP).to_be_bytes());
        assert!(accounting.admit(&policy, overflow, peer, 0).is_err());

        let other = Did::from(9_u32);
        let other_lease = accounting
            .admit(&policy, overflow, other, 0)
            .expect("another peer retains an exit share");
        drop(other_lease);
        drop(leases.pop());
        assert!(accounting.admit(&policy, overflow, peer, 0).is_ok());
    }
}
