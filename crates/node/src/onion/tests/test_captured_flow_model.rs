//! A TLA-style model of one captured TCP flow carried over an onion circuit, and its exhaustive
//! check (#963, the residual `IdleTimeout` of #950; drops at a hop, #966).
//!
//! # State
//!
//! The circuit is a path of nodes `0..=n`: node `0` is the client (the gateway and its onion
//! stream), node `n` the exit, the others relays. Link `i` joins nodes `i` and `i + 1`; backward
//! cells retrace the forward path through the relays' return edges, so both directions of a hop
//! ride one link.
//!
//! ```text
//! up[i], down[i] : FIFO of cells on link i toward the exit / toward the client
//! severed[i]     : link i failed
//! destroyed[k]   : relay k tore the circuit down
//! client         : opened, eof_seen          exit : closed
//! failures_left  : link failures the environment may still cause
//! outcome        : Pending | Delivered | Destroyed | OpenFailed | OpenTimedOut | IdleTimeout
//! ```
//!
//! Cells: `Open`, `Request` up; `Opened`, `Response`, `Shutdown`, `Close` down; `Destroy` both
//! ways. `Init` holds `up[0] = [Open]`, everything else empty, and `outcome = Pending`.
//!
//! # Actions (next-state relation)
//!
//! ```text
//! Cross(i, d)  the head of link i in direction d reaches the next node:
//!                relay k            : forwards it onto the next link (discards it once destroyed)
//!                exit   ← Open      : down += [Opened]
//!                exit   ← Request   : down += [Response, Shutdown]
//!                client ← Opened    : opened; up += [Request]
//!                client ← Response  : outcome := Delivered   (unless eof_seen)
//!                client ← Shutdown | Close : eof_seen        (gateway FIN; the client stays silent)
//! Sever(i)     [failures_left > 0 ∧ ¬severed[i]]  link i fails; its cells are lost.
//! Refuse(i, d) [failures_left > 0 ∧ head ≠ Destroy ∧ it reaches a relay]  the head crosses link i,
//!                and the relay's outbox refuses to carry it on (bounded queue full).
//! Abandon(e)   [failures_left > 0 ∧ outcome = Pending]  endpoint e's own data send is refused.
//! Drop(i, d)   [failures_left > 0]  the head of link i in direction d, of any kind, reaches its
//!                node, whose namespace queue drops it (full).
//! Expire       [quiescent ∧ outcome = Pending]  the first deadline fires:
//!                ¬opened ∧ OpenFirst → OpenTimedOut;  otherwise → IdleTimeout
//! ```
//!
//! How a failure is observed is the protocol under check:
//!
//! ```text
//! Current      only a sender learns that its send failed, cell by cell:
//!                relay          : the cell is discarded, nothing else happens
//!                Refuse, Drop   : the cell is discarded
//!                Abandon client : the pump stops → eof_seen   Abandon exit : closed, nothing sent
//!                client ← Open  : OpenFailed      client ← Request : the pump stops → eof_seen
//!                exit ← Opened  : the stream is dropped
//!                exit ← Response | Shutdown : the pump stops and sends Close
//! CircuitFate  both ends of a failed link observe it, and the circuit dies with its links:
//!                node i, i + 1 of link i : destroyed; Destroy is sent away from the link
//!                client observes : outcome := Destroyed      exit observes : closed
//!                a node receiving Destroy treats it as observing a failure on that side
//!                Refuse at relay k : k destroys the circuit and sends Destroy both ways
//!                Abandon client    : outcome := Destroyed; Destroy up
//!                Abandon exit      : closed; Destroy down
//!                Drop at relay k   : as Refuse at k        Drop at an endpoint : as Abandon
//!              Destroy is a control cell: no outbox bound refuses it. A dropped Destroy is a drop
//!              like any other: the node that drops it destroys the circuit in its place.
//! Variants, each one point away from CircuitFate, checked to show that point necessary:
//!   CircuitFateBoundedDestroy    the outbox bound refuses Destroy too
//!   CircuitFateSilentDrops       Drop discards the cell, as Current does (before #966)
//!   CircuitFateDropsDestroyAway  Drop at node k destroys only onward, in the cell's direction
//! ```
//!
//! `quiescent` ≡ every live link is empty. Fairness and bounded delay: while a cell waits, a
//! `Cross` is enabled and is taken before any deadline, because every hop delay is far below the
//! deadlines; a deadline fires only once nothing is in flight. `Sever` models a hop whose delay is
//! unbounded: its send is refused once the egress budget expires, the only way a delay becomes a
//! loss.
//!
//! # Correspondence with the implementation (model completeness)
//!
//! | Model | Code |
//! |---|---|
//! | FIFO per link and direction | `drain_peer` (`onion/circuit/send_outbox.rs`), one ordered lane per peer; core keeps a category's frames in order on one channel lane (`MessageCategory::channel_lane`, #906); namespace dispatch applies envelopes in arrival order (`extension/dispatch.rs`); the reducer re-emits `SealAndSend` in order (`circuit/reducer.rs`) |
//! | backward retraces forward | `advance_backward` routes by the return edge learned from the forward cell (`circuit/reducer.rs`) |
//! | exit reactions | `open_exit_stream`, then `pump_tcp_duplex` sends `Data` and `Shutdown` from one task (`onion/tcp/mod.rs`, `onion/tcp/pump.rs`) |
//! | client reactions | `handle_client_payload` → `send_client_inbound`; the gateway sends the request only after `BridgeEvent::Opened` (`gateway/src/runtime.rs`) |
//! | `Sever` | a link generation retires (`PeerRetired`, observed by both ends); under `Current` also `application_egress` refusing a send within `DATA_CHANNEL_SEND_ACCEPT_BUDGET` = 5 s, which `CircuitFate` retries instead (`swarm/transport/timeouts.rs`, `payload_send.rs`) |
//! | `Drop` | the namespace queue refuses an envelope (`NAMESPACE_QUEUE_CAPACITY` envelopes, `NAMESPACE_QUEUE_WEIGHT` bytes; `NamespaceQueues::submit`, `extension/dispatch.rs`) |
//! | `CircuitFate`, a drop is observed | a refusal from an admitted peer queues `LinkFact::Dropped`, outside the bounds, at most one per peer while it waits, and never dropped (`extension/dispatch.rs`); the onion protocol maps it to `OnionCircuitInput::Link` (`Protocol::link_fact`, `circuit/protocol.rs`) |
//! | `CircuitFate`, `Drop` at a relay | `destroy_link` removes every return edge on the link and seals `Destroy` toward both neighbours (`circuit/reducer.rs`) |
//! | `CircuitFate`, `Drop` at an endpoint | `Teardown(Link(Dropped))` ends the link's streams with `TcpStreamAbort::CellDropped`, a cause that originates here, so each sends `Destroy` to its adjacent hop; the cause is recorded beside the stream's lane (`TcpStreamFate`), so a full lane cannot lose it (`onion/tcp/fate.rs`, `tcp/inbound.rs`) |
//! | `Refuse`, `Abandon` | the per-peer onion outbox bound (`MAX_PENDING_ONION_SENDS_PER_PEER`, `MAX_PENDING_ONION_SEND_BYTES_PER_PEER`, `onion/circuit/send_outbox.rs`) refuses `enqueue_sealed` at a relay, or an endpoint's `send_sealed` |
//! | `Current`, relay | the outbox only logs a failed send: `completion: None` (`send_outbox.rs`) |
//! | `CircuitFate`, refusal is a delay | `send_until_settled` sends a cell refused before acceptance again while its peer is admitted (`send_outbox.rs`; `Error::is_refused_before_acceptance`, `error::send_class`) |
//! | `CircuitFate`, both ends observe `Sever` | `PeerRetired` reaches each end's onion protocol in namespace order and is never dropped (`Protocol::peer_retired`, `extension/dispatch.rs`) |
//! | `CircuitFate`, relay destruction | `destroy_adjacent`, `destroy_circuit`, `refuse_circuit` remove the return edge and seal `Destroy` toward the other neighbours (`circuit/reducer.rs`); the shell feeds a refused cell back as `CellRefused` (`circuit/shell.rs`) |
//! | `CircuitFate`, endpoints | `Teardown` ends client and exit streams and pending HTTPS requests (`onion/tcp/fate.rs`, `https/client.rs`); a stream aborted for a local cause sends `Destroy` to its adjacent hop (`tcp/client.rs`, `tcp/exit.rs`) |
//! | client outcome `Destroyed` | the pump ends `Aborted(CircuitDestroyed)`; the gateway connector reports `OnionStreamReset`, and the gateway resets the flow with `StreamReset` (`onion/gateway.rs`, `gateway/src/bridge.rs`) |
//! | `Destroy` exempt from the bound | `CellAdmission::Exempt`, `Charge::Exempt` (`send_outbox.rs`) |
//! | `Current`, endpoints | `open_tcp_stream` fails the open with `OnionUnavailable`; `accept_exit_open` removes the exit stream; a pump breaks and sends `Close` (`onion/tcp/mod.rs`, `pump.rs`) |
//! | `Expire` → `IdleTimeout` | smoltcp `set_timeout(flow_idle_timeout)` from the last client segment (`gateway/src/tcp/mod.rs`) |
//! | `Expire` → `OpenTimedOut` | `TCP_OPEN_TIMEOUT_SECS` from the `Open` send (`onion/tcp/mod.rs`) |
//! | `DeadlineOrder` | the request segment precedes the `Open` send, so the idle deadline is first iff `I ≤ O + Δ`: the test config (`I = O = 30 s`) is `IdleFirst`, the production default (`I = 300 s`) is `OpenFirst` |
//!
//! The core application inbox (`APPLICATION_INBOX_WEIGHT`) also drops a live message when full,
//! counted only. The node's inbox reader never waits, so it fills only while the reader is
//! starved; it is outside this model until its drops are observed too (#969).
//!
//! The other non-delivery sites on the path are unreachable within one flow and are not actions:
//! the crypto admission gate (4096 operations per 60 s per peer, `circuit/limiter.rs`), forward
//! layer expiry (≥ 120 s, `circuit/expiry.rs`), the relay return-edge TTL (120 s), liveness
//! eviction (45 s after a probe issued 15 s after admission), the client inbound channel (32
//! frames), and the exit's open-response quantum (250 ms, a delay).
//!
//! # Results (witnessed below for every circuit of 2 to 5 links)
//!
//! - **R1** Without a failure, every maximal execution ends `Delivered`, under both protocols.
//! - **R2** Under `Current`, one link failure can end the flow in `IdleTimeout` under either
//!   deadline order: a loss after the open is observed only by the gateway's idle clock.
//! - **R3** Under `CircuitFate`, up to two failures of any kind (`Sever`, `Refuse`, `Abandon`,
//!   `Drop`, a dropped `Destroy` included) never end the flow in `IdleTimeout`: every outcome is the
//!   response or a typed failure.
//! - **R4** Under `CircuitFateBoundedDestroy`, one `Refuse` can end the flow in `IdleTimeout`: the
//!   relay's `Destroy` toward the full queue is lost, so `Destroy` must be exempt from the bound.
//! - **R5** Under `CircuitFateSilentDrops`, one `Drop` can end the flow in `IdleTimeout`: no sender
//!   learns of a cell lost after its send was accepted, so the drop must be observed where it
//!   happens.
//! - **R6** Under `CircuitFateDropsDestroyAway`, one `Drop` can end the flow in `IdleTimeout`: the
//!   side that sent the dropped cell never learns, so a drop must destroy the circuit toward both
//!   neighbours, unlike a retirement, whose far side observes it by itself.
//!
//! Derivation of #950: by R1 and R2, the untyped reset occurs iff a link of the circuit fails
//! while the flow is in flight. Per-hop order holds; the missing law is delivery: the circuit
//! treats `send_direct` as a reliable FIFO, while the application egress is reliable only within
//! its admission budget, and only the sender of a refused cell learns of the loss.
//!
//! Law (Circuit fate), witnessed by R3: every cell of a circuit is delivered, or both endpoints of
//! the circuit observe its destruction. A failed send is observed by its sender alone, and may lie
//! on the only path toward the other endpoint; the failure is made symmetric at the link, so the
//! node on each side observes it and carries `Destroy` away from it. A drop is observed by its
//! receiver alone, so the receiver carries `Destroy` both ways (R5, R6).

