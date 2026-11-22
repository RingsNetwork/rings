//! Finite exhaustive Stateright model of the TURN entry ↔ `onion::udp` session bridge (#1292).
//!
//! Wires Stateright directly to the production state machines:
//! - [`rings_turn::bridge::TurnEntryBridge`] (ticket reservations, pre-yield pending `Allocate`
//!   registration & retransmission coalescing, cached final `Allocate` responses, `XOR-RELAYED-ADDRESS`
//!   projection, RFC 5766 `300s` peer-IP permission gate, `Refresh`, TCP disconnect, shutdown
//!   fence, and atomic capacity accounting), and
//! - [`rings_node::onion::udp::OnionUdpPinholes`] (`600s` stateful exit IP pinhole table driven by
//!   `AllowPeer` / `PeerAllowed` session frames).
//!
//! Exercises non-atomic steps (`BeginAllocate` → async `CompleteOpenOk`/`CompleteOpenErr`,
//! `BeginPermission` → `DeliverAllowPeer` → `DeliverPeerAllowed`), frame loss and duplication on
//! both client→exit and exit→client streams, clock expiry across `60s` ticket / `300s` permission /
//! `600s` pinhole+allocation horizons, `Refresh(0)`, TCP disconnect, dead stream reader during
//! teardown, and `TurnServer::close()`.
//!
//! Verifies Laws 1, 2, 3, 4, and 6 on the production implementation and confirms Stateright
//! refutes seeded mutants for every law (including all four bugs fixed during PR #1294 review).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;

use rings_node::onion::udp::OnionUdpPinholes;
use rings_node::onion::udp::DEFAULT_MAX_UDP_PINHOLES;
use rings_node::onion::udp::DEFAULT_UDP_PINHOLE_TTL_MS;
use rings_turn::bridge::BridgeAllocateStart;
use rings_turn::bridge::BridgeCompleteAllocate;
use rings_turn::bridge::BridgePermissionCheck;
use rings_turn::bridge::BridgeRefreshOutcome;
use rings_turn::bridge::TurnEntryBridge;
use rings_turn::bridge::BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS;
use rings_turn::bridge::BRIDGE_PERMISSION_TTL_MS;
use stateright::Checker;
use stateright::Model;
use stateright::Property;

const MAX_ALLOCATIONS: usize = 1;
const MAX_RESERVATIONS: usize = 1;
const MAX_PERMISSIONS: usize = 4;
const RESERVATION_TTL_MS: u128 = 60_000;
const ALLOCATION_LIFETIME_MS: u128 = 600_000;
const MAX_INITIATION_BUDGET: u8 = 6;

/// Clock epochs:
/// - `0` (`1_000 ms`): initial epoch.
/// - `1` (`62_000 ms`): past `60s` unclaimed ticket TTL; in-flight/claimed `Allocate` must still
///   retransmit idempotently without returning `401`.
/// - `2` (`365_000 ms`): past `300s` RFC 5766 permission TTL while `600s` exit pinhole & allocation
///   are still live.
/// - `3` (`1_000_000 ms`): past `600s` allocation and exit pinhole TTLs.
const CLOCK_EPOCHS_MS: [u128; 4] = [1_000, 62_000, 365_000, 1_000_000];

const ENTRY_LOCAL_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 3478);
const EXIT_ADDR_A: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 77)), 41000);
const EXIT_ADDR_B: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 77)), 41002);
const PEER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 50000);

fn ticket_bytes(ticket_idx: u8) -> [u8; 16] {
    [0xA0_u8.wrapping_add(ticket_idx); 16]
}

fn ticket_username(ticket_idx: u8) -> String {
    format!("tkt-{ticket_idx}")
}

