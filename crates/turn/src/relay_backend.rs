//! Transport-agnostic datagram relay backend traits and the pure allocation state machine.

use std::any::Any;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use turn::allocation::five_tuple::FiveTuple;

/// Opaque handle returned by [`TurnDatagramRelay::begin_close`] and passed to
/// [`TurnDatagramRelay::finish_close`].
pub type TurnRelayCloseHandle = Option<Box<dyn Any + Send>>;

/// Transport-agnostic unconnected UDP datagram relay bound for a single TURN allocation.
///
/// Implemented both by the local OS socket backend (`PolicyRelayConn`) and by external multi-hop
/// datagram transports (such as `rings-turn-service`'s `onion::udp` bridge).
#[async_trait]
pub trait TurnDatagramRelay: Send + Sync + 'static {
    /// Externally reachable UDP endpoint (`E_exit`) advertised to the TURN client in
    /// `XOR-RELAYED-ADDRESS`.
    fn relayed_addr(&self) -> SocketAddr;

    /// Receive the next inbound UDP datagram `(len, peer_src)` arriving at the relay endpoint.
    async fn recv_datagram(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>;

    /// Transmit one outbound UDP datagram `buf` from the relay endpoint to `target`.
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize>;

    /// Install or refresh inbound peer-IP pinholes (`ttl`) on a stateful relay backend before
    /// acknowledging `CreatePermission` or `ChannelBind`.
    async fn allow_peer_ips(&self, peer_ips: &[IpAddr], ttl: Duration) -> io::Result<()> {
        let _ = (peer_ips, ttl);
        Ok(())
    }

    /// Synchronously initiate teardown without waiting on any parked [`Self::recv_datagram`] call.
    fn begin_close(&self) -> TurnRelayCloseHandle;

    /// Complete asynchronous teardown of the relay session after [`Self::begin_close`].
    async fn finish_close(&self, handle: TurnRelayCloseHandle);
}

/// Factory opening fresh [`TurnDatagramRelay`] sessions for authenticated `Allocate` requests.
#[async_trait]
pub trait TurnDatagramRelayFactory: Send + Sync + 'static {
    /// Return whether this factory can allocate an IPv4 (`use_ipv4 == true`) or IPv6 (`false`)
    /// relay endpoint.
    fn supports_family(&self, use_ipv4: bool) -> bool;

    /// Open a new datagram relay endpoint (`need_even_port` when RFC 5766 `EVEN-PORT` is set).
    async fn allocate_relay(
        &self,
        use_ipv4: bool,
        need_even_port: bool,
    ) -> io::Result<Arc<dyn TurnDatagramRelay>>;
}

/// Deterministic, hashable, total-order representation of a TURN client 5-tuple.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TurnFiveTupleKey {
    /// Client source address.
    pub src_addr: SocketAddr,
    /// TURN listener destination address.
    pub dst_addr: SocketAddr,
    /// Transport protocol number (`17` UDP, `6` TCP).
    pub protocol: u8,
}

impl From<FiveTuple> for TurnFiveTupleKey {
    fn from(tuple: FiveTuple) -> Self {
        Self {
            src_addr: tuple.src_addr,
            dst_addr: tuple.dst_addr,
            protocol: tuple.protocol.0,
        }
    }
}

/// Error returned when reserving a single-use session ticket in [`TurnAllocationCore`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreReserveError {
    /// Server is shutting down and refuses new reservations.
    ShuttingDown,
    /// Unclaimed reservation table is at `max_reservations`.
    CapacityReached,
    /// A reservation with the same `session_id` already exists.
    DuplicateSessionId,
}

/// Error returned when installing peer-IP permissions on an allocation in [`TurnAllocationCore`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorePermissionError {
    /// No live allocation exists for the 5-tuple; `expired_open_id` carries any lazily evicted
    /// expired allocation whose relay must be closed.
    AllocationMismatch {
        /// Expired `open_id` evicted during lookup, if any.
        expired_open_id: Option<u64>,
    },
    /// Adding the requested peer IPs would exceed `max_permissions_per_allocation`.
    QuotaExceeded,
}