use std::collections::BTreeSet;
use std::collections::VecDeque;

/// Smallest link count: a route has at least one relay.
const MIN_LINKS: usize = 2;

/// Largest link count checked; the results hold for each, the induction step `N → N + 1`.
const MAX_LINKS: usize = 5;

/// One cell of the circuit, by its role in the flow.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Cell {
    Open,
    Request,
    Opened,
    Response,
    Shutdown,
    Close,
    Destroy,
}

/// What the client observes when the flow ends.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Outcome {
    Pending,
    Delivered,
    Destroyed,
    OpenFailed,
    OpenTimedOut,
    IdleTimeout,
}

impl Outcome {
    /// Whether the client learns why the flow ended, or got its response.
    const fn is_typed(self) -> bool {
        !matches!(self, Self::IdleTimeout | Self::Pending)
    }
}

/// Which deadline fires first at quiescence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeadlineOrder {
    /// The gateway's idle clock fires before the open deadline (test config).
    IdleFirst,
    /// An unfinished open fails typed first (production default).
    OpenFirst,
}

/// How a failed link is observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Protocol {
    /// Only the sender of a refused cell learns of it.
    Current,
    /// Both ends of a failed link observe it and destroy the circuit, and a node that drops a cell
    /// destroys the circuit in its place.
    CircuitFate,
    /// `CircuitFate` with `Destroy` held to the outbox bound: a relay's `Destroy` toward the full
    /// queue that refused a cell is refused too. Not implemented; checked to show the exemption
    /// is necessary.
    CircuitFateBoundedDestroy,
    /// `CircuitFate` whose nodes drop cells silently, as before #966. Checked to show that a drop
    /// must be observed where it happens.
    CircuitFateSilentDrops,
    /// `CircuitFate` whose node observing a drop destroys the circuit only away from the link the
    /// cell came on, as after a retirement. Not implemented; checked to show that a drop must be
    /// carried toward both neighbours.
    CircuitFateDropsDestroyAway,
}