fn exit_addr_for_open(open_id: u64) -> SocketAddr {
    if open_id % 2 == 1 {
        EXIT_ADDR_A
    } else {
        EXIT_ADDR_B
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Mutant {
    /// Law 1 (PR #1294 bug): yields to `allocate_relay` before registering `PendingAllocate`, so a
    /// retransmitted `Allocate` sees a consumed ticket without a pending entry and returns `401`.
    PendingAllocateNotRegisteredBeforeYield,
    /// Law 1: fails to consume the single-use `SessionTicket` upon claim, letting a second 5-tuple
    /// claim the same ticket.
    TicketReusableAcrossTuples,
    /// Law 2: projects the TURN entry's local address (`ENTRY_LOCAL_ADDR`) into
    /// `XOR-RELAYED-ADDRESS` instead of the opened `onion::udp` session's `E_exit`.
    RelayedAddressUsesEntryLocalAddr,
    /// Law 3: forwards inbound exit datagrams whenever the `600s` exit pinhole is open without
    /// enforcing `TurnEntryBridge`'s `300s` RFC 5766 permission gate.
    BypassEntryPermissionOnInbound,
    /// Law 4 (PR #1294 bug): blocks relay teardown completion when the upstream stream reader is
    /// dead (`reader_dead == true`), stranding closing allocations and exit sockets.
    TeardownWaitsOnDeadReader,
    /// Law 4 (PR #1294 bug): installs an in-flight backend open into `live_allocations` even after
    /// `TurnServer::close()` (`begin_shutdown`) has begun.
    InstallAfterShutdown,
    /// Law 6 (PR #1294 bug): checks `live_allocation_len() < max_allocations` instead of atomic
    /// `used_allocation_slots() < max_allocations`, admitting concurrent opens past capacity.
    NonAtomicCapacityCheck,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum AllocateResp {
    Ok {
        alloc_id: u64,
        relayed_addr: SocketAddr,
    },
    Err401Unauthorized,
    Err437AllocationMismatch,
    Err500BackendFailed,
    Err508InsufficientCapacity,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum StepWitness {
    CoalescedInFlight,
    ReplayCached,
    EndToEndDatagram,
    PermissionExpiredDrop,
    DeadReaderTeardown,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct InFlightBackendOpen {
    tuple: u8,
    ticket_idx: u8,
    tx_id: u8,
    open_id: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExitSessionState {
    bound_addr: SocketAddr,
    pinholes: OnionUdpPinholes,
    wire_allow_peer: u8,
    wire_peer_allowed: u8,
    awaiting_perm_commit: bool,
    reader_dead: bool,
    closing: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RetransmitWindow {
    /// `Allocate` claimed `ticket_idx` and backend open is currently in flight (no final response
    /// produced yet, or final response recorded once open completes). Retransmissions must never
    /// return `401` and must match `final_resp` once set.
    Active {
        final_resp: Option<AllocateResp>,
        expires_at_ms: u128,
    },
    /// Allocation or cache window was terminated by `Refresh(0)`, `TcpDisconnect`, `close()`, or
    /// clock expiry; replaying the single-use ticket must never resurrect `AllocateResp::Ok`.
    Retired,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BridgeModelState {
    budget_used: u8,
    clock_epoch: u8,
    tickets_issued_mask: u8,

    bridge: TurnEntryBridge<u8, u8, AllocateResp>,
    in_flight_opens: BTreeMap<u64, InFlightBackendOpen>,
    exit_sessions: BTreeMap<u64, ExitSessionState>,

    ticket_claimed_tuples: BTreeMap<u8, BTreeSet<u8>>,
    retransmit_windows: BTreeMap<(u8, u8, u8), RetransmitWindow>,

    law1_violation: bool,
    law2_violation: bool,
    law3_violation: bool,
    law4_install_after_shutdown: bool,
    law6_violation: bool,
    last_witness: Option<StepWitness>,
}

impl BridgeModelState {
    fn now_ms(&self) -> u128 {
        CLOCK_EPOCHS_MS[usize::from(self.clock_epoch)]
    }

    fn total_tracked_relays(&self) -> usize {
        self.in_flight_opens
            .len()
            .saturating_add(self.exit_sessions.len())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum BridgeAction {
    IssueTicket {
        ticket_idx: u8,
    },
    SendAllocate {
        tuple: u8,
        ticket_idx: u8,
        tx_id: u8,
    },
    CompleteOpenOk {
        open_id: u64,
    },
    CompleteOpenErr {
        open_id: u64,
    },
    BeginCreatePermission {
        tuple: u8,
    },
    DuplicateAllowPeer {
        alloc_id: u64,
    },
    DropAllowPeer {
        alloc_id: u64,
    },
    DeliverAllowPeer {
        alloc_id: u64,
    },
    DuplicatePeerAllowed {
        alloc_id: u64,
    },
    DropPeerAllowed {
        alloc_id: u64,
    },
    DeliverPeerAllowed {
        alloc_id: u64,
    },
    ProbeInboundPeerDatagram {
        tuple: u8,
        alloc_id: u64,
    },
    RefreshExtend {
        tuple: u8,
        alloc_id: u64,
    },
    RefreshZero {
        tuple: u8,
        alloc_id: u64,
    },
    TcpDisconnect {
        tuple: u8,
    },
    KillStreamReader {
        alloc_id: u64,
    },
    CompleteRelayClose {
        id: u64,
    },
    AdvanceClock,
    BeginShutdown,
}

#[derive(Clone, Default)]
struct BridgeModel {
    mutant: Option<Mutant>,
}

impl BridgeModel {
    fn retire_tuple_windows(state: &mut BridgeModelState, tuple: u8) {
        for ((k_tuple, _, _), window) in &mut state.retransmit_windows {
            if *k_tuple == tuple {
                *window = RetransmitWindow::Retired;
            }
        }
    }

    fn record_final_response(
        &self,
        state: &mut BridgeModelState,
        tuple: u8,
        ticket_idx: u8,
        tx_id: u8,
        resp: AllocateResp,
        expires_at_ms: Option<u128>,
    ) {
        let key = (tuple, ticket_idx, tx_id);
        match state.retransmit_windows.get_mut(&key) {
            Some(RetransmitWindow::Active {
                final_resp,
                expires_at_ms: win_exp,
            }) => {
                if resp == AllocateResp::Err401Unauthorized {
                    state.law1_violation = true;
                }
                if let Some(new_exp) = expires_at_ms {
                    *win_exp = new_exp;
                }
                match *final_resp {
                    Some(prev) if prev != resp => {
                        state.law1_violation = true;
                    }
                    Some(_) => {}
                    None => {
                        *final_resp = Some(resp);
                    }
                }
            }
            Some(RetransmitWindow::Retired) => {
                if matches!(resp, AllocateResp::Ok { .. }) {
                    state.law1_violation = true;
                }
            }
            None => {}
        }
    }

    fn mark_session_closing(state: &mut BridgeModelState, id: u64) {
        if let Some(sess) = state.exit_sessions.get_mut(&id) {
            sess.closing = true;
            sess.awaiting_perm_commit = false;
            sess.wire_allow_peer = 0;
            sess.wire_peer_allowed = 0;
        }
    }

    fn check_step_invariants(&self, state: &mut BridgeModelState) {
        for tuples in state.ticket_claimed_tuples.values() {
            if tuples.len() > 1 {
                state.law1_violation = true;
            }
        }
        for alloc in state.bridge.live_allocations().values() {
            match state.exit_sessions.get(&alloc.alloc_id) {
                Some(sess) if !sess.closing && sess.bound_addr == alloc.relay_addr => {}
                _ => {
                    state.law2_violation = true;
                }
            }
        }
        if state.bridge.is_shutting_down() && state.bridge.live_allocation_len() > 0 {
            state.law4_install_after_shutdown = true;
        }
        if state.bridge.unclaimed_ticket_len() > MAX_RESERVATIONS
            || state.bridge.used_allocation_slots() > MAX_ALLOCATIONS
            || state.total_tracked_relays() > MAX_ALLOCATIONS
        {
            state.law6_violation = true;
        }
    }

    fn has_stuck_teardown(&self, state: &BridgeModelState) -> bool {
        let mut enabled = Vec::new();
        self.actions(state, &mut enabled);
        let any_close_progress = enabled.iter().any(|act| {
            matches!(
                act,
                BridgeAction::CompleteRelayClose { .. }
                    | BridgeAction::CompleteOpenOk { .. }
                    | BridgeAction::CompleteOpenErr { .. }
            )
        });
        let has_closing = state.bridge.closing_allocation_len() > 0
            || state.exit_sessions.values().any(|s| s.closing);
        has_closing && !any_close_progress
    }
}

impl Model for BridgeModel {
    type State = BridgeModelState;
    type Action = BridgeAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![BridgeModelState {
            budget_used: 0,
            clock_epoch: 0,
            tickets_issued_mask: 0,
            bridge: TurnEntryBridge::new(MAX_ALLOCATIONS, MAX_RESERVATIONS, RESERVATION_TTL_MS),
            in_flight_opens: BTreeMap::new(),
            exit_sessions: BTreeMap::new(),
            ticket_claimed_tuples: BTreeMap::new(),
            retransmit_windows: BTreeMap::new(),
            law1_violation: false,
            law2_violation: false,
            law3_violation: false,
            law4_install_after_shutdown: false,
            law6_violation: false,
            last_witness: None,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // Free asynchronous completion / wire delivery steps (always allowed to drain).
        for &closing_id in state.bridge.closing_allocations() {
            if state.in_flight_opens.contains_key(&closing_id) {
                continue;
            }
            let blocked_by_dead_reader = self.mutant == Some(Mutant::TeardownWaitsOnDeadReader)
                && state
                    .exit_sessions
                    .get(&closing_id)
                    .is_some_and(|sess| sess.reader_dead);
            if !blocked_by_dead_reader {
                actions.push(BridgeAction::CompleteRelayClose { id: closing_id });
            }
        }

        for &open_id in state.in_flight_opens.keys() {
            actions.push(BridgeAction::CompleteOpenOk { open_id });
            actions.push(BridgeAction::CompleteOpenErr { open_id });
        }

        for (&alloc_id, sess) in &state.exit_sessions {
            if sess.closing {
                continue;
            }
            if sess.wire_allow_peer > 0 {
                actions.push(BridgeAction::DeliverAllowPeer { alloc_id });
            }
            if sess.wire_peer_allowed > 0 {
                actions.push(BridgeAction::DeliverPeerAllowed { alloc_id });
            }
        }

        // Budgeted client / fault / clock initiation steps (`budget_used < MAX_INITIATION_BUDGET`).
        if state.budget_used >= MAX_INITIATION_BUDGET {
            return;
        }

        if !state.bridge.is_shutting_down() {
            for ticket_idx in 0..2_u8 {
                if (state.tickets_issued_mask & (1 << ticket_idx)) == 0 {
                    actions.push(BridgeAction::IssueTicket { ticket_idx });
                }
            }

            if state.tickets_issued_mask != 0 {
                for tuple in 0..2_u8 {
                    for ticket_idx in 0..2_u8 {
                        if (state.tickets_issued_mask & (1 << ticket_idx)) != 0 {
                            actions.push(BridgeAction::SendAllocate {
                                tuple,
                                ticket_idx,
                                tx_id: 1,
                            });
                        }
                    }
                }
            }

            for (&tuple, alloc) in state.bridge.live_allocations() {
                let alloc_id = alloc.alloc_id;
                if let Some(sess) = state.exit_sessions.get(&alloc_id) {
                    if !sess.closing {
                        if !sess.awaiting_perm_commit {
                            actions.push(BridgeAction::BeginCreatePermission { tuple });
                        }
                        actions.push(BridgeAction::ProbeInboundPeerDatagram { tuple, alloc_id });
                        actions.push(BridgeAction::RefreshExtend { tuple, alloc_id });
                        actions.push(BridgeAction::RefreshZero { tuple, alloc_id });
                        if !sess.reader_dead {
                            actions.push(BridgeAction::KillStreamReader { alloc_id });
                        }
                        if sess.wire_allow_peer > 0 && sess.wire_allow_peer < 2 {
                            actions.push(BridgeAction::DuplicateAllowPeer { alloc_id });
                            actions.push(BridgeAction::DropAllowPeer { alloc_id });
                        }
                        if sess.wire_peer_allowed > 0 && sess.wire_peer_allowed < 2 {
                            actions.push(BridgeAction::DuplicatePeerAllowed { alloc_id });
                            actions.push(BridgeAction::DropPeerAllowed { alloc_id });
                        }
                    }
                }
                actions.push(BridgeAction::TcpDisconnect { tuple });
            }

            for open in state.in_flight_opens.values() {
                if !state.bridge.live_allocations().contains_key(&open.tuple) {
                    actions.push(BridgeAction::TcpDisconnect { tuple: open.tuple });
                }
            }

            actions.push(BridgeAction::BeginShutdown);
        }

        if usize::from(state.clock_epoch) + 1 < CLOCK_EPOCHS_MS.len() {
            actions.push(BridgeAction::AdvanceClock);
        }
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut s = last_state.clone();
        s.last_witness = None;
        let now_ms = s.now_ms();

        let consumes_budget = !matches!(
            action,
            BridgeAction::CompleteOpenOk { .. }
                | BridgeAction::CompleteOpenErr { .. }
                | BridgeAction::DeliverAllowPeer { .. }
                | BridgeAction::DeliverPeerAllowed { .. }
                | BridgeAction::CompleteRelayClose { .. }
        );
        if consumes_budget {
            s.budget_used = s.budget_used.saturating_add(1);
        }

        match action {
            BridgeAction::IssueTicket { ticket_idx } => {
                s.tickets_issued_mask |= 1 << ticket_idx;
                let _ =
                    s.bridge
                        .reserve_ticket(ticket_bytes(ticket_idx), RESERVATION_TTL_MS, now_ms);
            }
            BridgeAction::SendAllocate {
                tuple,
                ticket_idx,
                tx_id,
            } => {
                let username = ticket_username(ticket_idx);
                let ticket_id = ticket_bytes(ticket_idx);

                let expected_owner = s
                    .bridge
                    .expected_owner_for_allocate(&tuple, &tx_id, now_ms)
                    .map(str::to_owned);
                let ticket_unexpired_at_auth = s.clock_epoch == 0;
                let auth_ok = match expected_owner.as_deref() {
                    Some(owner) => owner == username,
                    None => ticket_unexpired_at_auth,
                };
                if !auth_ok {
                    self.record_final_response(
                        &mut s,
                        tuple,
                        ticket_idx,
                        tx_id,
                        AllocateResp::Err401Unauthorized,
                        None,
                    );
                    self.check_step_invariants(&mut s);
                    return Some(s);
                }

                if self.mutant == Some(Mutant::TicketReusableAcrossTuples) {
                    let _ = s
                        .bridge
                        .reserve_ticket(ticket_id, RESERVATION_TTL_MS, now_ms);
                }

                if self.mutant == Some(Mutant::PendingAllocateNotRegisteredBeforeYield)
                    && !s.in_flight_opens.values().any(|o| o.tuple == tuple)
                    && !s.bridge.live_allocations().contains_key(&tuple)
                {
                    let (expired_ids, start) = s.bridge.begin_allocate(
                        tuple,
                        username.clone(),
                        tx_id,
                        Some(ticket_id),
                        ALLOCATION_LIFETIME_MS,
                        now_ms,
                    );
                    for exp_id in expired_ids {
                        Self::mark_session_closing(&mut s, exp_id);
                    }
                    if let BridgeAllocateStart::StartBackendOpen { open_id } = start {
                        let _ = s.bridge.complete_allocate_err(tuple, open_id, None, now_ms);
                        s.ticket_claimed_tuples
                            .entry(ticket_idx)
                            .or_default()
                            .insert(tuple);
                        s.retransmit_windows.insert(
                            (tuple, ticket_idx, tx_id),
                            RetransmitWindow::Active {
                                final_resp: None,
                                expires_at_ms: u128::MAX,
                            },
                        );
                        s.in_flight_opens.insert(open_id, InFlightBackendOpen {
                            tuple,
                            ticket_idx,
                            tx_id,
                            open_id,
                        });
                        self.check_step_invariants(&mut s);
                        return Some(s);
                    }
                }

                if self.mutant == Some(Mutant::NonAtomicCapacityCheck)
                    && s.bridge.live_allocation_len() < MAX_ALLOCATIONS
                    && s.bridge.used_allocation_slots() >= MAX_ALLOCATIONS
                    && !s.in_flight_opens.values().any(|o| o.tuple == tuple)
                {
                    let extra_open_id = 900_u64.saturating_add(u64::from(s.budget_used));
                    s.ticket_claimed_tuples
                        .entry(ticket_idx)
                        .or_default()
                        .insert(tuple);
                    s.in_flight_opens
                        .insert(extra_open_id, InFlightBackendOpen {
                            tuple,
                            ticket_idx,
                            tx_id,
                            open_id: extra_open_id,
                        });
                    self.check_step_invariants(&mut s);
                    return Some(s);
                }

                let (expired_ids, decision) = s.bridge.begin_allocate(
                    tuple,
                    username.clone(),
                    tx_id,
                    Some(ticket_id),
                    ALLOCATION_LIFETIME_MS,
                    now_ms,
                );
                for exp_id in expired_ids {
                    Self::mark_session_closing(&mut s, exp_id);
                }

                match decision {
                    BridgeAllocateStart::ShuttingDown => {}
                    BridgeAllocateStart::ReplayCached(resp) => {
                        s.last_witness = Some(StepWitness::ReplayCached);
                        self.record_final_response(&mut s, tuple, ticket_idx, tx_id, resp, None);
                    }
                    BridgeAllocateStart::CoalescedInFlight { .. } => {
                        s.last_witness = Some(StepWitness::CoalescedInFlight);
                    }
                    BridgeAllocateStart::AllocationMismatch => {
                        self.record_final_response(
                            &mut s,
                            tuple,
                            ticket_idx,
                            tx_id,
                            AllocateResp::Err437AllocationMismatch,
                            None,
                        );
                    }
                    BridgeAllocateStart::TicketUnknownOrConsumed => {
                        self.record_final_response(
                            &mut s,
                            tuple,
                            ticket_idx,
                            tx_id,
                            AllocateResp::Err401Unauthorized,
                            None,
                        );
                    }
                    BridgeAllocateStart::CapacityReached {
                        cache_for_retransmit,
                    } => {
                        let exp = now_ms.saturating_add(BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS);
                        if cache_for_retransmit {
                            s.ticket_claimed_tuples
                                .entry(ticket_idx)
                                .or_default()
                                .insert(tuple);
                            s.retransmit_windows.insert(
                                (tuple, ticket_idx, tx_id),
                                RetransmitWindow::Active {
                                    final_resp: Some(AllocateResp::Err508InsufficientCapacity),
                                    expires_at_ms: exp,
                                },
                            );
                            s.bridge.cache_allocate_response(
                                tuple,
                                username,
                                tx_id,
                                AllocateResp::Err508InsufficientCapacity,
                                exp,
                                now_ms,
                            );
                        }
                        self.record_final_response(
                            &mut s,
                            tuple,
                            ticket_idx,
                            tx_id,
                            AllocateResp::Err508InsufficientCapacity,
                            Some(exp),
                        );
                    }
                    BridgeAllocateStart::StartBackendOpen { open_id } => {
                        s.ticket_claimed_tuples
                            .entry(ticket_idx)
                            .or_default()
                            .insert(tuple);
                        s.retransmit_windows.insert(
                            (tuple, ticket_idx, tx_id),
                            RetransmitWindow::Active {
                                final_resp: None,
                                expires_at_ms: u128::MAX,
                            },
                        );
                        s.in_flight_opens.insert(open_id, InFlightBackendOpen {
                            tuple,
                            ticket_idx,
                            tx_id,
                            open_id,
                        });
                    }
                }
            }
            BridgeAction::CompleteOpenOk { open_id } => {
                let open = s.in_flight_opens.remove(&open_id)?;
                let e_exit = exit_addr_for_open(open_id);
                let advertised_addr =
                    if self.mutant == Some(Mutant::RelayedAddressUsesEntryLocalAddr) {
                        ENTRY_LOCAL_ADDR
                    } else {
                        e_exit
                    };

                s.exit_sessions.insert(open_id, ExitSessionState {
                    bound_addr: e_exit,
                    pinholes: OnionUdpPinholes::new(
                        DEFAULT_UDP_PINHOLE_TTL_MS,
                        DEFAULT_MAX_UDP_PINHOLES,
                    ),
                    wire_allow_peer: 0,
                    wire_peer_allowed: 0,
                    awaiting_perm_commit: false,
                    reader_dead: false,
                    closing: false,
                });

                if self.mutant == Some(Mutant::InstallAfterShutdown) && s.bridge.is_shutting_down()
                {
                    s.law4_install_after_shutdown = true;
                    return Some(s);
                }

                let ok_resp = AllocateResp::Ok {
                    alloc_id: open_id,
                    relayed_addr: advertised_addr,
                };
                match s.bridge.complete_allocate_ok(
                    open.tuple,
                    open_id,
                    advertised_addr,
                    ok_resp,
                    now_ms,
                ) {
                    BridgeCompleteAllocate::Installed { alloc_id } => {
                        if advertised_addr != e_exit {
                            s.law2_violation = true;
                        }
                        debug_assert_eq!(alloc_id, open_id);
                        self.record_final_response(
                            &mut s,
                            open.tuple,
                            open.ticket_idx,
                            open.tx_id,
                            ok_resp,
                            Some(now_ms.saturating_add(ALLOCATION_LIFETIME_MS)),
                        );
                    }
                    BridgeCompleteAllocate::AbortAndCloseRelay { closing_id } => {
                        Self::mark_session_closing(&mut s, closing_id);
                    }
                }
            }
            BridgeAction::CompleteOpenErr { open_id } => {
                let open = s.in_flight_opens.remove(&open_id)?;
                let err_resp = AllocateResp::Err500BackendFailed;
                let exp = now_ms.saturating_add(BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS);
                let should_reply = s.bridge.complete_allocate_err(
                    open.tuple,
                    open_id,
                    Some((err_resp, BRIDGE_FAILED_ALLOCATE_CACHE_TTL_MS)),
                    now_ms,
                );
                if should_reply {
                    self.record_final_response(
                        &mut s,
                        open.tuple,
                        open.ticket_idx,
                        open.tx_id,
                        err_resp,
                        Some(exp),
                    );
                }
            }
            BridgeAction::BeginCreatePermission { tuple } => {
                let alloc_id = s.bridge.live_allocations().get(&tuple)?.alloc_id;
                let check = s.bridge.can_install_permissions(
                    &tuple,
                    alloc_id,
                    &[PEER_ADDR.ip()],
                    MAX_PERMISSIONS,
                    now_ms,
                );
                if check == BridgePermissionCheck::Permitted {
                    if let Some(sess) = s.exit_sessions.get_mut(&alloc_id) {
                        if !sess.closing {
                            sess.awaiting_perm_commit = true;
                            sess.wire_allow_peer = sess.wire_allow_peer.saturating_add(1);
                        }
                    }
                }
            }
            BridgeAction::DuplicateAllowPeer { alloc_id } => {
                let sess = s.exit_sessions.get_mut(&alloc_id)?;
                sess.wire_allow_peer = sess.wire_allow_peer.saturating_add(1);
            }
            BridgeAction::DropAllowPeer { alloc_id } => {
                let sess = s.exit_sessions.get_mut(&alloc_id)?;
                sess.wire_allow_peer = sess.wire_allow_peer.saturating_sub(1);
                if sess.wire_allow_peer == 0 && sess.wire_peer_allowed == 0 {
                    sess.awaiting_perm_commit = false;
                }
            }
            BridgeAction::DeliverAllowPeer { alloc_id } => {
                let sess = s.exit_sessions.get_mut(&alloc_id)?;
                sess.wire_allow_peer = sess.wire_allow_peer.saturating_sub(1);
                if !sess.closing {
                    sess.pinholes.allow_ip(PEER_ADDR.ip(), now_ms);
                    sess.wire_peer_allowed = sess.wire_peer_allowed.saturating_add(1).min(2);
                }
            }
            BridgeAction::DuplicatePeerAllowed { alloc_id } => {
                let sess = s.exit_sessions.get_mut(&alloc_id)?;
                sess.wire_peer_allowed = sess.wire_peer_allowed.saturating_add(1);
            }
            BridgeAction::DropPeerAllowed { alloc_id } => {
                let sess = s.exit_sessions.get_mut(&alloc_id)?;
                sess.wire_peer_allowed = sess.wire_peer_allowed.saturating_sub(1);
                if sess.wire_allow_peer == 0 && sess.wire_peer_allowed == 0 {
                    sess.awaiting_perm_commit = false;
                }
            }
            BridgeAction::DeliverPeerAllowed { alloc_id } => {
                let sess = s.exit_sessions.get_mut(&alloc_id)?;
                sess.wire_peer_allowed = sess.wire_peer_allowed.saturating_sub(1);
                if sess.awaiting_perm_commit && !sess.closing {
                    sess.awaiting_perm_commit = false;
                    let owner_tuple = s
                        .bridge
                        .live_allocations()
                        .iter()
                        .find_map(|(&k, a)| (a.alloc_id == alloc_id).then_some(k));
                    if let Some(tuple) = owner_tuple {
                        let _ = s.bridge.commit_permissions(
                            &tuple,
                            alloc_id,
                            &[PEER_ADDR.ip()],
                            BRIDGE_PERMISSION_TTL_MS,
                            MAX_PERMISSIONS,
                            now_ms,
                        );
                    }
                }
            }
            BridgeAction::ProbeInboundPeerDatagram { tuple, alloc_id } => {
                let exit_admits = s.exit_sessions.get_mut(&alloc_id).is_some_and(|sess| {
                    !sess.closing && sess.pinholes.admits(PEER_ADDR.ip(), now_ms)
                });
                let entry_admits = s
                    .bridge
                    .inbound_forwarding_channel(&tuple, alloc_id, PEER_ADDR, now_ms)
                    .is_some();

                let delivered_to_client =
                    if self.mutant == Some(Mutant::BypassEntryPermissionOnInbound) {
                        exit_admits
                    } else {
                        exit_admits && entry_admits
                    };
                let expected_gate = exit_admits && entry_admits;
                if delivered_to_client != expected_gate {
                    s.law3_violation = true;
                }
                if delivered_to_client {
                    s.last_witness = Some(StepWitness::EndToEndDatagram);
                } else if exit_admits && !entry_admits {
                    s.last_witness = Some(StepWitness::PermissionExpiredDrop);
                }
            }
            BridgeAction::RefreshExtend { tuple, alloc_id } => {
                match s
                    .bridge
                    .refresh(&tuple, alloc_id, ALLOCATION_LIFETIME_MS, now_ms)
                {
                    BridgeRefreshOutcome::Expired { close_alloc_id } => {
                        Self::retire_tuple_windows(&mut s, tuple);
                        Self::mark_session_closing(&mut s, close_alloc_id);
                    }
                    BridgeRefreshOutcome::Refreshed { expires_at_ms } => {
                        for ((k_tuple, _, _), window) in &mut s.retransmit_windows {
                            if *k_tuple == tuple {
                                if let RetransmitWindow::Active {
                                    expires_at_ms: win_exp,
                                    ..
                                } = window
                                {
                                    *win_exp = expires_at_ms;
                                }
                            }
                        }
                    }
                    BridgeRefreshOutcome::NotFound | BridgeRefreshOutcome::Deleted { .. } => {}
                }
            }
            BridgeAction::RefreshZero { tuple, alloc_id } => {
                Self::retire_tuple_windows(&mut s, tuple);
                match s.bridge.refresh(&tuple, alloc_id, 0, now_ms) {
                    BridgeRefreshOutcome::Deleted { close_alloc_id }
                    | BridgeRefreshOutcome::Expired { close_alloc_id } => {
                        Self::mark_session_closing(&mut s, close_alloc_id);
                    }
                    BridgeRefreshOutcome::NotFound | BridgeRefreshOutcome::Refreshed { .. } => {}
                }
            }
            BridgeAction::TcpDisconnect { tuple } => {
                Self::retire_tuple_windows(&mut s, tuple);
                let outcome = s.bridge.disconnect_tuple(&tuple);
                if let Some(id) = outcome.closed_alloc_id {
                    Self::mark_session_closing(&mut s, id);
                }
            }
            BridgeAction::KillStreamReader { alloc_id } => {
                let sess = s.exit_sessions.get_mut(&alloc_id)?;
                sess.reader_dead = true;
            }
            BridgeAction::CompleteRelayClose { id } => {
                let had_dead_reader = s
                    .exit_sessions
                    .get(&id)
                    .is_some_and(|sess| sess.reader_dead);
                s.exit_sessions.remove(&id);
                let _ = s.bridge.finish_teardown(id);
                if had_dead_reader {
                    s.last_witness = Some(StepWitness::DeadReaderTeardown);
                }
            }
            BridgeAction::AdvanceClock => {
                s.clock_epoch = s.clock_epoch.saturating_add(1);
                let new_now = s.now_ms();
                for window in s.retransmit_windows.values_mut() {
                    if let RetransmitWindow::Active {
                        final_resp: Some(_),
                        expires_at_ms,
                    } = window
                    {
                        if new_now > *expires_at_ms {
                            *window = RetransmitWindow::Retired;
                        }
                    }
                }
                s.bridge.prune_expired_tickets(new_now);
                let expired = s.bridge.sweep_expired_allocations(new_now);
                for id in expired {
                    Self::mark_session_closing(&mut s, id);
                }
                for sess in s.exit_sessions.values_mut() {
                    sess.pinholes.prune_expired(new_now);
                }
            }
            BridgeAction::BeginShutdown => {
                for window in s.retransmit_windows.values_mut() {
                    *window = RetransmitWindow::Retired;
                }
                let live_ids = s.bridge.begin_shutdown();
                for id in live_ids {
                    Self::mark_session_closing(&mut s, id);
                }
            }
        }

        self.check_step_invariants(&mut s);
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "Law 1: single-binding ticket & idempotent Allocate retransmission (never 401)",
                |_, s| !s.law1_violation,
            ),
            Property::<Self>::always(
                "Law 2: XOR-RELAYED-ADDRESS equals E_exit of opened onion::udp session",
                |_, s| !s.law2_violation,
            ),
            Property::<Self>::always(
                "Law 3: end-to-end permission & exit pinhole gate",
                |_, s| !s.law3_violation,
            ),
            Property::<Self>::always(
                "Law 4: bounded non-blocking teardown, zero orphan, no install after close()",
                |model, s| !s.law4_install_after_shutdown && !model.has_stuck_teardown(s),
            ),
            Property::<Self>::always(
                "Law 6: atomic reservation & allocation capacity bounds",
                |_, s| !s.law6_violation,
            ),
            Property::<Self>::sometimes("witness: coalesced in-flight Allocate", |_, s| {
                s.last_witness == Some(StepWitness::CoalescedInFlight)
            }),
            Property::<Self>::sometimes("witness: cached Allocate replay", |_, s| {
                s.last_witness == Some(StepWitness::ReplayCached)
            }),
            Property::<Self>::sometimes(
                "witness: end-to-end permitted peer datagram reaches client",
                |_, s| s.last_witness == Some(StepWitness::EndToEndDatagram),
            ),
            Property::<Self>::sometimes(
                "witness: datagram dropped after 300s RFC 5766 permission expiry while 600s pinhole open",
                |_, s| s.last_witness == Some(StepWitness::PermissionExpiredDrop),
            ),
            Property::<Self>::sometimes(
                "witness: non-blocking teardown completes despite dead stream reader",
                |_, s| s.last_witness == Some(StepWitness::DeadReaderTeardown),
            ),
        ]
    }
}

fn assert_mutant_refuted(mutant: Mutant, expected_law_property: &'static str) {
    let checker = BridgeModel {
        mutant: Some(mutant),
    }
    .checker()
    .threads(1)
    .spawn_bfs()
    .join();
    assert!(
        checker.discovery(expected_law_property).is_some(),
        "expected Stateright BFS to refute {mutant:?} on `{expected_law_property}`"
    );
}

#[test]
fn stateright_turn_onion_udp_bridge_laws_1_2_3_4_6_hold() {
    let checker = BridgeModel::default()
        .checker()
        .threads(1)
        .spawn_bfs()
        .join();
    println!(
        "BridgeModel exhaustive BFS explored {} unique states",
        checker.unique_state_count()
    );
    assert!(
        checker.unique_state_count() >= 500,
        "expected exhaustive bridge BFS to explore >= 500 states, got {}",
        checker.unique_state_count()
    );
    checker.assert_properties();
}

#[test]
fn stateright_refutes_mutant_pending_allocate_not_registered_before_yield_law_1() {
    assert_mutant_refuted(
        Mutant::PendingAllocateNotRegisteredBeforeYield,
        "Law 1: single-binding ticket & idempotent Allocate retransmission (never 401)",
    );
}

#[test]
fn stateright_refutes_mutant_ticket_reusable_across_tuples_law_1() {
    assert_mutant_refuted(
        Mutant::TicketReusableAcrossTuples,
        "Law 1: single-binding ticket & idempotent Allocate retransmission (never 401)",
    );
}

#[test]
fn stateright_refutes_mutant_relayed_address_uses_entry_local_addr_law_2() {
    assert_mutant_refuted(
        Mutant::RelayedAddressUsesEntryLocalAddr,
        "Law 2: XOR-RELAYED-ADDRESS equals E_exit of opened onion::udp session",
    );
}

#[test]
fn stateright_refutes_mutant_bypass_entry_permission_on_inbound_law_3() {
    assert_mutant_refuted(
        Mutant::BypassEntryPermissionOnInbound,
        "Law 3: end-to-end permission & exit pinhole gate",
    );
}

#[test]
fn stateright_refutes_mutant_teardown_waits_on_dead_reader_law_4() {
    assert_mutant_refuted(
        Mutant::TeardownWaitsOnDeadReader,
        "Law 4: bounded non-blocking teardown, zero orphan, no install after close()",
    );
}

#[test]
fn stateright_refutes_mutant_install_after_shutdown_law_4() {
    assert_mutant_refuted(
        Mutant::InstallAfterShutdown,
        "Law 4: bounded non-blocking teardown, zero orphan, no install after close()",
    );
}

#[test]
fn stateright_refutes_mutant_non_atomic_capacity_check_law_6() {
    assert_mutant_refuted(
        Mutant::NonAtomicCapacityCheck,
        "Law 6: atomic reservation & allocation capacity bounds",
    );
}
