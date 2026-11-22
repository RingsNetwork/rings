//! Pure deterministic state machine for the TURN entry front-end bridge (#1292).
//!
//! [`TurnEntryBridge`] owns the pure protocol invariants coupling RFC 5766 TURN entry allocations
//! to an asynchronous [`crate::TurnDatagramRelayAllocator`] (such as `OnionUdpRelayBackend` in
//! `rings-turn-service`):
//!
//! 1. **Ticket single-binding & idempotent retransmission (Law 1)**: each
//!    [`crate::SignedTurnSessionTicket`] reservation is claimed by at most one 5-tuple, and
//!    retransmitting `Allocate` with the same `(five_tuple, username, transaction_id)` while the
//!    backend session open is in flight (or after completion) coalesces onto that attempt and
//!    replays the exact final response, never `401 Unauthorized`.
//! 2. **Relayed-address projection (Law 2)**: every installed allocation records the concrete
//!    relayed [`SocketAddr`] (`E_exit`) returned by the backend session open.
//! 3. **RFC 5766 permission & channel gate (Law 3)**: peer IP permissions (`300s`) and channel
//!    bindings (`600s` default) are checked and committed atomically against `now_ms`, and inbound
//!    or outbound datagrams pass only while the allocation and its peer-IP permission are live.
//! 4. **Bounded lease & zero orphan after shutdown (Law 4)**: `Refresh(0)`, TCP disconnect,
//!    allocation expiry, and `TurnServer::close()` move allocations into explicit closing state
//!    until [`TurnEntryBridge::finish_teardown`] confirms relay closure, and no in-flight open can
//!    install an allocation once shutdown begins.
//! 5. **Capacity atomicity (Law 6)**: `unclaimed_tickets.len() <= max_reservations` and
//!    `live_allocations.len() + pending_allocations.len() + closing_allocations.len() <= max_allocations`
//!    hold at every transition without underflow or permit leak.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::net::SocketAddr;

/// Default RFC 5766 peer-IP permission lifetime in milliseconds (`300,000 ms` = 5 minutes).
pub const BRIDGE_PERMISSION_TTL_MS: u128 = 300_000;

/// Default retention window in milliseconds for cached `Allocate` error responses (`60,000 ms`),
/// ensuring late UDP retransmissions of a failed single-use `SessionTicket` `Allocate` receive the
/// same error response rather than `401 Unauthorized`.
pub const BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS: u128 = 60_000;

/// Failure reasons when reserving a [`crate::SignedTurnSessionTicket`] slot in [`TurnEntryBridge`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BridgeReserveError {
    /// Requested or server-configured reservation TTL is zero.
    InvalidTtl,
    /// Unclaimed reservation table has reached `max_reservations`.
    CapacityReached {
        /// Configured cap on concurrent unclaimed reservations.
        max_reservations: usize,
    },
}

/// Failure reasons when binding a TURN channel in [`TurnEntryBridge`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BridgeChannelBindError {
    /// Allocation is missing, expired, or shutting down.
    AllocationGone,
    /// Channel number or peer socket address is already bound to a different counterpart.
    Conflict,
    /// Adding `peer.ip()` would exceed `max_permissions_per_allocation`.
    PermissionQuotaExceeded,
}

/// Result of pre-checking whether a batch of peer IP permissions can be installed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BridgePermissionCheck {
    /// Allocation is live and has room for all requested peer IPs.
    Permitted,
    /// Allocation is missing, expired, or shutting down.
    AllocationGone,
    /// Installing the requested peer IPs would exceed `max_permissions_per_allocation`.
    QuotaExceeded,
}

/// Outcome of [`TurnEntryBridge::begin_allocate`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BridgeAllocateStart<Resp> {
    /// Server is shutting down; drop request without mutating state.
    ShuttingDown,
    /// Replay an already-computed final `Allocate` response for this `(key, username, tx_id)`.
    ReplayCached(Resp),
    /// Coalesce onto an already in-flight backend open for this `(key, username, tx_id)`.
    CoalescedInFlight {
        /// Identifier of the in-flight open attempt.
        open_id: u64,
    },
    /// Another allocation (or in-flight open with a different `username` or `tx_id`) owns `key`.
    AllocationMismatch,
    /// Single-use `SessionTicket` is unknown, expired, or was already claimed.
    TicketUnknownOrConsumed,
    /// `live + pending + closing` has reached `max_allocations`.
    CapacityReached {
        /// Whether any single-use ticket carried by this request was consumed (so the caller should
        /// cache the `508` response for idempotent retransmission of `(key, username, tx_id)`).
        cache_for_retransmit: bool,
    },
    /// Capacity slot reserved and pending entry registered; caller must drive `allocate_relay` and
    /// resolve via [`TurnEntryBridge::complete_allocate_ok`] or
    /// [`TurnEntryBridge::complete_allocate_err`].
    StartBackendOpen {
        /// Unique identifier assigned to this open attempt (becomes `alloc_id` on install).
        open_id: u64,
    },
}