impl Protocol {
    /// Whether both ends of a failed link observe it.
    fn observes_link_fate(self) -> bool {
        !matches!(self, Self::Current)
    }

    /// Whether a node that drops a cell observes the drop.
    fn observes_drops(self) -> bool {
        !matches!(self, Self::Current | Self::CircuitFateSilentDrops)
    }
}

/// Direction of travel on a link.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Direction {
    /// Toward the exit.
    Up,
    /// Toward the client.
    Down,
}

/// One link of the circuit.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct Link {
    up: VecDeque<Cell>,
    down: VecDeque<Cell>,
    severed: bool,
}

impl Link {
    /// The queue of `direction`.
    fn queue_mut(&mut self, direction: Direction) -> &mut VecDeque<Cell> {
        match direction {
            Direction::Up => &mut self.up,
            Direction::Down => &mut self.down,
        }
    }
}

/// One model state.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct State {
    links: Vec<Link>,
    destroyed: Vec<bool>,
    opened: bool,
    eof_seen: bool,
    exit_closed: bool,
    failures_left: u8,
    outcome: Outcome,
}

impl State {
    /// `Init` for a circuit of `links` links with `failures` link failures allowed.
    fn initial(links: usize, failures: u8) -> Self {
        let mut state = Self {
            links: vec![Link::default(); links],
            destroyed: vec![false; links + 1],
            opened: false,
            eof_seen: false,
            exit_closed: false,
            failures_left: failures,
            outcome: Outcome::Pending,
        };
        if let Some(first) = state.links.first_mut() {
            first.up.push_back(Cell::Open);
        }
        state
    }