/// Synchronous transition decision produced by [`TurnAllocationCore::begin_allocate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BeginAllocateDecision {
    /// Begin an asynchronous relay open under `open_id`; capacity and single-use ticket (if any)
    /// have been atomically reserved.
    StartOpen {
        /// Monotonic identifier correlating the in-flight open with [`TurnAllocationCore::complete_allocate`].
        open_id: u64,
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
    /// Identical `(five_tuple, tx_id, username)` arrived while `open_id` is still opening; coalesce
    /// onto the in-flight open without re-consuming the single-use ticket.
    CoalescePending {
        /// Identifier of the in-flight open to attach to.
        open_id: u64,
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
    /// Identical `(five_tuple, tx_id, username)` arrived after the allocation already completed;
    /// replay the cached success response advertising `relayed_addr`.
    ReplayActive {
        /// Bound relay endpoint of the established allocation.
        relayed_addr: SocketAddr,
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
    /// Another pending or active allocation already owns `five_tuple` (`437 Allocation Mismatch`).
    RejectAllocationMismatch {
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
    /// Single-use session ticket is unknown or was already consumed by a different transaction (`401`).
    RejectTicketUnknown {
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
    /// Single-use session ticket expired before `Allocate` arrived (`401`).
    RejectTicketExpired {
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
    /// Combined pending + active allocations reached `max_allocations` (`508 Insufficient Capacity`).
    RejectInsufficientCapacity {
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
    /// Server is shutting down (`508` / drop).
    RejectShuttingDown {
        /// Expired active `open_id`s pruned during this step whose relays must be closed.
        expired_open_ids: Vec<u64>,
    },
}

impl BeginAllocateDecision {
    /// Borrow any expired active `open_id`s pruned during [`TurnAllocationCore::begin_allocate`].
    pub fn expired_open_ids(&self) -> &[u64] {
        match self {
            Self::StartOpen {
                expired_open_ids, ..
            }
            | Self::CoalescePending {
                expired_open_ids, ..
            }
            | Self::ReplayActive {
                expired_open_ids, ..
            }
            | Self::RejectAllocationMismatch {
                expired_open_ids, ..
            }
            | Self::RejectTicketUnknown {
                expired_open_ids, ..
            }
            | Self::RejectTicketExpired {
                expired_open_ids, ..
            }
            | Self::RejectInsufficientCapacity {
                expired_open_ids, ..
            }
            | Self::RejectShuttingDown {
                expired_open_ids, ..
            } => expired_open_ids,
        }
    }
}

/// Synchronous transition decision produced by [`TurnAllocationCore::complete_allocate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompleteAllocateDecision {
    /// Allocation installed in `active_allocations` with `XOR-RELAYED-ADDRESS == relayed_addr`.
    Installed {
        /// Monotonic identifier of the installed allocation.
        open_id: u64,
        /// Externally reachable relay endpoint (`E_exit`).
        relayed_addr: SocketAddr,
        /// Initial transaction ID that opened this allocation.
        tx_id: [u8; 12],
    },
    /// Relay open failed while the pending entry was still live; return `508 Insufficient Capacity`.
    RespondOpenError {
        /// Transaction ID of the failed `Allocate` request.
        tx_id: [u8; 12],
    },
    /// Pending entry was cancelled (client disconnected or server began shutdown); close any
    /// opened relay immediately without installing it.
    AbortRelay,
}

/// Synchronous transition decision produced by [`TurnAllocationCore::refresh`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshDecision {
    /// Live allocation refreshed to a positive lifetime.
    Refreshed {
        /// Identifier of the refreshed allocation.
        open_id: u64,
    },
    /// Live allocation deleted by `Lifetime(0)`; caller must close `open_id`'s relay.
    Deleted {
        /// Identifier of the deleted allocation to close.
        open_id: u64,
    },
    /// No live allocation matched (`437 Allocation Mismatch`); `expired_open_id` must be closed if
    /// an expired entry was lazily evicted.
    Mismatch {
        /// Expired `open_id` evicted during lookup, if any.
        expired_open_id: Option<u64>,
    },
}

/// Metadata recorded for an in-flight asynchronous `Allocate` relay open.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PendingAllocateSpec {
    /// Monotonic open identifier.
    pub open_id: u64,
    /// RFC 5389 96-bit transaction ID.
    pub tx_id: [u8; 12],
    /// Authenticated TURN username.
    pub username: String,
    /// Clamped allocation lifetime in milliseconds to apply upon install.
    pub lifetime_ms: u128,
}

/// Metadata recorded for an established active TURN allocation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ActiveAllocationSpec {
    /// Monotonic open identifier.
    pub open_id: u64,
    /// RFC 5389 96-bit transaction ID of the creating `Allocate` request.
    pub tx_id: [u8; 12],
    /// Authenticated TURN username owning this allocation.
    pub username: String,
    /// Externally reachable relay socket address (`E_exit`).
    pub relayed_addr: SocketAddr,
    /// Expiry timestamp (`now_ms` units).
    pub expires_at_ms: u128,
    /// Active peer-IP permissions (`peer_ip -> expires_at_ms`).
    pub permissions: BTreeMap<IpAddr, u128>,
}

/// Pure, deterministic state machine governing TURN ticket reservations, pending asynchronous
/// `Allocate` opens, active allocations, permissions, expiry, and shutdown.
///
/// Designed with ordered maps (`BTreeMap`) and derived `Eq + Hash` so both production
/// [`crate::TurnServer`] and exhaustive Stateright model checks execute the exact same transition
/// functions.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TurnAllocationCore {
    max_allocations: usize,
    max_reservations: usize,
    max_permissions_per_allocation: usize,
    shutting_down: bool,
    next_open_id: u64,
    reservations: BTreeMap<[u8; 16], u128>,
    pending_opens: BTreeMap<TurnFiveTupleKey, PendingAllocateSpec>,
    active_allocations: BTreeMap<TurnFiveTupleKey, ActiveAllocationSpec>,
}

impl TurnAllocationCore {
    /// Construct a new allocation state machine with explicit capacity bounds.
    pub const fn new(
        max_allocations: usize,
        max_reservations: usize,
        max_permissions_per_allocation: usize,
    ) -> Self {
        Self {
            max_allocations,
            max_reservations,
            max_permissions_per_allocation,
            shutting_down: false,
            next_open_id: 1,
            reservations: BTreeMap::new(),
            pending_opens: BTreeMap::new(),
            active_allocations: BTreeMap::new(),
        }
    }

    /// Maximum concurrent pending + active allocations allowed.
    pub const fn max_allocations(&self) -> usize {
        self.max_allocations
    }

    /// Maximum concurrent unclaimed ticket reservations allowed.
    pub const fn max_reservations(&self) -> usize {
        self.max_reservations
    }

    /// Return whether [`Self::begin_shutdown`] has been invoked.
    pub const fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Current number of stored unclaimed reservations (before lazy expiry).
    pub fn reservation_len(&self) -> usize {
        self.reservations.len()
    }

    /// Current number of in-flight pending `Allocate` opens.
    pub fn pending_open_len(&self) -> usize {
        self.pending_opens.len()
    }

    /// Current number of stored active allocations (before lazy expiry).
    pub fn active_allocation_len(&self) -> usize {
        self.active_allocations.len()
    }

    /// Borrow the pending open table.
    pub const fn pending_opens(&self) -> &BTreeMap<TurnFiveTupleKey, PendingAllocateSpec> {
        &self.pending_opens
    }

    /// Borrow the active allocation table.
    pub const fn active_allocations(&self) -> &BTreeMap<TurnFiveTupleKey, ActiveAllocationSpec> {
        &self.active_allocations
    }

    /// Prune expired unclaimed reservations (`now_ms > expires_at_ms`) and expired active
    /// allocations (`expires_at_ms <= now_ms`), returning evicted active `open_id`s.
    pub fn prune_expired(&mut self, now_ms: u128) -> Vec<u64> {
        self.reservations
            .retain(|_, &mut expires_at_ms| now_ms <= expires_at_ms);
        let expired_keys: Vec<TurnFiveTupleKey> = self
            .active_allocations
            .iter()
            .filter(|(_, alloc)| alloc.expires_at_ms <= now_ms)
            .map(|(k, _)| *k)
            .collect();
        let mut evicted_ids = Vec::with_capacity(expired_keys.len());
        for key in expired_keys {
            if let Some(removed) = self.active_allocations.remove(&key) {
                evicted_ids.push(removed.open_id);
            }
        }
        evicted_ids
    }

    /// Record a newly issued single-use session ticket expiring at `expires_at_ms`.
    pub fn reserve_ticket(
        &mut self,
        session_id: [u8; 16],
        expires_at_ms: u128,
        now_ms: u128,
    ) -> std::result::Result<(), CoreReserveError> {
        self.reservations.retain(|_, &mut exp_ms| now_ms <= exp_ms);
        if self.shutting_down {
            return Err(CoreReserveError::ShuttingDown);
        }
        if self.reservations.contains_key(&session_id) {
            return Err(CoreReserveError::DuplicateSessionId);
        }
        if self.reservations.len() >= self.max_reservations {
            return Err(CoreReserveError::CapacityReached);
        }
        self.reservations.insert(session_id, expires_at_ms);
        Ok(())
    }

    /// Synchronously arbitrate an authenticated `Allocate` request before yielding to open a relay
    /// session.
    ///
    /// Enforces:
    /// - Idempotent replay on active `(tuple, tx_id, username)` and coalescing on pending
    ///   `(tuple, tx_id, username)` before consulting single-use `ticket_session_id` (Law 1).
    /// - Atomic `pending_opens.len() + active_allocations.len() <= max_allocations` reservation
    ///   before yielding (Law 6).
    pub fn begin_allocate(
        &mut self,
        tuple: TurnFiveTupleKey,
        tx_id: [u8; 12],
        username: String,
        ticket_session_id: Option<[u8; 16]>,
        lifetime_ms: u128,
        now_ms: u128,
    ) -> BeginAllocateDecision {
        self.begin_allocate_with_busy_slots(
            tuple,
            tx_id,
            username,
            ticket_session_id,
            lifetime_ms,
            now_ms,
            0,
        )
    }

    /// Variant of [`Self::begin_allocate`] accounting for `extra_busy_slots` still held by
    /// cancelling or closing relay sessions in the outer runtime.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_allocate_with_busy_slots(
        &mut self,
        tuple: TurnFiveTupleKey,
        tx_id: [u8; 12],
        username: String,
        ticket_session_id: Option<[u8; 16]>,
        lifetime_ms: u128,
        now_ms: u128,
        extra_busy_slots: usize,
    ) -> BeginAllocateDecision {
        let expired_open_ids = self.prune_expired(now_ms);
        if self.shutting_down {
            return BeginAllocateDecision::RejectShuttingDown { expired_open_ids };
        }

        if let Some(active) = self.active_allocations.get(&tuple) {
            if active.username == username && active.tx_id == tx_id {
                return BeginAllocateDecision::ReplayActive {
                    relayed_addr: active.relayed_addr,
                    expired_open_ids,
                };
            }
            return BeginAllocateDecision::RejectAllocationMismatch { expired_open_ids };
        }

        if let Some(pending) = self.pending_opens.get(&tuple) {
            if pending.username == username && pending.tx_id == tx_id {
                return BeginAllocateDecision::CoalescePending {
                    open_id: pending.open_id,
                    expired_open_ids,
                };
            }
            return BeginAllocateDecision::RejectAllocationMismatch { expired_open_ids };
        }

        if let Some(session_id) = ticket_session_id {
            // Note: prune_expired(now_ms) already removed tickets with now_ms > expires_at_ms, so
            // check raw lookup cleanly by keeping expired detection explicit when present.
            let Some(&expires_at_ms) = self.reservations.get(&session_id) else {
                return BeginAllocateDecision::RejectTicketUnknown { expired_open_ids };
            };
            if now_ms > expires_at_ms {
                self.reservations.remove(&session_id);
                return BeginAllocateDecision::RejectTicketExpired { expired_open_ids };
            }
        }

        if self
            .pending_opens
            .len()
            .saturating_add(self.active_allocations.len())
            .saturating_add(extra_busy_slots)
            >= self.max_allocations
        {
            if let Some(session_id) = ticket_session_id {
                self.reservations.remove(&session_id);
            }
            return BeginAllocateDecision::RejectInsufficientCapacity { expired_open_ids };
        }

        if let Some(session_id) = ticket_session_id {
            self.reservations.remove(&session_id);
        }

        let open_id = self.next_open_id;
        self.next_open_id = self.next_open_id.saturating_add(1);
        self.pending_opens.insert(tuple, PendingAllocateSpec {
            open_id,
            tx_id,
            username,
            lifetime_ms,
        });
        BeginAllocateDecision::StartOpen {
            open_id,
            expired_open_ids,
        }
    }

    /// Complete an asynchronous relay open started by [`Self::begin_allocate`].
    pub fn complete_allocate(
        &mut self,
        tuple: TurnFiveTupleKey,
        open_id: u64,
        outcome: std::result::Result<SocketAddr, ()>,
        now_ms: u128,
    ) -> CompleteAllocateDecision {
        let matches_pending = self
            .pending_opens
            .get(&tuple)
            .is_some_and(|pending| pending.open_id == open_id);
        if !matches_pending {
            return CompleteAllocateDecision::AbortRelay;
        }
        let Some(pending) = self.pending_opens.remove(&tuple) else {
            return CompleteAllocateDecision::AbortRelay;
        };
        if self.shutting_down {
            return CompleteAllocateDecision::AbortRelay;
        }
        match outcome {
            Ok(relayed_addr) => {
                let expires_at_ms = now_ms.saturating_add(pending.lifetime_ms);
                self.active_allocations.insert(tuple, ActiveAllocationSpec {
                    open_id: pending.open_id,
                    tx_id: pending.tx_id,
                    username: pending.username,
                    relayed_addr,
                    expires_at_ms,
                    permissions: BTreeMap::new(),
                });
                CompleteAllocateDecision::Installed {
                    open_id: pending.open_id,
                    relayed_addr,
                    tx_id: pending.tx_id,
                }
            }
            Err(()) => CompleteAllocateDecision::RespondOpenError {
                tx_id: pending.tx_id,
            },
        }
    }

    /// Install or refresh peer-IP permissions (`ttl_ms`) on `tuple`'s live allocation.
    pub fn install_permissions(
        &mut self,
        tuple: &TurnFiveTupleKey,
        peer_ips: &[IpAddr],
        ttl_ms: u128,
        now_ms: u128,
    ) -> std::result::Result<(), CorePermissionError> {
        let is_expired = self
            .active_allocations
            .get(tuple)
            .is_some_and(|alloc| alloc.expires_at_ms <= now_ms);
        if is_expired {
            let expired_open_id = self.active_allocations.remove(tuple).map(|a| a.open_id);
            return Err(CorePermissionError::AllocationMismatch { expired_open_id });
        }
        let Some(alloc) = self.active_allocations.get_mut(tuple) else {
            return Err(CorePermissionError::AllocationMismatch {
                expired_open_id: None,
            });
        };
        alloc.permissions.retain(|_, &mut exp| exp > now_ms);
        let mut projected_len = alloc.permissions.len();
        let mut seen_new = BTreeSet::new();
        for &ip in peer_ips {
            if !alloc.permissions.contains_key(&ip) && seen_new.insert(ip) {
                projected_len = projected_len.saturating_add(1);
            }
        }
        if projected_len > self.max_permissions_per_allocation {
            return Err(CorePermissionError::QuotaExceeded);
        }
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        for &ip in peer_ips {
            alloc.permissions.insert(ip, expires_at_ms);
        }
        Ok(())
    }

    /// Return whether `tuple` has a live allocation whose permission table admits `peer_ip` at
    /// `now_ms`.
    pub fn has_permission(
        &mut self,
        tuple: &TurnFiveTupleKey,
        peer_ip: IpAddr,
        now_ms: u128,
    ) -> bool {
        let Some(alloc) = self.active_allocations.get_mut(tuple) else {
            return false;
        };
        if alloc.expires_at_ms <= now_ms {
            return false;
        }
        alloc.permissions.retain(|_, &mut exp| exp > now_ms);
        alloc.permissions.contains_key(&peer_ip)
    }

    /// Refresh or delete (`lifetime_ms == 0`) the allocation at `tuple`.
    pub fn refresh(
        &mut self,
        tuple: &TurnFiveTupleKey,
        lifetime_ms: u128,
        now_ms: u128,
    ) -> RefreshDecision {
        let Some(existing) = self.active_allocations.get(tuple) else {
            return RefreshDecision::Mismatch {
                expired_open_id: None,
            };
        };
        if existing.expires_at_ms <= now_ms {
            let expired = self.active_allocations.remove(tuple).map(|a| a.open_id);
            return RefreshDecision::Mismatch {
                expired_open_id: expired,
            };
        }
        if lifetime_ms == 0 {
            let Some(removed) = self.active_allocations.remove(tuple) else {
                return RefreshDecision::Mismatch {
                    expired_open_id: None,
                };
            };
            return RefreshDecision::Deleted {
                open_id: removed.open_id,
            };
        }
        let Some(alloc) = self.active_allocations.get_mut(tuple) else {
            return RefreshDecision::Mismatch {
                expired_open_id: None,
            };
        };
        alloc.expires_at_ms = now_ms.saturating_add(lifetime_ms);
        let open_id = alloc.open_id;
        RefreshDecision::Refreshed { open_id }
    }

    /// Remove `tuple` if its current `open_id` equals `expected_open_id`.
    pub fn remove_if_open_id(&mut self, tuple: &TurnFiveTupleKey, expected_open_id: u64) -> bool {
        if self
            .active_allocations
            .get(tuple)
            .is_some_and(|alloc| alloc.open_id == expected_open_id)
        {
            self.active_allocations.remove(tuple);
            true
        } else {
            false
        }
    }

    /// Cancel any pending open and remove any active allocation for `tuple` (e.g. TCP disconnect),
    /// returning the active `open_id` if one was installed.
    pub fn disconnect_tuple(&mut self, tuple: &TurnFiveTupleKey) -> Option<u64> {
        self.pending_opens.remove(tuple);
        self.active_allocations.remove(tuple).map(|a| a.open_id)
    }

    /// Remove all active allocations matching `username` (plus any expired entries at `now_ms`),
    /// returning all removed `open_id`s.
    pub fn remove_by_username(&mut self, username: &str, now_ms: u128) -> Vec<u64> {
        let pending_keys: Vec<TurnFiveTupleKey> = self
            .pending_opens
            .iter()
            .filter(|(_, p)| p.username == username)
            .map(|(k, _)| *k)
            .collect();
        for key in pending_keys {
            self.pending_opens.remove(&key);
        }
        let keys: Vec<TurnFiveTupleKey> = self
            .active_allocations
            .iter()
            .filter(|(_, a)| a.username == username || a.expires_at_ms <= now_ms)
            .map(|(k, _)| *k)
            .collect();
        let mut removed = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(alloc) = self.active_allocations.remove(&key) {
                removed.push(alloc.open_id);
            }
        }
        removed
    }

    /// Enter terminal shutdown state, clearing reservations and pending opens and returning all
    /// active `open_id`s to close.
    pub fn begin_shutdown(&mut self) -> Vec<u64> {
        self.shutting_down = true;
        self.reservations.clear();
        self.pending_opens.clear();
        let old = std::mem::take(&mut self.active_allocations);
        old.into_values().map(|a| a.open_id).collect()
    }
}
