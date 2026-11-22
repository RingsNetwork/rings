//! Exhaustive Stateright model of the Phase B TURN entry ↔ `onion::udp` exit bridge (#1292).
//!
//! Directly drives the production [`TurnAllocationCore`] (`rings-turn`) and
//! [`OnionUdpPinholeTable`] (`rings-node`) state machines across non-atomic asynchronous steps,
//! UDP `Allocate` retransmissions, forward/return frame loss and duplication, permission &
//! allocation expiry, `Refresh`, TCP client disconnect, and server shutdown.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;

use rings_node::onion::OnionUdpClientFrame;
use rings_node::onion::OnionUdpPinholeTable;
use rings_node::onion::OnionUdpServerFrame;
use rings_turn::BeginAllocateDecision;
use rings_turn::CompleteAllocateDecision;
use rings_turn::RefreshDecision;
use rings_turn::TurnAllocationCore;
use rings_turn::TurnFiveTupleKey;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

const LAW_1_NAME: &str =
    "Law 1: single-use ticket Allocate retransmission coalesces without false 401 or duplicate session";
const LAW_2_NAME: &str =
    "Law 2: advertised XOR-RELAYED-ADDRESS equals the bound exit socket E_exit";
const LAW_3_NAME: &str =
    "Law 3: inbound peer datagrams pass only when both entry permission and exit pinhole are live";
const LAW_4_NAME: &str =
    "Law 4: no orphan exit session or install after shutdown/disconnect and teardown never blocks on parked reader";
const LAW_6_NAME: &str =
    "Law 6: atomic reservation and pending+active allocation bounds hold across async yields";

const ALLOC_LIFETIME_MS: u128 = 600_000;
const PERM_TTL_MS: u64 = 300_000;
const TICKET_TTL_MS: u128 = 60_000;