    /// The exit's node index.
    fn exit(&self) -> usize {
        self.links.len()
    }

    /// End a pending flow with `outcome`; a flow that already ended keeps its outcome.
    fn conclude(&mut self, outcome: Outcome) {
        if self.outcome == Outcome::Pending {
            self.outcome = outcome;
        }
    }

    /// Whether no cell is in flight on a live link.
    fn quiescent(&self) -> bool {
        self.links
            .iter()
            .all(|link| link.up.is_empty() && link.down.is_empty())
    }

    /// The link `node` sends on in `direction`, by index.
    fn link_from(node: usize, direction: Direction) -> Option<usize> {
        match direction {
            Direction::Up => Some(node),
            Direction::Down => node.checked_sub(1),
        }
    }

    /// `node` sends `cell` in `direction`; a send on a failed link fails at its sender.
    fn send(&mut self, protocol: Protocol, node: usize, direction: Direction, cell: Cell) {
        let Some(index) = Self::link_from(node, direction) else {
            return;
        };
        let Some(link) = self.links.get_mut(index) else {
            return;
        };
        if link.severed {
            self.send_failed(protocol, node, cell);
        } else {
            link.queue_mut(direction).push_back(cell);
        }
    }

    /// How the sender of `cell` reacts to its refused send. Under `CircuitFate` the sender has
    /// already observed the failure of its link, so nothing more happens.
    fn send_failed(&mut self, protocol: Protocol, node: usize, cell: Cell) {
        if protocol.observes_link_fate() {
            return;
        }
        let exit = self.exit();
        match (node, cell) {
            (0, Cell::Open) => self.conclude(Outcome::OpenFailed),
            (0, Cell::Request) => self.eof_seen = true,
            (node, Cell::Opened) if node == exit => self.exit_closed = true,
            (node, Cell::Response | Cell::Shutdown) if node == exit => {
                self.send(protocol, node, Direction::Down, Cell::Close);
            }
            _ => {}
        }
    }