/// Outcome of [`TurnEntryBridge::check_existing_allocate`] before `Allocate` attribute validation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BridgeExistingAllocate<Resp> {
    /// No active, pending, or cached entry matches `key`; proceed with normal `Allocate` admission.
    None,
    /// Server is shutting down; drop request without mutating state.
    ShuttingDown,
    /// Replay an already-computed final `Allocate` response for `(key, username, tx_id)`.
    ReplayCached(Resp),
    /// Coalesce onto an already in-flight backend open for `(key, username, tx_id)`.
    CoalescedInFlight {
        /// Identifier of the in-flight open attempt.
        open_id: u64,
    },
    /// Another allocation (or in-flight open with a different `username` or `tx_id`) owns `key`.
    AllocationMismatch,
}

/// Outcome of [`TurnEntryBridge::complete_allocate_ok`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BridgeCompleteAllocate {
    /// Allocation installed in `live_allocations` under `alloc_id`.
    Installed {
        /// Installed allocation identifier.
        alloc_id: u64,
    },
    /// Open was cancelled (e.g. TCP disconnect) or server began shutting down while `allocate_relay`
    /// was in flight; caller must immediately close the newly allocated relay and then invoke
    /// [`TurnEntryBridge::finish_teardown`].
    AbortAndCloseRelay {
        /// Closing identifier tracked in `closing_allocations` until `finish_teardown`.
        closing_id: u64,
    },
}

/// Outcome of [`TurnEntryBridge::refresh`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BridgeRefreshOutcome {
    /// Allocation was not found (already deleted or replaced).
    NotFound,
    /// Allocation had already expired at `now_ms`; moved to `closing_allocations`.
    Expired {
        /// Allocation ID moved to `closing_allocations` for relay teardown.
        close_alloc_id: u64,
    },
    /// `Refresh(0)` deleted the allocation; moved to `closing_allocations`.
    Deleted {
        /// Allocation ID moved to `closing_allocations` for relay teardown.
        close_alloc_id: u64,
    },
    /// Allocation lifetime extended to `expires_at_ms`.
    Refreshed {
        /// Updated expiry timestamp in milliseconds.
        expires_at_ms: u128,
    },
}

/// Outcome of [`TurnEntryBridge::expire_if_due`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BridgeExpireOutcome {
    /// Allocation was already removed by another path.
    AlreadyRemoved,
    /// Allocation was refreshed and remains live until `expires_at_ms`.
    StillLive {
        /// Current expiry timestamp in milliseconds.
        expires_at_ms: u128,
    },
    /// Allocation expired at `now_ms` and moved to `closing_allocations`.
    Expired {
        /// Allocation ID moved to `closing_allocations` for relay teardown.
        close_alloc_id: u64,
    },
}

/// Outcome of [`TurnEntryBridge::disconnect_tuple`].
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct BridgeDisconnectOutcome {
    /// In-flight `open_id` cancelled by the disconnect (moved to `closing_allocations` until the
    /// in-flight task observes cancellation and calls `finish_teardown`).
    pub cancelled_open_id: Option<u64>,
    /// Live `alloc_id` removed by the disconnect (moved to `closing_allocations` until relay
    /// teardown finishes).
    pub closed_alloc_id: Option<u64>,
}

/// In-flight `Allocate` entry registered atomically before `allocate_relay` yields.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BridgePendingAllocate<Tx> {
    /// Unique identifier assigned to this open attempt.
    pub open_id: u64,
    /// Authenticated TURN username owning this attempt.
    pub username: String,
    /// STUN `TransactionId` of the initial `Allocate` request.
    pub tx_id: Tx,
    /// Clamped allocation lifetime in milliseconds to apply upon install.
    pub lifetime_ms: u128,
    /// Count of duplicate/retransmitted `Allocate`s coalesced onto this attempt while in flight.
    pub coalesced_count: u32,
}

/// Installed live TURN allocation state tracked inside [`TurnEntryBridge`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BridgeLiveAllocation {
    /// Unique allocation identifier.
    pub alloc_id: u64,
    /// Authenticated TURN username owning this allocation.
    pub username: String,
    /// Public relayed socket address (`E_exit`) advertised in `XOR-RELAYED-ADDRESS`.
    pub relay_addr: SocketAddr,
    /// Allocation expiry timestamp in milliseconds.
    pub expires_at_ms: u128,
    /// Active RFC 5766 peer-IP permissions (`peer_ip -> expires_at_ms`).
    pub permissions: BTreeMap<IpAddr, u128>,
    /// Active RFC 5766 channel bindings (`channel_number -> (peer_addr, expires_at_ms)`).
    pub channels: BTreeMap<u16, (SocketAddr, u128)>,
}

/// Cached final `Allocate` response for idempotent retransmission handling.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BridgeCachedAllocate<Tx, Resp> {
    /// Authenticated TURN username that produced this response.
    pub username: String,
    /// STUN `TransactionId` of the `Allocate` request.
    pub tx_id: Tx,
    /// Serialized (or abstract) final response.
    pub response: Resp,
    /// Timestamp after which this cached entry may be evicted.
    pub expires_at_ms: u128,
}

