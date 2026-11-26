//! Bounded, time-indexed peer-IP pinhole table for `onion::udp` exit sessions.

use std::collections::BTreeMap;
use std::net::IpAddr;

/// Default maximum concurrent peer-IP pinholes retained by one `onion::udp` session.
pub const MAX_PINHOLES_PER_SESSION: usize = 256;

/// Maximum lifetime in milliseconds granted to a single peer-IP pinhole (`5 min`).
pub const MAX_PINHOLE_TTL_MS: u64 = 300_000;

/// Pure state machine tracking permitted inbound peer IP addresses and their expiry instants for
/// one `onion::udp` exit socket.
///
/// Law (Inbound gate): an inbound UDP datagram from `peer_ip` at `now_ms` is admitted if and only
/// if `allows(peer_ip, now_ms)` holds, i.e. `allow_ip(peer_ip, ttl_ms, t)` succeeded at some
/// `t <= now_ms` with `t + min(ttl_ms, MAX_PINHOLE_TTL_MS) > now_ms`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OnionUdpPinholeTable {
    /// Maximum number of active peer-IP pinhole entries retained simultaneously.
    max_entries: usize,
    /// Map from permitted peer IP address to its expiration timestamp in milliseconds.
    entries: BTreeMap<IpAddr, u128>,
}

impl Default for OnionUdpPinholeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl OnionUdpPinholeTable {
    /// Construct an empty pinhole table bounded by [`MAX_PINHOLES_PER_SESSION`].
    pub const fn new() -> Self {
        Self::with_capacity_limit(MAX_PINHOLES_PER_SESSION)
    }

    /// Construct an empty pinhole table with an explicit entry cap (used by model checkers and
    /// unit tests to exercise capacity boundaries with small state spaces).
    pub const fn with_capacity_limit(max_entries: usize) -> Self {
        Self {
            max_entries,
            entries: BTreeMap::new(),
        }
    }

    /// Open or refresh a pinhole for `peer_ip` for `min(ttl_ms, MAX_PINHOLE_TTL_MS)` milliseconds
    /// beyond `now_ms`.
    ///
    /// Expired pinholes at `now_ms` are pruned first. Returns `false` without mutating active
    /// entries when `ttl_ms == 0`, `max_entries == 0`, or the table is already at capacity with
    /// distinct live peer IPs.
    pub fn allow_ip(&mut self, peer_ip: IpAddr, ttl_ms: u64, now_ms: u128) -> bool {
        self.prune_expired(now_ms);
        let clamped_ttl_ms = u128::from(ttl_ms.min(MAX_PINHOLE_TTL_MS));
        if clamped_ttl_ms == 0 || self.max_entries == 0 {
            return false;
        }
        let expires_at_ms = now_ms.saturating_add(clamped_ttl_ms);
        if let Some(existing) = self.entries.get_mut(&peer_ip) {
            *existing = expires_at_ms;
            return true;
        }
        if self.entries.len() >= self.max_entries {
            return false;
        }
        self.entries.insert(peer_ip, expires_at_ms);
        true
    }

    /// Return whether `peer_ip` currently holds an unexpired pinhole at `now_ms`.
    pub fn allows(&self, peer_ip: IpAddr, now_ms: u128) -> bool {
        self.entries
            .get(&peer_ip)
            .is_some_and(|&expires_at_ms| expires_at_ms > now_ms)
    }

    /// Remove all pinholes whose expiry instant is at or before `now_ms`.
    pub fn prune_expired(&mut self, now_ms: u128) {
        self.entries
            .retain(|_, expires_at_ms| *expires_at_ms > now_ms);
    }

    /// Return the number of stored pinhole entries (before lazy expiry).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the pinhole table stores zero entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