    /// `node` observes the circuit's destruction on the side opposite `toward`, and carries it on.
    fn observe_destruction(&mut self, protocol: Protocol, node: usize, toward: Direction) {
        if node == 0 {
            self.conclude(Outcome::Destroyed);
        } else if node == self.exit() {
            self.exit_closed = true;
        } else if let Some(destroyed) = self.destroyed.get_mut(node) {
            if !*destroyed {
                *destroyed = true;
                self.send(protocol, node, toward, Cell::Destroy);
            }
        }
    }

    /// `cell` arrives at `node`, travelling in `direction`.
    fn receive(&mut self, protocol: Protocol, node: usize, direction: Direction, cell: Cell) {
        let exit = self.exit();
        if cell == Cell::Destroy {
            self.observe_destruction(protocol, node, direction);
            return;
        }
        if node == exit {
            self.exit_receives(protocol, cell);
        } else if node == 0 {
            self.client_receives(protocol, cell);
        } else if !self.destroyed.get(node).copied().unwrap_or(true) {
            self.send(protocol, node, direction, cell);
        }
    }

    /// Exit reactions: `Opened` after the open, then the response and its end from one pump.
    fn exit_receives(&mut self, protocol: Protocol, cell: Cell) {
        if self.exit_closed {
            return;
        }
        let exit = self.exit();
        let replies: &[Cell] = match cell {
            Cell::Open => &[Cell::Opened],
            Cell::Request => &[Cell::Response, Cell::Shutdown],
            _ => &[],
        };
        for reply in replies {
            self.send(protocol, exit, Direction::Down, *reply);
        }
    }