/// Pure deterministic state machine of the TURN entry front-end bridge (#1292).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TurnEntryBridge<K: Ord, Tx, Resp> {
    max_allocations: usize,
    max_reservations: usize,
    max_reservation_ttl_ms: u128,
    next_id: u64,
    shutting_down: bool,
    unclaimed_tickets: BTreeMap<[u8; 16], u128>,
    pending_allocations: BTreeMap<K, BridgePendingAllocate<Tx>>,
    live_allocations: BTreeMap<K, BridgeLiveAllocation>,
    closing_allocations: BTreeSet<u64>,
    cached_allocate: BTreeMap<K, BridgeCachedAllocate<Tx, Resp>>,
}

impl<K, Tx, Resp> TurnEntryBridge<K, Tx, Resp>
where
    K: Copy + Eq + Ord,
    Tx: Copy + Eq,
    Resp: Clone,
{
    /// Create a new [`TurnEntryBridge`] with capacity limits `max_allocations` (`>= 1`),
    /// `max_reservations`, and `max_reservation_ttl_ms`.
    pub const fn new(
        max_allocations: usize,
        max_reservations: usize,
        max_reservation_ttl_ms: u128,
    ) -> Self {
        let cap = if max_allocations == 0 {
            1
        } else {
            max_allocations
        };
        Self {
            max_allocations: cap,
            max_reservations,
            max_reservation_ttl_ms,
            next_id: 1,
            shutting_down: false,
            unclaimed_tickets: BTreeMap::new(),
            pending_allocations: BTreeMap::new(),
            live_allocations: BTreeMap::new(),
            closing_allocations: BTreeSet::new(),
            cached_allocate: BTreeMap::new(),
        }
    }

    /// Configured maximum concurrent allocations (`live + pending + closing`).
    pub const fn max_allocations(&self) -> usize {
        self.max_allocations
    }

    /// Configured maximum concurrent unclaimed `SessionTicket` reservations.
    pub const fn max_reservations(&self) -> usize {
        self.max_reservations
    }

    /// Whether [`Self::begin_shutdown`] has been invoked.
    pub const fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Current count of unclaimed ticket reservations (without pruning).
    pub fn unclaimed_ticket_len(&self) -> usize {
        self.unclaimed_tickets.len()
    }

    /// Current count of in-flight `Allocate` attempts.
    pub fn pending_allocation_len(&self) -> usize {
        self.pending_allocations.len()
    }

    /// Current count of installed live allocations (without pruning).
    pub fn live_allocation_len(&self) -> usize {
        self.live_allocations.len()
    }

    /// Current count of allocations or cancelled opens whose relay teardown is still in progress.
    pub fn closing_allocation_len(&self) -> usize {
        self.closing_allocations.len()
    }

    /// Total allocation capacity slots currently held (`live + pending + closing`).
    pub fn used_allocation_slots(&self) -> usize {
        self.live_allocations
            .len()
            .saturating_add(self.pending_allocations.len())
            .saturating_add(self.closing_allocations.len())
    }

    /// Remaining free allocation capacity slots (`max_allocations - used_allocation_slots()`).
    pub fn available_allocation_slots(&self) -> usize {
        self.max_allocations
            .saturating_sub(self.used_allocation_slots())
    }

    /// Borrow the map of live allocations.
    pub const fn live_allocations(&self) -> &BTreeMap<K, BridgeLiveAllocation> {
        &self.live_allocations
    }

    /// Borrow the map of pending allocations.
    pub const fn pending_allocations(&self) -> &BTreeMap<K, BridgePendingAllocate<Tx>> {
        &self.pending_allocations
    }

    /// Borrow the set of closing allocation IDs.
    pub const fn closing_allocations(&self) -> &BTreeSet<u64> {
        &self.closing_allocations
    }

    /// Prune expired unclaimed tickets (`now_ms > expires_at_ms`) and return the remaining count.
    pub fn prune_expired_tickets(&mut self, now_ms: u128) -> usize {
        self.unclaimed_tickets.retain(|_, &mut exp| now_ms <= exp);
        self.unclaimed_tickets.len()
    }

    /// Prune expired live allocations (`now_ms >= expires_at_ms`), moving their IDs into
    /// `closing_allocations` and returning the newly expired `alloc_id`s for relay teardown.
    pub fn sweep_expired_allocations(&mut self, now_ms: u128) -> Vec<u64> {
        let expired_keys: Vec<K> = self
            .live_allocations
            .iter()
            .filter(|(_, alloc)| now_ms >= alloc.expires_at_ms)
            .map(|(&k, _)| k)
            .collect();
        let mut expired_ids = Vec::with_capacity(expired_keys.len());
        for key in expired_keys {
            if let Some(alloc) = self.live_allocations.remove(&key) {
                self.cached_allocate.remove(&key);
                self.closing_allocations.insert(alloc.alloc_id);
                expired_ids.push(alloc.alloc_id);
            }
        }
        expired_ids
    }

    /// Reserve an unclaimed `SessionTicket` slot, clamping `requested_ttl_ms` to
    /// `max_reservation_ttl_ms` and returning the clamped `expires_at_ms`.
    pub fn reserve_ticket(
        &mut self,
        session_id: [u8; 16],
        requested_ttl_ms: u128,
        now_ms: u128,
    ) -> std::result::Result<u128, BridgeReserveError> {
        if requested_ttl_ms == 0 || self.max_reservation_ttl_ms == 0 || self.shutting_down {
            return Err(BridgeReserveError::InvalidTtl);
        }
        self.prune_expired_tickets(now_ms);
        if !self.unclaimed_tickets.contains_key(&session_id)
            && self.unclaimed_tickets.len() >= self.max_reservations
        {
            return Err(BridgeReserveError::CapacityReached {
                max_reservations: self.max_reservations,
            });
        }
        let clamped_ttl = requested_ttl_ms.min(self.max_reservation_ttl_ms);
        let expires_at_ms = now_ms.saturating_add(clamped_ttl);
        self.unclaimed_tickets.insert(session_id, expires_at_ms);
        Ok(expires_at_ms)
    }

    /// Return the expected owner username for an incoming `Allocate` on `(key, tx_id)` when it
    /// matches an in-flight open, an active allocation, or a live cached `Allocate` response.
    ///
    /// Allows `DelegationAuthHandler` to verify `MESSAGE-INTEGRITY` on retransmissions even if a
    /// single-use `SessionTicket`'s short unclaimed TTL elapsed after the first `Allocate` claimed
    /// it.
    pub fn expected_owner_for_allocate(&self, key: &K, tx_id: &Tx, now_ms: u128) -> Option<&str> {
        if let Some(pending) = self.pending_allocations.get(key) {
            if &pending.tx_id == tx_id {
                return Some(pending.username.as_str());
            }
        }
        if let Some(live) = self.live_allocations.get(key) {
            if now_ms < live.expires_at_ms {
                return Some(live.username.as_str());
            }
        }
        if let Some(cached) = self.cached_allocate.get(key) {
            if now_ms <= cached.expires_at_ms && &cached.tx_id == tx_id {
                return Some(cached.username.as_str());
            }
        }
        None
    }

    /// Return the expected owner username for an established allocation on `key` at `now_ms`.
    pub fn expected_owner_for_established(&self, key: &K, now_ms: u128) -> Option<&str> {
        self.live_allocations
            .get(key)
            .filter(|alloc| now_ms < alloc.expires_at_ms)
            .map(|alloc| alloc.username.as_str())
    }

    /// Perform a single-key (`O(log N)`) active-allocation lookup for `key` at `now_ms`, checking
    /// expiration only for `key` rather than sweeping the entire allocation table.
    ///
    /// Returns `(expired_alloc_id_to_close, live_alloc_id)`.
    pub fn active_allocation_id(&mut self, key: &K, now_ms: u128) -> (Option<u64>, Option<u64>) {
        if self.shutting_down {
            return (None, None);
        }
        let Some(alloc) = self.live_allocations.get(key) else {
            return (None, None);
        };
        let alloc_id = alloc.alloc_id;
        if now_ms >= alloc.expires_at_ms {
            self.live_allocations.remove(key);
            self.cached_allocate.remove(key);
            self.closing_allocations.insert(alloc_id);
            (Some(alloc_id), None)
        } else {
            (None, Some(alloc_id))
        }
    }

    /// Check whether `key` already holds a live allocation, an in-flight `PendingAllocate`, or a
    /// cached `Allocate` response BEFORE `Allocate` attribute validation or ticket consumption.
    ///
    /// Checks expiration only for `key` (returning any newly expired `alloc_id` so the caller can
    /// tear down its relay handle) and never starts a new backend open.
    pub fn check_existing_allocate(
        &mut self,
        key: &K,
        username: &str,
        tx_id: &Tx,
        now_ms: u128,
    ) -> (Option<u64>, BridgeExistingAllocate<Resp>) {
        let mut expired_id = None;
        if self
            .live_allocations
            .get(key)
            .is_some_and(|alloc| now_ms >= alloc.expires_at_ms)
        {
            if let Some(alloc) = self.live_allocations.remove(key) {
                self.cached_allocate.remove(key);
                self.closing_allocations.insert(alloc.alloc_id);
                expired_id = Some(alloc.alloc_id);
            }
        }

        if self.shutting_down {
            return (expired_id, BridgeExistingAllocate::ShuttingDown);
        }

        if let Some(live) = self.live_allocations.get(key) {
            if live.username == username {
                if let Some(cached) = self.cached_allocate.get(key) {
                    if &cached.tx_id == tx_id {
                        return (
                            expired_id,
                            BridgeExistingAllocate::ReplayCached(cached.response.clone()),
                        );
                    }
                }
            }
            return (expired_id, BridgeExistingAllocate::AllocationMismatch);
        }

        if let Some(pending) = self.pending_allocations.get_mut(key) {
            if pending.username == username && &pending.tx_id == tx_id {
                pending.coalesced_count = pending.coalesced_count.saturating_add(1);
                return (expired_id, BridgeExistingAllocate::CoalescedInFlight {
                    open_id: pending.open_id,
                });
            }
            return (expired_id, BridgeExistingAllocate::AllocationMismatch);
        }

        if let Some(cached) = self.cached_allocate.get(key) {
            if now_ms <= cached.expires_at_ms
                && cached.username == username
                && &cached.tx_id == tx_id
            {
                return (
                    expired_id,
                    BridgeExistingAllocate::ReplayCached(cached.response.clone()),
                );
            }
            if now_ms > cached.expires_at_ms {
                self.cached_allocate.remove(key);
            }
        }

        (expired_id, BridgeExistingAllocate::None)
    }

    /// Atomically admit, coalesce, replay, or reject an authenticated `Allocate` request before
    /// yielding to the relay allocator.
    ///
    /// Returns `(expired_alloc_ids_to_close, decision)`.
    pub fn begin_allocate(
        &mut self,
        key: K,
        username: String,
        tx_id: Tx,
        session_ticket_id: Option<[u8; 16]>,
        lifetime_ms: u128,
        now_ms: u128,
    ) -> (Vec<u64>, BridgeAllocateStart<Resp>) {
        self.prune_expired_tickets(now_ms);
        let expired_ids = self.sweep_expired_allocations(now_ms);

        if self.shutting_down {
            return (expired_ids, BridgeAllocateStart::ShuttingDown);
        }

        if let Some(live) = self.live_allocations.get(&key) {
            if live.username == username {
                if let Some(cached) = self.cached_allocate.get(&key) {
                    if cached.tx_id == tx_id {
                        return (
                            expired_ids,
                            BridgeAllocateStart::ReplayCached(cached.response.clone()),
                        );
                    }
                }
            }
            return (expired_ids, BridgeAllocateStart::AllocationMismatch);
        }

        if let Some(pending) = self.pending_allocations.get_mut(&key) {
            if pending.username == username && pending.tx_id == tx_id {
                pending.coalesced_count = pending.coalesced_count.saturating_add(1);
                return (expired_ids, BridgeAllocateStart::CoalescedInFlight {
                    open_id: pending.open_id,
                });
            }
            return (expired_ids, BridgeAllocateStart::AllocationMismatch);
        }

        if let Some(cached) = self.cached_allocate.get(&key) {
            if now_ms <= cached.expires_at_ms
                && cached.username == username
                && cached.tx_id == tx_id
            {
                return (
                    expired_ids,
                    BridgeAllocateStart::ReplayCached(cached.response.clone()),
                );
            }
            self.cached_allocate.remove(&key);
        }

        if let Some(ticket_id) = session_ticket_id {
            if !self.unclaimed_tickets.contains_key(&ticket_id) {
                return (expired_ids, BridgeAllocateStart::TicketUnknownOrConsumed);
            }
        }

        if self.used_allocation_slots() >= self.max_allocations {
            let consumed = if let Some(ticket_id) = session_ticket_id {
                self.unclaimed_tickets.remove(&ticket_id).is_some()
            } else {
                false
            };
            return (expired_ids, BridgeAllocateStart::CapacityReached {
                cache_for_retransmit: consumed,
            });
        }

        if let Some(ticket_id) = session_ticket_id {
            self.unclaimed_tickets.remove(&ticket_id);
        }

        let open_id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.pending_allocations.insert(key, BridgePendingAllocate {
            open_id,
            username,
            tx_id,
            lifetime_ms,
            coalesced_count: 0,
        });
        (expired_ids, BridgeAllocateStart::StartBackendOpen {
            open_id,
        })
    }

    /// Cache a final `Allocate` response (both `200 OK` and terminal error responses such as `508`
    /// after consuming a single-use ticket) in a bounded table so UDP retransmissions replay the
    /// exact final response instead of returning `401 Unauthorized`.
    pub fn cache_allocate_response(
        &mut self,
        key: K,
        username: String,
        tx_id: Tx,
        response: Resp,
        expires_at_ms: u128,
        now_ms: u128,
    ) {
        if self.shutting_down {
            return;
        }
        let max_cache_entries = self
            .max_allocations
            .saturating_add(self.max_reservations)
            .saturating_mul(4)
            .max(64);
        self.cached_allocate
            .retain(|_, entry| now_ms <= entry.expires_at_ms);
        while !self.cached_allocate.contains_key(&key)
            && self.cached_allocate.len() >= max_cache_entries
        {
            let Some(oldest_key) = self
                .cached_allocate
                .iter()
                .min_by_key(|(k, entry)| {
                    (self.live_allocations.contains_key(k), entry.expires_at_ms)
                })
                .map(|(&k, _)| k)
            else {
                break;
            };
            self.cached_allocate.remove(&oldest_key);
        }
        self.cached_allocate.insert(key, BridgeCachedAllocate {
            username,
            tx_id,
            response,
            expires_at_ms,
        });
    }

    /// Complete an in-flight `Allocate` after `allocate_relay` succeeded with `relay_addr`
    /// (`E_exit`).
    ///
    /// If the server began shutting down or the tuple disconnected while `allocate_relay` was in
    /// flight, refuses installation and keeps `open_id` in `closing_allocations` until the caller
    /// closes the newly opened relay and invokes [`Self::finish_teardown`].
    pub fn complete_allocate_ok(
        &mut self,
        key: K,
        open_id: u64,
        relay_addr: SocketAddr,
        response: Resp,
        now_ms: u128,
    ) -> BridgeCompleteAllocate {
        let matching = self
            .pending_allocations
            .get(&key)
            .is_some_and(|p| p.open_id == open_id);
        if !matching {
            self.closing_allocations.insert(open_id);
            return BridgeCompleteAllocate::AbortAndCloseRelay {
                closing_id: open_id,
            };
        }
        let Some(pending) = self.pending_allocations.remove(&key) else {
            self.closing_allocations.insert(open_id);
            return BridgeCompleteAllocate::AbortAndCloseRelay {
                closing_id: open_id,
            };
        };
        if self.shutting_down {
            self.closing_allocations.insert(open_id);
            return BridgeCompleteAllocate::AbortAndCloseRelay {
                closing_id: open_id,
            };
        }
        let expires_at_ms = now_ms.saturating_add(pending.lifetime_ms);
        self.live_allocations.insert(key, BridgeLiveAllocation {
            alloc_id: open_id,
            username: pending.username.clone(),
            relay_addr,
            expires_at_ms,
            permissions: BTreeMap::new(),
            channels: BTreeMap::new(),
        });
        self.cache_allocate_response(
            key,
            pending.username,
            pending.tx_id,
            response,
            expires_at_ms,
            now_ms,
        );
        BridgeCompleteAllocate::Installed { alloc_id: open_id }
    }

    /// Complete an in-flight `Allocate` after `allocate_relay` failed (no relay was opened).
    ///
    /// Immediately releases the pending capacity slot (or any cancelled closing marker for
    /// `open_id`) and returns `true` iff the caller should transmit `error_response` to the client.
    pub fn complete_allocate_err(
        &mut self,
        key: K,
        open_id: u64,
        error_response: Option<(Resp, u128)>,
        now_ms: u128,
    ) -> bool {
        let matching = self
            .pending_allocations
            .get(&key)
            .is_some_and(|p| p.open_id == open_id);
        if !matching {
            self.closing_allocations.remove(&open_id);
            return false;
        }
        let Some(pending) = self.pending_allocations.remove(&key) else {
            self.closing_allocations.remove(&open_id);
            return false;
        };
        if self.shutting_down {
            return false;
        }
        if let Some((resp, ttl_ms)) = error_response {
            self.cache_allocate_response(
                key,
                pending.username,
                pending.tx_id,
                resp,
                now_ms.saturating_add(ttl_ms),
                now_ms,
            );
        }
        true
    }

    /// Check whether `peer_ips` can be installed on `(key, alloc_id)` at `now_ms` without mutating
    /// permissions yet (so the caller can await `relay.allow_peer_ip` first).
    pub fn can_install_permissions(
        &mut self,
        key: &K,
        alloc_id: u64,
        peer_ips: &[IpAddr],
        max_permissions: usize,
        now_ms: u128,
    ) -> BridgePermissionCheck {
        if self.shutting_down {
            return BridgePermissionCheck::AllocationGone;
        }
        let Some(alloc) = self.live_allocations.get_mut(key) else {
            return BridgePermissionCheck::AllocationGone;
        };
        if alloc.alloc_id != alloc_id || now_ms >= alloc.expires_at_ms {
            return BridgePermissionCheck::AllocationGone;
        }
        alloc.permissions.retain(|_, &mut exp| exp > now_ms);
        let mut projected = alloc.permissions.len();
        let mut seen = BTreeSet::new();
        for &ip in peer_ips {
            if !alloc.permissions.contains_key(&ip) && seen.insert(ip) {
                projected += 1;
            }
        }
        if projected > max_permissions {
            BridgePermissionCheck::QuotaExceeded
        } else {
            BridgePermissionCheck::Permitted
        }
    }

    /// Commit `peer_ips` permissions on `(key, alloc_id)` with lifetime `perm_ttl_ms` after
    /// `relay.allow_peer_ip` succeeded.
    pub fn commit_permissions(
        &mut self,
        key: &K,
        alloc_id: u64,
        peer_ips: &[IpAddr],
        perm_ttl_ms: u128,
        max_permissions: usize,
        now_ms: u128,
    ) -> BridgePermissionCheck {
        let check = self.can_install_permissions(key, alloc_id, peer_ips, max_permissions, now_ms);
        if check != BridgePermissionCheck::Permitted {
            return check;
        }
        if let Some(alloc) = self.live_allocations.get_mut(key) {
            let new_exp = now_ms.saturating_add(perm_ttl_ms);
            for &ip in peer_ips {
                alloc.permissions.insert(ip, new_exp);
            }
        }
        BridgePermissionCheck::Permitted
    }

    /// Check whether `(channel, peer)` can be bound on `(key, alloc_id)` at `now_ms` without
    /// mutating state yet.
    pub fn can_bind_channel(
        &mut self,
        key: &K,
        alloc_id: u64,
        channel: u16,
        peer: SocketAddr,
        max_permissions: usize,
        now_ms: u128,
    ) -> std::result::Result<(), BridgeChannelBindError> {
        if self.shutting_down {
            return Err(BridgeChannelBindError::AllocationGone);
        }
        let Some(alloc) = self.live_allocations.get_mut(key) else {
            return Err(BridgeChannelBindError::AllocationGone);
        };
        if alloc.alloc_id != alloc_id || now_ms >= alloc.expires_at_ms {
            return Err(BridgeChannelBindError::AllocationGone);
        }
        alloc.permissions.retain(|_, &mut exp| exp > now_ms);
        alloc.channels.retain(|_, (_, exp)| *exp > now_ms);

        for (&bound_ch, &(bound_peer, _)) in &alloc.channels {
            if (bound_ch == channel && bound_peer != peer)
                || (bound_peer == peer && bound_ch != channel)
            {
                return Err(BridgeChannelBindError::Conflict);
            }
        }
        if !alloc.permissions.contains_key(&peer.ip()) && alloc.permissions.len() >= max_permissions
        {
            return Err(BridgeChannelBindError::PermissionQuotaExceeded);
        }
        Ok(())
    }

    /// Commit a `(channel, peer)` binding together with its `peer.ip()` permission after
    /// `relay.allow_peer_ip` succeeded.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_channel_bind(
        &mut self,
        key: &K,
        alloc_id: u64,
        channel: u16,
        peer: SocketAddr,
        channel_ttl_ms: u128,
        perm_ttl_ms: u128,
        max_permissions: usize,
        now_ms: u128,
    ) -> std::result::Result<(), BridgeChannelBindError> {
        self.can_bind_channel(key, alloc_id, channel, peer, max_permissions, now_ms)?;
        if let Some(alloc) = self.live_allocations.get_mut(key) {
            alloc
                .permissions
                .insert(peer.ip(), now_ms.saturating_add(perm_ttl_ms));
            alloc
                .channels
                .insert(channel, (peer, now_ms.saturating_add(channel_ttl_ms)));
        }
        Ok(())
    }

    /// Return `true` iff `(key, alloc_id)` is live at `now_ms` and holds an unexpired RFC 5766
    /// permission for `ip`.
    pub fn has_permission(&mut self, key: &K, alloc_id: u64, ip: IpAddr, now_ms: u128) -> bool {
        if self.shutting_down {
            return false;
        }
        let Some(alloc) = self.live_allocations.get_mut(key) else {
            return false;
        };
        if alloc.alloc_id != alloc_id || now_ms >= alloc.expires_at_ms {
            return false;
        }
        alloc.permissions.retain(|_, &mut exp| exp > now_ms);
        alloc.permissions.contains_key(&ip)
    }

    /// Resolve inbound forwarding for a datagram from `peer` on `(key, alloc_id)` at `now_ms`:
    /// returns `None` if the allocation is gone/expired or lacks an unexpired permission for
    /// `peer.ip()`, or `Some(Some(channel))` / `Some(None)` when forwarding is permitted via
    /// `ChannelData` / `Data` indication respectively.
    pub fn inbound_forwarding_channel(
        &mut self,
        key: &K,
        alloc_id: u64,
        peer: SocketAddr,
        now_ms: u128,
    ) -> Option<Option<u16>> {
        if self.shutting_down {
            return None;
        }
        let alloc = self.live_allocations.get_mut(key)?;
        if alloc.alloc_id != alloc_id || now_ms >= alloc.expires_at_ms {
            return None;
        }
        alloc.permissions.retain(|_, &mut exp| exp > now_ms);
        alloc.channels.retain(|_, (_, exp)| *exp > now_ms);
        if !alloc.permissions.contains_key(&peer.ip()) {
            return None;
        }
        let channel = alloc
            .channels
            .iter()
            .find_map(|(&ch, &(bound_peer, _))| (bound_peer == peer).then_some(ch));
        Some(channel)
    }

    /// Resolve outbound `ChannelData` destination for `channel` on `(key, alloc_id)` at `now_ms`.
    ///
    /// Per RFC 5766 §11.6, outbound `ChannelData` is forwarded whenever the channel binding itself
    /// is unexpired (default `600s`), even after the `300s` inbound permission created/refreshed
    /// by `ChannelBind` has elapsed (inbound peer datagrams and `Send` indications continue to
    /// require an unexpired permission).
    pub fn outbound_channel_peer(
        &mut self,
        key: &K,
        alloc_id: u64,
        channel: u16,
        now_ms: u128,
    ) -> Option<SocketAddr> {
        if self.shutting_down {
            return None;
        }
        let alloc = self.live_allocations.get_mut(key)?;
        if alloc.alloc_id != alloc_id || now_ms >= alloc.expires_at_ms {
            return None;
        }
        alloc.channels.retain(|_, (_, exp)| *exp > now_ms);
        alloc.channels.get(&channel).map(|&(p, _)| p)
    }

    /// Apply a `Refresh` request (`lifetime_ms == 0` deletes; `> 0` extends if still live).
    pub fn refresh(
        &mut self,
        key: &K,
        alloc_id: u64,
        lifetime_ms: u128,
        now_ms: u128,
    ) -> BridgeRefreshOutcome {
        let Some(current_id) = self.live_allocations.get(key).map(|a| a.alloc_id) else {
            return BridgeRefreshOutcome::NotFound;
        };
        if current_id != alloc_id {
            return BridgeRefreshOutcome::NotFound;
        }
        if lifetime_ms == 0 {
            self.live_allocations.remove(key);
            self.cached_allocate.remove(key);
            self.closing_allocations.insert(alloc_id);
            return BridgeRefreshOutcome::Deleted {
                close_alloc_id: alloc_id,
            };
        }
        let expired = self
            .live_allocations
            .get(key)
            .is_some_and(|a| now_ms >= a.expires_at_ms);
        if expired {
            self.live_allocations.remove(key);
            self.cached_allocate.remove(key);
            self.closing_allocations.insert(alloc_id);
            return BridgeRefreshOutcome::Expired {
                close_alloc_id: alloc_id,
            };
        }
        let expires_at_ms = now_ms.saturating_add(lifetime_ms);
        if let Some(alloc) = self.live_allocations.get_mut(key) {
            alloc.expires_at_ms = expires_at_ms;
        }
        if let Some(cached) = self.cached_allocate.get_mut(key) {
            cached.expires_at_ms = expires_at_ms;
        }
        BridgeRefreshOutcome::Refreshed { expires_at_ms }
    }

    /// Expire `(key, alloc_id)` if `now_ms >= expires_at_ms`, moving it to `closing_allocations`.
    pub fn expire_if_due(&mut self, key: &K, alloc_id: u64, now_ms: u128) -> BridgeExpireOutcome {
        let Some(alloc) = self.live_allocations.get(key) else {
            return BridgeExpireOutcome::AlreadyRemoved;
        };
        if alloc.alloc_id != alloc_id {
            return BridgeExpireOutcome::AlreadyRemoved;
        }
        if now_ms < alloc.expires_at_ms {
            return BridgeExpireOutcome::StillLive {
                expires_at_ms: alloc.expires_at_ms,
            };
        }
        self.live_allocations.remove(key);
        self.cached_allocate.remove(key);
        self.closing_allocations.insert(alloc_id);
        BridgeExpireOutcome::Expired {
            close_alloc_id: alloc_id,
        }
    }

    /// Remove `(key, alloc_id)` unconditionally if still current (e.g. when `recv_datagram` ends),
    /// moving `alloc_id` into `closing_allocations`.
    pub fn remove_if_current(&mut self, key: &K, alloc_id: u64) -> Option<u64> {
        if self
            .live_allocations
            .get(key)
            .is_some_and(|a| a.alloc_id == alloc_id)
        {
            self.live_allocations.remove(key);
            self.cached_allocate.remove(key);
            self.closing_allocations.insert(alloc_id);
            Some(alloc_id)
        } else {
            None
        }
    }

    /// Remove all live allocations matching `username` (plus any expired allocations at `now_ms`),
    /// moving their IDs into `closing_allocations`.
    pub fn remove_by_username(&mut self, username: &str, now_ms: u128) -> Vec<u64> {
        let keys: Vec<K> = self
            .live_allocations
            .iter()
            .filter(|(_, a)| a.username == username || now_ms >= a.expires_at_ms)
            .map(|(&k, _)| k)
            .collect();
        let mut ids = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(alloc) = self.live_allocations.remove(&key) {
                self.cached_allocate.remove(&key);
                self.closing_allocations.insert(alloc.alloc_id);
                ids.push(alloc.alloc_id);
            }
        }
        ids
    }

    /// Handle TCP peer disconnect on `key`, cancelling any in-flight `Allocate` and moving any
    /// live allocation into `closing_allocations`.
    pub fn disconnect_tuple(&mut self, key: &K) -> BridgeDisconnectOutcome {
        self.cached_allocate.remove(key);
        let cancelled_open_id = self.pending_allocations.remove(key).map(|p| {
            self.closing_allocations.insert(p.open_id);
            p.open_id
        });
        let closed_alloc_id = self.live_allocations.remove(key).map(|a| {
            self.closing_allocations.insert(a.alloc_id);
            a.alloc_id
        });
        BridgeDisconnectOutcome {
            cancelled_open_id,
            closed_alloc_id,
        }
    }

    /// Enter terminal shutdown (`TurnServer::close()`): rejects all new reservations/allocations,
    /// clears unclaimed tickets and response caches, moves all pending and live allocations into
    /// `closing_allocations`, and returns the `alloc_id`s of previously live allocations to close.
    pub fn begin_shutdown(&mut self) -> Vec<u64> {
        self.shutting_down = true;
        self.unclaimed_tickets.clear();
        self.cached_allocate.clear();
        let pending_ids: Vec<u64> = self
            .pending_allocations
            .values()
            .map(|p| p.open_id)
            .collect();
        self.pending_allocations.clear();
        for id in pending_ids {
            self.closing_allocations.insert(id);
        }
        let mut live_ids = Vec::with_capacity(self.live_allocations.len());
        let drained: Vec<(K, BridgeLiveAllocation)> = std::mem::take(&mut self.live_allocations)
            .into_iter()
            .collect();
        for (_, alloc) in drained {
            self.closing_allocations.insert(alloc.alloc_id);
            live_ids.push(alloc.alloc_id);
        }
        live_ids
    }

    /// Record completion of relay teardown for `id`, releasing its allocation capacity slot.
    pub fn finish_teardown(&mut self, id: u64) -> bool {
        self.closing_allocations.remove(&id)
    }
}