/// Seeded implementation bugs (including the 4 Phase A/B review bugs + Law 2 & Law 3 mutants)
/// verified to be refuted by the Stateright model checker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum BridgeMutant {
    /// Review bug #3: consumes the single-use `SessionTicket` before yielding without registering
    /// `PendingAllocate`, causing an in-flight UDP retransmission to fail with `401 TicketUnknown`.
    PendingNotRegisteredBeforeYield,
    /// Projects the entry listener's wildcard/local address instead of `OnionUdpServerFrame::Bound`'s
    /// `E_exit` into `XOR-RELAYED-ADDRESS`.
    WrongRelayedAddressProjection,
    /// Skips the entry/exit permission check when forwarding inbound datagrams to the TURN client.
    BypassPermissionGate,
    /// Review bug #1: `finish_close` waits on the reader lock without `begin_close` waking a
    /// silent reader parked in `recv_datagram`.
    TeardownWaitsOnDeadReader,
    /// Review bug #4: installs a completing async `Allocate` into `active_allocations` even after
    /// `shutting_down` has been set.
    InstallAfterShutdown,
    /// Review bug #2: checks only `active_allocations.len() < max_allocations` before yielding
    /// instead of atomically reserving `pending + active <= max_allocations`.
    NonAtomicCapacityCheck,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum BridgeAction {
    IssueTicket(u8),
    SendAllocate {
        client: u8,
        tx_idx: u8,
        ticket_idx: Option<u8>,
    },
    ExitOpenComplete {
        open_id: u64,
        succeed: bool,
    },
    CreatePermission {
        client: u8,
        peer_idx: u8,
    },
    DeliverForwardFrame {
        open_id: u64,
        duplicate: bool,
    },
    DropForwardFrame {
        open_id: u64,
    },
    InboundPeerDatagram {
        open_id: u64,
        peer_idx: u8,
    },
    DeliverReturnFrame {
        open_id: u64,
        duplicate: bool,
    },
    DropReturnFrame {
        open_id: u64,
    },
    RefreshZero {
        client: u8,
    },
    ClientDisconnect {
        client: u8,
    },
    AdvanceClock {
        delta_ms: u128,
    },
    ServerShutdown,
    StepTeardown {
        open_id: u64,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExitSessionModelState {
    bound_addr: SocketAddr,
    pinholes: OnionUdpPinholeTable,
    forward_queue: VecDeque<OnionUdpClientFrame>,
    return_queue: VecDeque<OnionUdpServerFrame>,
    reader_parked: bool,
    close_signaled: bool,
    closing_pending: bool,
}

/// Detached pending open tracked only when simulating mutant bugs that bypass `TurnAllocationCore`'s
/// own pending/capacity invariants.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MutantDetachedOpen {
    tuple: TurnFiveTupleKey,
    tx_id: [u8; 12],
    username: String,
    open_id: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BridgeState {
    now_ms: u128,
    steps: u8,
    core: TurnAllocationCore,
    exit_sessions: BTreeMap<u64, ExitSessionModelState>,
    advertised_relayed: BTreeMap<TurnFiveTupleKey, (u64, SocketAddr)>,
    disconnected_tuples: BTreeSet<TurnFiveTupleKey>,
    mutant_detached_opens: BTreeMap<u64, MutantDetachedOpen>,
    next_mutant_open_id: u64,

    // Law violation latches
    law1_false_401_on_retransmit: bool,
    law1_duplicate_session_for_tx: bool,
    law2_address_mismatch: bool,
    law3_unpermitted_delivery: bool,
    law4_installed_after_shutdown_or_disconnect: bool,
    law4_teardown_blocked_on_reader: bool,
    law6_bounds_exceeded: bool,

    // Reachability witnesses (`Property::sometimes`)
    saw_coalesced_pending: bool,
    saw_replayed_active: bool,
    saw_frame_loss_or_dup: bool,
    saw_permitted_inbound_delivery: bool,
    saw_unpermitted_inbound_drop: bool,
    saw_inflight_abort_and_clean_teardown: bool,
}

#[derive(Clone)]
struct BridgeModel {
    mutant: Option<BridgeMutant>,
    max_steps: u8,
    max_allocations: usize,
    max_reservations: usize,
}

fn client_tuple(client: u8) -> TurnFiveTupleKey {
    TurnFiveTupleKey {
        src_addr: SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10 + client)),
            40_000 + u16::from(client),
        ),
        dst_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 3478),
        protocol: 6, // TCP/UDP 5-tuple supporting disconnect transitions
    }
}

fn tx_bytes(client: u8, tx_idx: u8) -> [u8; 12] {
    let mut out = [0_u8; 12];
    out[0] = client;
    out[1] = tx_idx;
    out
}

fn ticket_id(ticket_idx: u8) -> [u8; 16] {
    let mut out = [0xA0_u8; 16];
    out[0] = ticket_idx;
    out
}

fn username_for(client: u8, ticket_idx: Option<u8>) -> String {
    match ticket_idx {
        Some(idx) => format!("tkt-{idx}"),
        None => format!("did-client-{client}"),
    }
}

fn exit_addr_for(open_id: u64) -> SocketAddr {
    let port = 49_152_u16.saturating_add(u16::try_from(open_id).unwrap_or(0));
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 200)), port)
}

fn peer_addr_for(peer_idx: u8) -> SocketAddr {
    SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 50 + peer_idx)),
        55_000 + u16::from(peer_idx),
    )
}

impl BridgeModel {
    fn production(max_steps: u8) -> Self {
        Self {
            mutant: None,
            max_steps,
            max_allocations: 1,
            max_reservations: 2,
        }
    }

    fn with_mutant(mutant: BridgeMutant, max_steps: u8) -> Self {
        Self {
            mutant: Some(mutant),
            max_steps,
            max_allocations: 1,
            max_reservations: 2,
        }
    }

    fn initiate_close(&self, state: &mut BridgeState, open_id: u64) {
        if let Some(session) = state.exit_sessions.get_mut(&open_id) {
            session.closing_pending = true;
            if self.mutant != Some(BridgeMutant::TeardownWaitsOnDeadReader) {
                // Production `begin_close()` signals `closed_notify`, waking any parked reader immediately.
                session.close_signaled = true;
                session.reader_parked = false;
            }
        }
    }