    /// Client reactions: the gateway sends the request once the stream opened; a response after
    /// the stream ended is not delivered.
    fn client_receives(&mut self, protocol: Protocol, cell: Cell) {
        match cell {
            Cell::Opened => {
                self.opened = true;
                self.send(protocol, 0, Direction::Up, Cell::Request);
            }
            Cell::Response if !self.eof_seen => self.conclude(Outcome::Delivered),
            Cell::Shutdown | Cell::Close => self.eof_seen = true,
            _ => {}
        }
    }

    /// `Cross(i, d)`: the head of link `index` in `direction` reaches the next node.
    fn cross(&self, protocol: Protocol, index: usize, direction: Direction) -> Option<Self> {
        let mut next = self.clone();
        let cell = next
            .links
            .get_mut(index)?
            .queue_mut(direction)
            .pop_front()?;
        let node = match direction {
            Direction::Up => index + 1,
            Direction::Down => index,
        };
        next.receive(protocol, node, direction, cell);
        Some(next)
    }

    /// `Sever(i)`: link `index` fails and its cells are lost.
    fn sever(&self, protocol: Protocol, index: usize) -> Option<Self> {
        if self.failures_left == 0 || self.links.get(index)?.severed {
            return None;
        }
        let mut next = self.clone();
        next.failures_left -= 1;
        let link = next.links.get_mut(index)?;
        link.severed = true;
        let lost_up = std::mem::take(&mut link.up);
        let lost_down = std::mem::take(&mut link.down);
        if protocol.observes_link_fate() {
            next.observe_destruction(protocol, index, Direction::Down);
            next.observe_destruction(protocol, index + 1, Direction::Up);
        } else {
            lost_up
                .into_iter()
                .for_each(|cell| next.send_failed(protocol, index, cell));
            lost_down
                .into_iter()
                .for_each(|cell| next.send_failed(protocol, index + 1, cell));
        }
        Some(next)
    }

    /// `node` destroys its circuit and carries the destruction to both of its neighbours.
    fn destroy_at(&mut self, protocol: Protocol, node: usize) {
        if let Some(destroyed) = self.destroyed.get_mut(node) {
            if !*destroyed {
                *destroyed = true;
                self.send(protocol, node, Direction::Up, Cell::Destroy);
                self.send(protocol, node, Direction::Down, Cell::Destroy);
            }
        }
    }

    /// `Refuse(i, d)`: the head of link `index` reaches a relay, which cannot carry it on.
    fn refuse(&self, protocol: Protocol, index: usize, direction: Direction) -> Option<Self> {
        let node = match direction {
            Direction::Up => index + 1,
            Direction::Down => index,
        };
        if self.failures_left == 0 || node == 0 || node == self.exit() {
            return None;
        }
        let mut next = self.clone();
        let queue = next.links.get_mut(index)?.queue_mut(direction);
        if queue.front()? == &Cell::Destroy {
            return None;
        }
        queue.pop_front();
        next.failures_left -= 1;
        if protocol.observes_link_fate() {
            next.destroy_at(protocol, node);
        }
        if protocol == Protocol::CircuitFateBoundedDestroy {
            // The queue that refused the cell refuses this relay's Destroy behind it as well.
            let full = State::link_from(node, direction)?;
            next.links.get_mut(full)?.queue_mut(direction).pop_back();
        }
        Some(next)
    }

    /// The endpoint `node` ends its stream for a cause of its own and destroys the circuit toward
    /// its adjacent hop.
    fn end_endpoint(&mut self, protocol: Protocol, node: usize) {
        if node == 0 {
            self.conclude(Outcome::Destroyed);
            self.send(protocol, 0, Direction::Up, Cell::Destroy);
        } else {
            self.exit_closed = true;
            self.send(protocol, node, Direction::Down, Cell::Destroy);
        }
    }

    /// `Abandon(e)`: the endpoint `node` aborts its stream after its own send was refused.
    fn abandon(&self, protocol: Protocol, node: usize) -> Option<Self> {
        if self.failures_left == 0 || self.outcome != Outcome::Pending {
            return None;
        }
        let mut next = self.clone();
        next.failures_left -= 1;
        if protocol.observes_link_fate() {
            next.end_endpoint(protocol, node);
        } else if node == next.exit() {
            next.exit_closed = true;
        } else {
            next.eof_seen = true;
        }
        Some(next)
    }

    /// `Drop(i, d)`: the head of link `index` in `direction`, of any kind, reaches its node, whose
    /// namespace queue drops it. A node that observes the drop destroys the circuit in its place:
    /// a relay toward both neighbours, an endpoint toward its adjacent hop.
    fn drop_cell(&self, protocol: Protocol, index: usize, direction: Direction) -> Option<Self> {
        if self.failures_left == 0 {
            return None;
        }
        let mut next = self.clone();
        next.links
            .get_mut(index)?
            .queue_mut(direction)
            .pop_front()?;
        next.failures_left -= 1;
        let node = match direction {
            Direction::Up => index + 1,
            Direction::Down => index,
        };
        if protocol == Protocol::CircuitFateDropsDestroyAway {
            next.observe_destruction(protocol, node, direction);
        } else if protocol.observes_drops() {
            if node == 0 || node == next.exit() {
                next.end_endpoint(protocol, node);
            } else {
                next.destroy_at(protocol, node);
            }
        }
        Some(next)
    }

    /// `Expire`: at quiescence the first deadline of `order` ends a pending flow.
    fn expire(&self, order: DeadlineOrder) -> Option<Self> {
        if self.outcome != Outcome::Pending || !self.quiescent() {
            return None;
        }
        let mut next = self.clone();
        next.outcome = match (self.opened, order) {
            (false, DeadlineOrder::OpenFirst) => Outcome::OpenTimedOut,
            _ => Outcome::IdleTimeout,
        };
        Some(next)
    }

    /// Every successor under the next-state relation; none once the flow ended.
    fn successors(&self, protocol: Protocol, order: DeadlineOrder) -> Vec<Self> {
        if self.outcome != Outcome::Pending {
            return Vec::new();
        }
        let mut next = Vec::new();
        for index in 0..self.links.len() {
            for direction in [Direction::Up, Direction::Down] {
                next.extend(self.cross(protocol, index, direction));
                next.extend(self.refuse(protocol, index, direction));
                next.extend(self.drop_cell(protocol, index, direction));
            }
            next.extend(self.sever(protocol, index));
        }
        next.extend(self.abandon(protocol, 0));
        next.extend(self.abandon(protocol, self.exit()));
        next.extend(self.expire(order));
        next
    }
}

/// The outcomes of every maximal execution from `Init`: exhaustive exploration of the state graph.
fn outcomes(
    protocol: Protocol,
    order: DeadlineOrder,
    links: usize,
    failures: u8,
) -> BTreeSet<Outcome> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![State::initial(links, failures)];
    let mut ended = BTreeSet::new();
    while let Some(state) = frontier.pop() {
        if !seen.insert(state.clone()) {
            continue;
        }
        let successors = state.successors(protocol, order);
        if successors.is_empty() {
            ended.insert(state.outcome);
        }
        frontier.extend(successors);
    }
    ended
}