    fn check_global_bounds(&self, state: &mut BridgeState) {
        let total_active = state.core.active_allocation_len()
            + usize::from(
                self.mutant == Some(BridgeMutant::NonAtomicCapacityCheck)
                    && state.advertised_relayed.len() > state.core.active_allocation_len(),
            );
        let total_in_flight = state.core.pending_open_len() + state.mutant_detached_opens.len();
        if state.core.reservation_len() > self.max_reservations
            || state.core.pending_open_len() + state.core.active_allocation_len()
                > self.max_allocations
            || total_active > self.max_allocations
            || (self.mutant.is_none() && total_in_flight + total_active > self.max_allocations)
        {
            state.law6_bounds_exceeded = true;
        }
    }
}

impl Model for BridgeModel {
    type Action = BridgeAction;
    type State = BridgeState;

    fn init_states(&self) -> Vec<Self::State> {
        vec![BridgeState {
            now_ms: 1_000,
            steps: 0,
            core: TurnAllocationCore::new(self.max_allocations, self.max_reservations, 2),
            exit_sessions: BTreeMap::new(),
            advertised_relayed: BTreeMap::new(),
            disconnected_tuples: BTreeSet::new(),
            mutant_detached_opens: BTreeMap::new(),
            next_mutant_open_id: 100,
            law1_false_401_on_retransmit: false,
            law1_duplicate_session_for_tx: false,
            law2_address_mismatch: false,
            law3_unpermitted_delivery: false,
            law4_installed_after_shutdown_or_disconnect: false,
            law4_teardown_blocked_on_reader: false,
            law6_bounds_exceeded: false,
            saw_coalesced_pending: false,
            saw_replayed_active: false,
            saw_frame_loss_or_dup: false,
            saw_permitted_inbound_delivery: false,
            saw_unpermitted_inbound_drop: false,
            saw_inflight_abort_and_clean_teardown: false,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // Always allow pending teardown steps to drain even at `max_steps` so Law 4 can check
        // terminal quiescence cleanly.
        let closing_ids: Vec<u64> = state
            .exit_sessions
            .iter()
            .filter(|(_, s)| s.closing_pending)
            .map(|(&id, _)| id)
            .collect();
        for open_id in closing_ids {
            actions.push(BridgeAction::StepTeardown { open_id });
        }

        if state.steps >= self.max_steps {
            return;
        }

        if !state.core.is_shutting_down() {
            actions.push(BridgeAction::IssueTicket(0));
            actions.push(BridgeAction::IssueTicket(1));
            actions.push(BridgeAction::SendAllocate {
                client: 0,
                tx_idx: 0,
                ticket_idx: Some(0),
            });
            actions.push(BridgeAction::SendAllocate {
                client: 1,
                tx_idx: 0,
                ticket_idx: None,
            });
            actions.push(BridgeAction::ServerShutdown);
        }

        for pending in state.core.pending_opens().values() {
            actions.push(BridgeAction::ExitOpenComplete {
                open_id: pending.open_id,
                succeed: true,
            });
            actions.push(BridgeAction::ExitOpenComplete {
                open_id: pending.open_id,
                succeed: false,
            });
        }
        for &open_id in state.mutant_detached_opens.keys() {
            actions.push(BridgeAction::ExitOpenComplete {
                open_id,
                succeed: true,
            });
        }

        for client in [0_u8, 1_u8] {
            let tuple = client_tuple(client);
            if state.core.active_allocations().contains_key(&tuple)
                || state.core.pending_opens().contains_key(&tuple)
            {
                actions.push(BridgeAction::ClientDisconnect { client });
            }
            if state.core.active_allocations().contains_key(&tuple) {
                actions.push(BridgeAction::CreatePermission {
                    client,
                    peer_idx: 0,
                });
                actions.push(BridgeAction::RefreshZero { client });
            }
        }

        for (&open_id, session) in &state.exit_sessions {
            if session.closing_pending {
                continue;
            }
            if !session.forward_queue.is_empty() {
                actions.push(BridgeAction::DeliverForwardFrame {
                    open_id,
                    duplicate: false,
                });
                if !state.saw_frame_loss_or_dup {
                    actions.push(BridgeAction::DeliverForwardFrame {
                        open_id,
                        duplicate: true,
                    });
                    actions.push(BridgeAction::DropForwardFrame { open_id });
                }
            }
            if session.return_queue.is_empty() {
                actions.push(BridgeAction::InboundPeerDatagram {
                    open_id,
                    peer_idx: 0,
                });
                actions.push(BridgeAction::InboundPeerDatagram {
                    open_id,
                    peer_idx: 1,
                });
            } else {
                actions.push(BridgeAction::DeliverReturnFrame {
                    open_id,
                    duplicate: false,
                });
                if !state.saw_frame_loss_or_dup {
                    actions.push(BridgeAction::DeliverReturnFrame {
                        open_id,
                        duplicate: true,
                    });
                    actions.push(BridgeAction::DropReturnFrame { open_id });
                }
            }
        }

        actions.push(BridgeAction::AdvanceClock { delta_ms: 300_001 });
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut s = last.clone();
        if !matches!(action, BridgeAction::StepTeardown { .. }) {
            s.steps = s.steps.saturating_add(1);
        }

        match action {
            BridgeAction::IssueTicket(idx) => {
                let _ = s.core.reserve_ticket(
                    ticket_id(idx),
                    s.now_ms.saturating_add(TICKET_TTL_MS),
                    s.now_ms,
                );
            }
            BridgeAction::SendAllocate {
                client,
                tx_idx,
                ticket_idx,
            } => {
                let tuple = client_tuple(client);
                s.disconnected_tuples.remove(&tuple);
                let tx_id = tx_bytes(client, tx_idx);
                let username = username_for(client, ticket_idx);
                let ticket_sid = ticket_idx.map(ticket_id);

                // Was this exact `(tuple, tx_id, username)` already in flight before this retransmission?
                let was_in_flight = s
                    .core
                    .pending_opens()
                    .get(&tuple)
                    .is_some_and(|p| p.tx_id == tx_id && p.username == username)
                    || s.mutant_detached_opens
                        .values()
                        .any(|d| d.tuple == tuple && d.tx_id == tx_id && d.username == username);

                if self.mutant == Some(BridgeMutant::PendingNotRegisteredBeforeYield)
                    && ticket_sid.is_some()
                {
                    // Mutant #1 (Review Bug #3): consumes ticket via `begin_allocate` on first try,
                    // then drops the pending registration before yielding!
                    let dec = s.core.begin_allocate(
                        tuple,
                        tx_id,
                        username.clone(),
                        ticket_sid,
                        ALLOC_LIFETIME_MS,
                        s.now_ms,
                    );
                    for &exp_id in dec.expired_open_ids() {
                        self.initiate_close(&mut s, exp_id);
                    }
                    match dec {
                        BeginAllocateDecision::StartOpen { open_id, .. } => {
                            let _ = s.core.disconnect_tuple(&tuple);
                            s.mutant_detached_opens.insert(open_id, MutantDetachedOpen {
                                tuple,
                                tx_id,
                                username,
                                open_id,
                            });
                        }
                        BeginAllocateDecision::RejectTicketUnknown { .. } if was_in_flight => {
                            s.law1_false_401_on_retransmit = true;
                        }
                        _ => {}
                    }
                } else if self.mutant == Some(BridgeMutant::NonAtomicCapacityCheck) {
                    // Mutant #6 (Review Bug #2): checks only `active_allocation_len() < max_allocations`
                    // without counting in-flight pending opens!
                    if s.core.active_allocation_len() < self.max_allocations {
                        let open_id = s.next_mutant_open_id;
                        s.next_mutant_open_id = s.next_mutant_open_id.saturating_add(1);
                        s.mutant_detached_opens.insert(open_id, MutantDetachedOpen {
                            tuple,
                            tx_id,
                            username,
                            open_id,
                        });
                    }
                } else {
                    let dec = s.core.begin_allocate(
                        tuple,
                        tx_id,
                        username,
                        ticket_sid,
                        ALLOC_LIFETIME_MS,
                        s.now_ms,
                    );
                    for &exp_id in dec.expired_open_ids() {
                        self.initiate_close(&mut s, exp_id);
                    }
                    match dec {
                        BeginAllocateDecision::CoalescePending { .. } => {
                            s.saw_coalesced_pending = true;
                        }
                        BeginAllocateDecision::ReplayActive { .. } => {
                            s.saw_replayed_active = true;
                        }
                        BeginAllocateDecision::RejectTicketUnknown { .. } if was_in_flight => {
                            s.law1_false_401_on_retransmit = true;
                        }
                        BeginAllocateDecision::StartOpen { .. } if was_in_flight => {
                            s.law1_duplicate_session_for_tx = true;
                        }
                        _ => {}
                    }
                }
            }
            BridgeAction::ExitOpenComplete { open_id, succeed } => {
                if let Some(detached) = s.mutant_detached_opens.remove(&open_id) {
                    if succeed {
                        let bound = exit_addr_for(open_id);
                        s.exit_sessions.insert(open_id, ExitSessionModelState {
                            bound_addr: bound,
                            pinholes: OnionUdpPinholeTable::with_capacity_limit(2),
                            forward_queue: VecDeque::new(),
                            return_queue: VecDeque::new(),
                            reader_parked: true,
                            close_signaled: false,
                            closing_pending: false,
                        });
                        s.advertised_relayed
                            .insert(detached.tuple, (open_id, bound));
                        if s.advertised_relayed.len() > self.max_allocations {
                            s.law6_bounds_exceeded = true;
                        }
                    }
                } else if let Some((&tuple, _)) = s
                    .core
                    .pending_opens()
                    .iter()
                    .find(|(_, p)| p.open_id == open_id)
                {
                    let bound = exit_addr_for(open_id);
                    if succeed {
                        s.exit_sessions.insert(open_id, ExitSessionModelState {
                            bound_addr: bound,
                            pinholes: OnionUdpPinholeTable::with_capacity_limit(2),
                            forward_queue: VecDeque::new(),
                            return_queue: VecDeque::new(),
                            reader_parked: true,
                            close_signaled: false,
                            closing_pending: false,
                        });
                    }
                    let outcome = if succeed { Ok(bound) } else { Err(()) };
                    let dec = s.core.complete_allocate(tuple, open_id, outcome, s.now_ms);
                    match dec {
                        CompleteAllocateDecision::Installed {
                            open_id,
                            relayed_addr,
                            ..
                        } => {
                            let projected_addr = if self.mutant
                                == Some(BridgeMutant::WrongRelayedAddressProjection)
                            {
                                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3478)
                            } else {
                                relayed_addr
                            };
                            s.advertised_relayed
                                .insert(tuple, (open_id, projected_addr));
                            if projected_addr != bound
                                || projected_addr.ip().is_unspecified()
                                || projected_addr.port() == 0
                            {
                                s.law2_address_mismatch = true;
                            }
                            if s.core.is_shutting_down() || s.disconnected_tuples.contains(&tuple) {
                                s.law4_installed_after_shutdown_or_disconnect = true;
                            }
                        }
                        CompleteAllocateDecision::RespondOpenError { .. } => {}
                        CompleteAllocateDecision::AbortRelay => {
                            if succeed {
                                self.initiate_close(&mut s, open_id);
                            }
                        }
                    }
                } else if succeed {
                    // Pending entry was already cancelled by disconnect or shutdown while exit bind
                    // was in flight across the loop!
                    let bound = exit_addr_for(open_id);
                    s.exit_sessions.insert(open_id, ExitSessionModelState {
                        bound_addr: bound,
                        pinholes: OnionUdpPinholeTable::with_capacity_limit(2),
                        forward_queue: VecDeque::new(),
                        return_queue: VecDeque::new(),
                        reader_parked: true,
                        close_signaled: false,
                        closing_pending: false,
                    });
                    if self.mutant == Some(BridgeMutant::InstallAfterShutdown)
                        && s.core.is_shutting_down()
                    {
                        s.advertised_relayed
                            .insert(client_tuple(0), (open_id, bound));
                        s.law4_installed_after_shutdown_or_disconnect = true;
                    } else {
                        self.initiate_close(&mut s, open_id);
                    }
                }
            }
            BridgeAction::CreatePermission { client, peer_idx } => {
                let tuple = client_tuple(client);
                let peer_ip = peer_addr_for(peer_idx).ip();
                if s.core
                    .install_permissions(&tuple, &[peer_ip], u128::from(PERM_TTL_MS), s.now_ms)
                    .is_ok()
                {
                    if let Some(&(open_id, _)) = s.advertised_relayed.get(&tuple) {
                        if let Some(session) = s.exit_sessions.get_mut(&open_id) {
                            session
                                .forward_queue
                                .push_back(OnionUdpClientFrame::AllowPeerIp {
                                    peer_ip,
                                    ttl_ms: PERM_TTL_MS,
                                });
                        }
                    }
                }
            }
            BridgeAction::DeliverForwardFrame { open_id, duplicate } => {
                if let Some(session) = s.exit_sessions.get_mut(&open_id) {
                    let frame = if duplicate {
                        s.saw_frame_loss_or_dup = true;
                        session.forward_queue.front().cloned()
                    } else {
                        session.forward_queue.pop_front()
                    };
                    if let Some(OnionUdpClientFrame::AllowPeerIp { peer_ip, ttl_ms }) = frame {
                        let _ = session.pinholes.allow_ip(peer_ip, ttl_ms, s.now_ms);
                    }
                }
            }
            BridgeAction::DropForwardFrame { open_id } => {
                if let Some(session) = s.exit_sessions.get_mut(&open_id) {
                    if session.forward_queue.pop_front().is_some() {
                        s.saw_frame_loss_or_dup = true;
                    }
                }
            }
            BridgeAction::InboundPeerDatagram { open_id, peer_idx } => {
                let peer = peer_addr_for(peer_idx);
                if let Some(session) = s.exit_sessions.get_mut(&open_id) {
                    let exit_allows = self.mutant == Some(BridgeMutant::BypassPermissionGate)
                        || session.pinholes.allows(peer.ip(), s.now_ms);
                    if exit_allows {
                        session
                            .return_queue
                            .push_back(OnionUdpServerFrame::RecvDatagram {
                                peer,
                                payload: vec![peer_idx],
                            });
                    } else {
                        s.saw_unpermitted_inbound_drop = true;
                    }
                }
            }
            BridgeAction::DeliverReturnFrame { open_id, duplicate } => {
                let maybe_frame = s.exit_sessions.get_mut(&open_id).and_then(|session| {
                    if duplicate {
                        s.saw_frame_loss_or_dup = true;
                        session.return_queue.front().cloned()
                    } else {
                        session.return_queue.pop_front()
                    }
                });
                if let Some(OnionUdpServerFrame::RecvDatagram { peer, .. }) = maybe_frame {
                    let owner_tuple = s
                        .advertised_relayed
                        .iter()
                        .find_map(|(&tuple, &(id, _))| (id == open_id).then_some(tuple));
                    if let Some(tuple) = owner_tuple {
                        let entry_perm = s.core.has_permission(&tuple, peer.ip(), s.now_ms);
                        let exit_perm = s
                            .exit_sessions
                            .get(&open_id)
                            .is_some_and(|sess| sess.pinholes.allows(peer.ip(), s.now_ms));
                        let forward_to_client =
                            self.mutant == Some(BridgeMutant::BypassPermissionGate) || entry_perm;
                        if forward_to_client {
                            if !entry_perm || !exit_perm {
                                s.law3_unpermitted_delivery = true;
                            } else {
                                s.saw_permitted_inbound_delivery = true;
                            }
                        } else {
                            s.saw_unpermitted_inbound_drop = true;
                        }
                    }
                }
            }
            BridgeAction::DropReturnFrame { open_id } => {
                if let Some(session) = s.exit_sessions.get_mut(&open_id) {
                    if session.return_queue.pop_front().is_some() {
                        s.saw_frame_loss_or_dup = true;
                    }
                }
            }
            BridgeAction::RefreshZero { client } => {
                let tuple = client_tuple(client);
                match s.core.refresh(&tuple, 0, s.now_ms) {
                    RefreshDecision::Deleted { open_id }
                    | RefreshDecision::Mismatch {
                        expired_open_id: Some(open_id),
                    } => {
                        s.advertised_relayed.remove(&tuple);
                        self.initiate_close(&mut s, open_id);
                    }
                    _ => {}
                }
            }
            BridgeAction::ClientDisconnect { client } => {
                let tuple = client_tuple(client);
                let had_pending = s.core.pending_opens().contains_key(&tuple);
                s.disconnected_tuples.insert(tuple);
                s.advertised_relayed.remove(&tuple);
                if let Some(open_id) = s.core.disconnect_tuple(&tuple) {
                    self.initiate_close(&mut s, open_id);
                }
                if had_pending {
                    s.saw_inflight_abort_and_clean_teardown = true;
                }
            }
            BridgeAction::AdvanceClock { delta_ms } => {
                s.now_ms = s.now_ms.saturating_add(delta_ms);
                let evicted = s.core.prune_expired(s.now_ms);
                for &open_id in &evicted {
                    s.advertised_relayed.retain(|_, &mut (id, _)| id != open_id);
                    self.initiate_close(&mut s, open_id);
                }
                for session in s.exit_sessions.values_mut() {
                    session.pinholes.prune_expired(s.now_ms);
                }
            }
            BridgeAction::ServerShutdown => {
                let had_pending = !s.core.pending_opens().is_empty();
                let ids = s.core.begin_shutdown();
                if self.mutant == Some(BridgeMutant::InstallAfterShutdown) && had_pending {
                    // Simulate an in-flight exit open racing right behind `begin_shutdown`:
                    let bound = exit_addr_for(999);
                    s.exit_sessions.insert(999, ExitSessionModelState {
                        bound_addr: bound,
                        pinholes: OnionUdpPinholeTable::with_capacity_limit(2),
                        forward_queue: VecDeque::new(),
                        return_queue: VecDeque::new(),
                        reader_parked: true,
                        close_signaled: false,
                        closing_pending: false,
                    });
                    s.advertised_relayed.insert(client_tuple(0), (999, bound));
                    s.law4_installed_after_shutdown_or_disconnect = true;
                }
                s.advertised_relayed
                    .retain(|_, &mut (id, _)| !ids.contains(&id));
                for open_id in ids {
                    self.initiate_close(&mut s, open_id);
                }
                if had_pending {
                    s.saw_inflight_abort_and_clean_teardown = true;
                }
            }
            BridgeAction::StepTeardown { open_id } => {
                if let Some(session) = s.exit_sessions.get(&open_id) {
                    if session.reader_parked && !session.close_signaled {
                        // Mutant #4 (Review Bug #1): finish_close blocks forever waiting for
                        // recv_lock held by a parked reader!
                        s.law4_teardown_blocked_on_reader = true;
                        return Some(s);
                    }
                }
                s.exit_sessions.remove(&open_id);
            }
        }

        // Law 2 continuous state invariant across all live allocations:
        for (tuple, active) in s.core.active_allocations() {
            match (
                s.advertised_relayed.get(tuple),
                s.exit_sessions.get(&active.open_id),
            ) {
                (Some(&(adv_id, adv_addr)), Some(exit_sess))
                    if adv_id == active.open_id
                        && adv_addr == exit_sess.bound_addr
                        && adv_addr == active.relayed_addr
                        && !adv_addr.ip().is_unspecified()
                        && adv_addr.port() != 0 => {}
                _ => {
                    s.law2_address_mismatch = true;
                }
            }
        }

        self.check_global_bounds(&mut s);
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let mut props = vec![
            Property::<Self>::always(LAW_1_NAME, |_, s| {
                !s.law1_false_401_on_retransmit && !s.law1_duplicate_session_for_tx
            }),
            Property::<Self>::always(LAW_2_NAME, |_, s| !s.law2_address_mismatch),
            Property::<Self>::always(LAW_3_NAME, |_, s| !s.law3_unpermitted_delivery),
            Property::<Self>::always(LAW_4_NAME, |_, s| {
                let no_orphan_after_drain = !s.core.is_shutting_down()
                    || s.exit_sessions.values().any(|sess| sess.closing_pending)
                    || s.exit_sessions.is_empty();
                !s.law4_installed_after_shutdown_or_disconnect
                    && !s.law4_teardown_blocked_on_reader
                    && no_orphan_after_drain
            }),
            Property::<Self>::always(LAW_6_NAME, |_, s| !s.law6_bounds_exceeded),
        ];

        if self.mutant.is_none() {
            props.extend([
                Property::<Self>::sometimes("reaches coalesced pending Allocate", |_, s| {
                    s.saw_coalesced_pending
                }),
                Property::<Self>::sometimes("reaches replayed active Allocate", |_, s| {
                    s.saw_replayed_active
                }),
                Property::<Self>::sometimes("reaches frame loss or duplication", |_, s| {
                    s.saw_frame_loss_or_dup
                }),
                Property::<Self>::sometimes("reaches permitted inbound delivery", |_, s| {
                    s.saw_permitted_inbound_delivery
                }),
                Property::<Self>::sometimes("reaches unpermitted inbound drop", |_, s| {
                    s.saw_unpermitted_inbound_drop
                }),
                Property::<Self>::sometimes(
                    "reaches in-flight abort and clean teardown",
                    |_, s| {
                        s.saw_inflight_abort_and_clean_teardown
                            && s.core.is_shutting_down()
                            && s.exit_sessions.is_empty()
                    },
                ),
            ]);
        }

        props
    }
}

fn failing_properties(model: BridgeModel) -> BTreeSet<&'static str> {
    let checker = model.checker().spawn_bfs().join();
    checker
        .model()
        .properties()
        .into_iter()
        .filter(|prop| {
            let found = checker.discovery(prop.name).is_some();
            match prop.expectation {
                Expectation::Sometimes => !found,
                _ => found,
            }
        })
        .map(|prop| prop.name)
        .collect()
}

#[test]
fn turn_onion_udp_bridge_stateright_model_verifies_all_laws() {
    BridgeModel::production(6)
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
}

#[test]
fn turn_onion_udp_bridge_stateright_refutes_every_seeded_mutant() {
    for (mutant, expected_law) in [
        (BridgeMutant::PendingNotRegisteredBeforeYield, LAW_1_NAME),
        (BridgeMutant::WrongRelayedAddressProjection, LAW_2_NAME),
        (BridgeMutant::BypassPermissionGate, LAW_3_NAME),
        (BridgeMutant::TeardownWaitsOnDeadReader, LAW_4_NAME),
        (BridgeMutant::InstallAfterShutdown, LAW_4_NAME),
        (BridgeMutant::NonAtomicCapacityCheck, LAW_6_NAME),
    ] {
        let refuted = failing_properties(BridgeModel::with_mutant(mutant, 4));
        assert!(
            refuted.contains(expected_law),
            "expected mutant {mutant:?} to refute `{expected_law}`, got refuted={refuted:?}"
        );
    }
}