/// Every protocol and deadline order, for the checks that range over both.
const PROTOCOLS: [Protocol; 5] = [
    Protocol::Current,
    Protocol::CircuitFate,
    Protocol::CircuitFateBoundedDestroy,
    Protocol::CircuitFateSilentDrops,
    Protocol::CircuitFateDropsDestroyAway,
];

/// Both deadline orders.
const ORDERS: [DeadlineOrder; 2] = [DeadlineOrder::IdleFirst, DeadlineOrder::OpenFirst];

/// R1: without a failure, every maximal execution delivers the response.
#[test]
fn test_without_a_failure_every_execution_delivers() {
    for links in MIN_LINKS..=MAX_LINKS {
        for (protocol, order) in PROTOCOLS
            .into_iter()
            .flat_map(|protocol| ORDERS.map(|order| (protocol, order)))
        {
            assert_eq!(
                outcomes(protocol, order, links, 0),
                BTreeSet::from([Outcome::Delivered]),
                "links = {links}, {protocol:?}, {order:?}"
            );
        }
    }
}

/// R2: under the current protocol, one link failure can end the flow in an untyped reset under
/// either deadline order (the residual #950 reset).
#[test]
fn test_current_protocol_lets_a_link_failure_end_in_an_untyped_reset() {
    for links in MIN_LINKS..=MAX_LINKS {
        for order in ORDERS {
            let ended = outcomes(Protocol::Current, order, links, 1);
            assert!(
                ended.contains(&Outcome::IdleTimeout),
                "links = {links}, {order:?}: {ended:?}"
            );
        }
    }
}

/// R3, Law (Circuit fate): up to two failures of any kind, drops included, end the flow with the
/// response or a typed failure, never an untyped reset.
#[test]
fn test_circuit_fate_types_every_link_failure() {
    for links in MIN_LINKS..=MAX_LINKS {
        for order in ORDERS {
            for failures in 1..=2 {
                let ended = outcomes(Protocol::CircuitFate, order, links, failures);
                assert!(
                    ended.iter().all(|outcome| outcome.is_typed()),
                    "links = {links}, {order:?}, failures = {failures}: {ended:?}"
                );
                assert!(ended.contains(&Outcome::Destroyed));
            }
        }
    }
}

/// R4: the outbox exemption of `Destroy` is necessary: bounded like a data cell, a relay's
/// `Destroy` toward the queue that refused a cell is lost, and the flow can end untyped.
#[test]
fn test_destroy_must_be_exempt_from_the_outbox_bound() {
    for links in MIN_LINKS..=MAX_LINKS {
        let ended = outcomes(
            Protocol::CircuitFateBoundedDestroy,
            DeadlineOrder::IdleFirst,
            links,
            1,
        );
        assert!(
            ended.contains(&Outcome::IdleTimeout),
            "links = {links}: {ended:?}"
        );
    }
}

/// R5: a drop must be observed where it happens: dropped silently, as before #966, one cell lost
/// at a hop can end the flow untyped, since no sender learns of it.
#[test]
fn test_a_drop_must_be_observed_where_it_happens() {
    for links in MIN_LINKS..=MAX_LINKS {
        for order in ORDERS {
            let ended = outcomes(Protocol::CircuitFateSilentDrops, order, links, 1);
            assert!(
                ended.contains(&Outcome::IdleTimeout),
                "links = {links}, {order:?}: {ended:?}"
            );
        }
    }
}

/// R6: a drop must be carried toward both neighbours: destroyed only away from the link the cell
/// came on, the side that sent it never learns, and one drop can end the flow untyped.
#[test]
fn test_a_drop_must_destroy_toward_both_neighbours() {
    for links in MIN_LINKS..=MAX_LINKS {
        for order in ORDERS {
            let ended = outcomes(Protocol::CircuitFateDropsDestroyAway, order, links, 1);
            assert!(
                ended.contains(&Outcome::IdleTimeout),
                "links = {links}, {order:?}: {ended:?}"
            );
        }
    }
}
