use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use serde::Deserialize;
use serde::Serialize;

use super::cell::encode_message;
use super::cell::OnionCellBucket;
use super::codec::OnionCircuitInput;
use super::codec::OnionWireMessage;
use super::protocol::OnionCircuitCapabilities;
use super::OnionBackwardFrame;
use super::OnionCircuitId;
use super::OnionCircuitPayload;
use super::OnionClientReturn;
use super::OnionForwardFrame;
use super::OnionForwardLayer;
use super::OnionForwardNonce;
use super::OnionForwardSequence;
use super::MAX_ONION_RELAY_CIRCUITS;
use super::ONION_FORWARD_MAX_VALIDITY_MS;
use super::ONION_RELAY_RETURN_TTL_MS;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::LinkFact;
use crate::extension::ext::Transition;
use crate::onion::OnionRouteError;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct RelayReturnKey {
    pub(super) circuit_id: OnionCircuitId,
    pub(super) next_hop: Did,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RelayReturnEdge {
    pub(super) key: RelayReturnKey,
    pub(super) previous_hop: Did,
    pub(super) previous_circuit_id: OnionCircuitId,
    pub(super) previous_delegatee_public_key: PublicKey<33>,
    /// The next hop's delegatee key, which seals a `Destroy` carried toward the exit.
    pub(super) next_delegatee_public_key: PublicKey<33>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RelayReturnEntry {
    previous_hop: Did,
    previous_circuit_id: OnionCircuitId,
    previous_delegatee_public_key: PublicKey<33>,
    next_delegatee_public_key: PublicKey<33>,
    expires_at_ms: u128,
}

/// Stateful return-hop table for encrypted relay circuits.
///
/// Invariant: every `(next_edge_id, next_hop) -> (previous_edge_id, previous_hop)` entry
/// represents exactly one live reverse edge learned from a prior forward relay action.
/// Preservation: forward relay insertion purges expired entries before capacity checks and never
/// rewrites a live key to a different previous hop; backward frames purge expired entries before
/// lookup and refresh only the matched edge.
/// Return-state removal is TTL-based because backward close semantics are encrypted to the
/// client and are not authenticated to relays.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnionCircuitState {
    relay_returns: Arc<BTreeMap<RelayReturnKey, RelayReturnEntry>>,
}

impl OnionCircuitState {
    #[cfg(all(test, rings_native))]
    pub(super) fn relay_return_count(&self) -> usize {
        self.relay_returns.len()
    }

    #[cfg(all(test, rings_native))]
    pub(super) fn shares_return_table_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.relay_returns, &other.relay_returns)
    }
}

/// Effects emitted by the route-aware circuit reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionCircuitEffect {
    /// Run forward-layer crypto at the shell boundary and re-inject the decoded layer.
    DecryptCell {
        /// Authenticated immediate sender.
        from: Did,
        /// Public padding class; direction and exact length remain encrypted.
        bucket: OnionCellBucket,
        /// Hop-encrypted fixed-size cell payload.
        sealed: AeadCiphertext,
    },
    /// Decrypt one forward onion layer after its outer cell has authenticated the direction.
    DecryptForward {
        /// Authenticated immediate sender.
        from: Did,
        /// Cell receipt time captured once at the shell boundary.
        received_at_ms: u128,
        /// Public padding class to preserve on the next edge.
        bucket: OnionCellBucket,
        /// Edge-local circuit id bound into the layer AEAD.
        circuit_id: OnionCircuitId,
        /// Forward onion layer encrypted to this node.
        payload: AeadCiphertext,
    },
    /// Encrypt and send one fixed-size cell at the shell boundary.
    SealAndSend {
        /// Next hop.
        to: Did,
        /// Next hop delegatee key authenticated inside the current layer.
        recipient: PublicKey<33>,
        /// Padding class preserved across relay edges.
        bucket: OnionCellBucket,
        /// Encoded direction and frame protected by the cell AEAD.
        encoded_message: Bytes,
        /// What the cell is, which decides how the outbox admits it.
        cell: OnionRelayedCell,
    },
    /// A forward frame reached the exit.
    Exit {
        /// Authenticated immediate sender.
        from: Did,
        /// Random circuit correlation id.
        circuit_id: OnionCircuitId,
        /// Immediate return peer.
        return_peer: Did,
        /// Immediate return peer delegatee key for the first backward cell.
        return_delegatee_public_key: PublicKey<33>,
        /// Client return key.
        client: OnionClientReturn,
        /// Replay token consumed by one-shot exit operations; stream frames use `forward_sequence`.
        forward_nonce: OnionForwardNonce,
        /// Monotonic client-to-exit sequence within this circuit.
        forward_sequence: OnionForwardSequence,
        /// Application payload.
        payload: OnionCircuitPayload,
    },
    /// Tear down the local endpoint of a destroyed circuit at the shell boundary.
    Teardown(OnionCircuitTeardown),
    /// Decrypt a backward frame for this local client at the shell boundary.
    DecryptClient {
        /// Authenticated immediate sender.
        from: Did,
        /// Random circuit correlation id.
        circuit_id: OnionCircuitId,
        /// AEAD payload encrypted to the client session public key.
        payload: AeadCiphertext,
    },
}

/// What one sealed cell is to the outbox (#963).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionRelayedCell {
    /// A relayed data cell of the circuit `to` knows as `circuit_id`, held to the outbox bounds;
    /// a refusal destroys the circuit (`CellRefused`).
    Data {
        /// Edge-local circuit id on the link to the cell's next hop.
        circuit_id: OnionCircuitId,
    },
    /// A `Destroy`, exempt from the outbox bounds.
    Destroy,
}

/// The local endpoints of destroyed circuits, as the shell finds them (#963).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionCircuitTeardown {
    /// Every circuit whose adjacent hop is the peer of the fact: its link retired, or a cell
    /// from it was dropped here (#966). After a drop the link is up, so each endpoint destroys
    /// its circuit toward the peer as well.
    Link(LinkFact),
    /// The circuit `circuit_id` whose adjacent hop `from` destroyed it.
    Circuit {
        /// Adjacent hop that sent the `Destroy`.
        from: Did,
        /// Edge-local circuit id on the link from `from`.
        circuit_id: OnionCircuitId,
    },
}

impl OnionCircuitTeardown {
    /// Whether this teardown ends a local endpoint whose circuit is `circuit_id` on its link to
    /// the adjacent hop `adjacent`.
    pub fn ends(self, adjacent: Did, circuit_id: OnionCircuitId) -> bool {
        match self {
            Self::Link(fact) => fact.peer() == adjacent,
            Self::Circuit {
                from,
                circuit_id: destroyed,
            } => from == adjacent && destroyed == circuit_id,
        }
    }
}

/// Pure state relation for onion circuits.
///
/// ```text
/// CellObserved(encrypted)      -> [DecryptCell]
/// CellReady(forward)           -> state, [DecryptForward]
/// ForwardReady(Relay)          -> state' with return edge, [SealAndSend next]
/// ForwardReady(Exit s)         -> state, [Exit]
/// CellReady(backward match)    -> state' with refreshed edge, [SealAndSend previous]
/// CellReady(backward no match) -> state, [DecryptClient]
/// CellReady(cover)             -> state, []
/// CellRefused(to, c)           -> state' without the edge, [SealAndSend Destroy to both neighbours]
/// Link(Retired p)              -> state' without p's edges, [SealAndSend Destroy away from p]
///                                 ++ [Teardown(Link (Retired p))]
/// Link(Dropped p)              -> state' without p's edges, [SealAndSend Destroy to both
///                                 neighbours] ++ [Teardown(Link (Dropped p))]
/// CellReady(Destroy c, from x) -> state' without the edge, [SealAndSend Destroy away from x]
///                                 or [Teardown(Circuit x c)] when no relay edge matches
/// ```
///
/// Law (Circuit fate, #963): after `Link(fact)` no return edge adjacent to `fact.peer()` remains.
/// Each edge a retirement removes sends exactly one `Destroy`, to its neighbour other than the
/// retired peer; each edge a drop removes sends one to each neighbour, since the link to the peer
/// is still up and the peer's side has not observed the loss (#966). A relay that receives
/// `Destroy` treats it as the retirement of that one circuit on the sender's side. So a
/// destruction observed on either side of a link reaches both endpoints of every circuit it
/// carries, through the circuit's own links; a `Destroy` dropped on its way is itself a drop, so
/// the node that dropped it destroys the circuit in its place.
///
/// The layer variant is the symbol it applies: a `Relay` layer applies `relay`, evaluated here, in
/// the pure core, as `relay = id` (peel, record the return edge, forward the inner layer
/// unchanged); an `Exit` layer applies the world-facing symbol `s = payload.service ∈ Σ_W`, which
/// the type of the service name guarantees, and becomes an `Exit` effect evaluated by the node's
/// `OnionAlgebra` in the shell.
///
/// Law: replaying `apply(state, input)` with the same values returns the same `(state', effects)`.
/// Clocks, crypto, IO, and locks are represented by effects and live in the shell.
#[derive(Clone, Debug)]
pub(super) struct OnionCircuitReducer {
    capabilities: OnionCircuitCapabilities,
}

impl OnionCircuitReducer {
    pub(super) const fn new(capabilities: OnionCircuitCapabilities) -> Self {
        Self { capabilities }
    }

    pub(super) fn apply(
        &self,
        state: &OnionCircuitState,
        input: OnionCircuitInput,
    ) -> Transition<OnionCircuitState, OnionCircuitEffect> {
        let mut state = state.clone();
        let effect = match input {
            OnionCircuitInput::Link(fact) => {
                let mut effects = destroy_link(&mut state, fact);
                effects.push(OnionCircuitEffect::Teardown(OnionCircuitTeardown::Link(
                    fact,
                )));
                return Transition::with(state, effects);
            }
            OnionCircuitInput::CellRefused { to, circuit_id } => {
                let effects = refuse_circuit(&mut state, to, circuit_id);
                return Transition::with(state, effects);
            }
            OnionCircuitInput::CellObserved {
                from,
                bucket,
                sealed,
            } => Ok(Some(OnionCircuitEffect::DecryptCell {
                from,
                bucket,
                sealed,
            })),
            OnionCircuitInput::CellReady {
                from,
                received_at_ms,
                bucket,
                message,
            } => self.advance_cell(from, received_at_ms, bucket, message, &mut state),
            OnionCircuitInput::ForwardReady {
                from,
                received_at_ms,
                bucket,
                circuit_id,
                layer,
            } => self
                .advance_forward(from, received_at_ms, bucket, circuit_id, layer, &mut state)
                .map(Some),
        };

        match effect {
            Ok(Some(effect)) => Transition::with(state, vec![effect]),
            Ok(None) => Transition::pure(state),
            Err(error) => {
                tracing::debug!("drop onion circuit message: {error}");
                Transition::pure(state)
            }
        }
    }

    fn advance_cell(
        &self,
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        message: OnionWireMessage,
        state: &mut OnionCircuitState,
    ) -> Result<Option<OnionCircuitEffect>> {
        match message {
            OnionWireMessage::Forward(frame) => {
                if !self.capabilities.registers_relay() {
                    return Err(Error::NoPermission);
                }
                Ok(Some(OnionCircuitEffect::DecryptForward {
                    from,
                    received_at_ms,
                    bucket,
                    circuit_id: frame.circuit_id,
                    payload: frame.layer,
                }))
            }
            OnionWireMessage::Backward(frame) => self
                .advance_backward(from, received_at_ms, bucket, frame, state)
                .map(Some),
            OnionWireMessage::Cover => Ok(None),
            OnionWireMessage::Destroy { circuit_id } => {
                destroy_circuit(state, from, circuit_id).map(Some)
            }
        }
    }

    fn advance_forward(
        &self,
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        circuit_id: OnionCircuitId,
        layer: OnionForwardLayer,
        state: &mut OnionCircuitState,
    ) -> Result<OnionCircuitEffect> {
        // Pre: `ForwardReady` is emitted only by the `DecryptForward` effect admitted in
        // `advance_cell`, where `accepts_forward_layers` is checked. The typed effect/feedback
        // transition carries that proof into this stage; repeating the capability check would
        // not guard a second ingress boundary.
        match layer {
            OnionForwardLayer::Relay {
                next_hop,
                next_circuit_id,
                next_delegatee_public_key,
                return_delegatee_public_key,
                inner,
            } => {
                self.validate_relay_forward()?;
                remember_return_hop(
                    state,
                    MAX_ONION_RELAY_CIRCUITS,
                    ONION_RELAY_RETURN_TTL_MS,
                    RelayReturnEdge {
                        key: RelayReturnKey {
                            circuit_id: next_circuit_id,
                            next_hop,
                        },
                        previous_hop: from,
                        previous_circuit_id: circuit_id,
                        previous_delegatee_public_key: return_delegatee_public_key,
                        next_delegatee_public_key,
                    },
                    received_at_ms,
                )?;
                encode_message(&OnionWireMessage::Forward(OnionForwardFrame {
                    circuit_id: next_circuit_id,
                    layer: inner,
                }))
                .map(|encoded_message| OnionCircuitEffect::SealAndSend {
                    to: next_hop,
                    recipient: next_delegatee_public_key,
                    bucket,
                    encoded_message,
                    cell: OnionRelayedCell::Data {
                        circuit_id: next_circuit_id,
                    },
                })
            }
            OnionForwardLayer::Exit {
                process_epoch,
                client,
                return_delegatee_public_key,
                expires_at_ms,
                forward_nonce,
                forward_sequence,
                payload,
            } => {
                if !self.capabilities.permits_exit_layer(process_epoch) {
                    return Err(Error::OnionRouteError(
                        OnionRouteError::ForwardEpochMismatch,
                    ));
                }
                // Invariant: every accepted layer expires while its replay witness is still live.
                // The upper bound also prevents a malicious client from extending authenticated
                // validity beyond the finite replay-cache retention contract.
                if expires_at_ms <= received_at_ms
                    || expires_at_ms > received_at_ms.saturating_add(ONION_FORWARD_MAX_VALIDITY_MS)
                {
                    return Err(Error::OnionRouteError(
                        OnionRouteError::ForwardPayloadExpired,
                    ));
                }
                Ok(OnionCircuitEffect::Exit {
                    from,
                    circuit_id,
                    return_peer: from,
                    return_delegatee_public_key,
                    client,
                    forward_nonce,
                    forward_sequence,
                    payload,
                })
            }
        }
    }

    fn advance_backward(
        &self,
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        frame: OnionBackwardFrame,
        state: &mut OnionCircuitState,
    ) -> Result<OnionCircuitEffect> {
        purge_expired_return_hops(state, received_at_ms);
        let key = RelayReturnKey {
            circuit_id: frame.circuit_id,
            next_hop: from,
        };
        if let Some(entry) = state.relay_returns.get(&key).copied() {
            let previous_hop = entry.previous_hop;
            let previous_circuit_id = entry.previous_circuit_id;
            let previous_delegatee_public_key = entry.previous_delegatee_public_key;
            if let Some(entry) = Arc::make_mut(&mut state.relay_returns).get_mut(&key) {
                entry.expires_at_ms = received_at_ms.saturating_add(ONION_RELAY_RETURN_TTL_MS);
            }
            let encoded_message =
                encode_message(&OnionWireMessage::Backward(OnionBackwardFrame {
                    circuit_id: previous_circuit_id,
                    payload: frame.payload,
                }))?;
            return Ok(OnionCircuitEffect::SealAndSend {
                to: previous_hop,
                recipient: previous_delegatee_public_key,
                bucket,
                encoded_message,
                cell: OnionRelayedCell::Data {
                    circuit_id: previous_circuit_id,
                },
            });
        }

        Ok(OnionCircuitEffect::DecryptClient {
            from,
            circuit_id: frame.circuit_id,
            payload: frame.payload,
        })
    }

    fn validate_relay_forward(&self) -> Result<()> {
        if !self.capabilities.registers_relay() {
            return Err(Error::NoPermission);
        }
        // The route constructor bounds honest routes. Untrusted recursive layers are bounded by
        // the identity-independent crypto window and relay-return capacity instead of an exact
        // countdown that would disclose this relay's absolute position.
        Ok(())
    }
}

pub(super) fn remember_return_hop(
    state: &mut OnionCircuitState,
    max_relay_circuits: usize,
    ttl_ms: u128,
    edge: RelayReturnEdge,
    now_ms: u128,
) -> Result<()> {
    let RelayReturnEdge {
        key,
        previous_hop,
        previous_circuit_id,
        previous_delegatee_public_key,
        next_delegatee_public_key,
    } = edge;
    purge_expired_return_hops(state, now_ms);
    let table = Arc::make_mut(&mut state.relay_returns);
    let table_is_full = table.len() >= max_relay_circuits;
    let peer_table_is_full = table
        .values()
        .filter(|entry| entry.previous_hop == previous_hop)
        .count()
        >= max_relay_circuits_per_peer(max_relay_circuits);
    match table.entry(key) {
        Entry::Occupied(mut entry) => {
            if entry.get().previous_hop != previous_hop
                || entry.get().previous_circuit_id != previous_circuit_id
                || entry.get().previous_delegatee_public_key != previous_delegatee_public_key
            {
                return Err(Error::OnionRouteError(OnionRouteError::ReturnEdgeConflict));
            }
            entry.get_mut().expires_at_ms = now_ms.saturating_add(ttl_ms);
        }
        Entry::Vacant(entry) => {
            if table_is_full {
                return Err(Error::OnionRouteError(OnionRouteError::RelayTableFull));
            }
            if peer_table_is_full {
                return Err(Error::OnionRouteError(OnionRouteError::RelayPeerTableFull));
            }
            entry.insert(RelayReturnEntry {
                previous_hop,
                previous_circuit_id,
                previous_delegatee_public_key,
                next_delegatee_public_key,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            });
        }
    }
    Ok(())
}

/// A `Destroy` of the circuit known as `circuit_id` on the link to `to`, sealed for it.
fn destroy_toward(
    to: Did,
    recipient: PublicKey<33>,
    circuit_id: OnionCircuitId,
) -> Result<OnionCircuitEffect> {
    encode_message(&OnionWireMessage::Destroy { circuit_id }).map(|encoded_message| {
        OnionCircuitEffect::SealAndSend {
            to,
            recipient,
            bucket: OnionCellBucket::KiB4,
            encoded_message,
            cell: OnionRelayedCell::Destroy,
        }
    })
}

/// The `Destroy` that carries the end of the circuit of `(key, entry)` away from `from`: toward
/// the exit when `from` is the previous hop, toward the client otherwise.
fn destroy_away_from(
    from: Did,
    key: RelayReturnKey,
    entry: RelayReturnEntry,
) -> Result<OnionCircuitEffect> {
    if entry.previous_hop == from {
        destroy_toward(
            key.next_hop,
            entry.next_delegatee_public_key,
            key.circuit_id,
        )
    } else {
        destroy_toward(
            entry.previous_hop,
            entry.previous_delegatee_public_key,
            entry.previous_circuit_id,
        )
    }
}

/// Remove every return edge adjacent to the peer of `fact` and destroy its circuit: away from a
/// retired peer, toward both neighbours after a drop.
///
/// Post: no edge with `key.next_hop = p` or `previous_hop = p` remains, for `p = fact.peer()`;
/// the effects hold one `Destroy` per removed edge, to the neighbour other than `p`, after a
/// retirement, and two, one to each neighbour, after a drop.
fn destroy_link(state: &mut OnionCircuitState, fact: LinkFact) -> Vec<OnionCircuitEffect> {
    let peer = fact.peer();
    let adjacent = state
        .relay_returns
        .iter()
        .filter(|(key, entry)| key.next_hop == peer || entry.previous_hop == peer)
        .map(|(key, entry)| (*key, *entry))
        .collect::<Vec<_>>();
    if adjacent.is_empty() {
        return Vec::new();
    }
    let table = Arc::make_mut(&mut state.relay_returns);
    adjacent
        .into_iter()
        .flat_map(|(key, entry)| {
            table.remove(&key);
            match fact {
                LinkFact::Retired(_) => vec![destroy_away_from(peer, key, entry)],
                LinkFact::Dropped(_) => Vec::from(destroy_both_ways(key, entry)),
            }
        })
        .filter_map(sent_destroy)
        .collect()
}

/// The two `Destroy`s that end the circuit of `(key, entry)` at this relay: toward the exit and
/// toward the client.
fn destroy_both_ways(
    key: RelayReturnKey,
    entry: RelayReturnEntry,
) -> [Result<OnionCircuitEffect>; 2] {
    [
        destroy_toward(
            key.next_hop,
            entry.next_delegatee_public_key,
            key.circuit_id,
        ),
        destroy_toward(
            entry.previous_hop,
            entry.previous_delegatee_public_key,
            entry.previous_circuit_id,
        ),
    ]
}

/// The effect of a sealed `Destroy`, or none when it could not be encoded, logged.
fn sent_destroy(effect: Result<OnionCircuitEffect>) -> Option<OnionCircuitEffect> {
    effect
        .inspect_err(|error| tracing::debug!("drop onion destroy: {error}"))
        .ok()
}

/// Remove the return edge of the circuit `to` knows as `circuit_id`, whose cell this node's outbox
/// refused, and destroy the circuit toward both neighbours: the `Destroy` toward `to` is exempt
/// from the bound that refused the cell, so it follows the queued cells there.
fn refuse_circuit(
    state: &mut OnionCircuitState,
    to: Did,
    circuit_id: OnionCircuitId,
) -> Vec<OnionCircuitEffect> {
    let Some((key, entry)) = adjacent_edge(state, to, circuit_id) else {
        return Vec::new();
    };
    Arc::make_mut(&mut state.relay_returns).remove(&key);
    destroy_both_ways(key, entry)
        .into_iter()
        .filter_map(sent_destroy)
        .collect()
}

/// The return edge of the circuit `neighbour` knows as `circuit_id`, from either side.
fn adjacent_edge(
    state: &OnionCircuitState,
    neighbour: Did,
    circuit_id: OnionCircuitId,
) -> Option<(RelayReturnKey, RelayReturnEntry)> {
    let from_next = RelayReturnKey {
        circuit_id,
        next_hop: neighbour,
    };
    state
        .relay_returns
        .get(&from_next)
        .map(|entry| (from_next, *entry))
        .or_else(|| {
            state
                .relay_returns
                .iter()
                .find(|(_, entry)| {
                    entry.previous_hop == neighbour && entry.previous_circuit_id == circuit_id
                })
                .map(|(key, entry)| (*key, *entry))
        })
}

/// Remove the return edge of the circuit `from` knows as `circuit_id` and carry the `Destroy`
/// on to its other neighbour; with no such relay edge, this node is an endpoint of the circuit.
fn destroy_circuit(
    state: &mut OnionCircuitState,
    from: Did,
    circuit_id: OnionCircuitId,
) -> Result<OnionCircuitEffect> {
    match adjacent_edge(state, from, circuit_id) {
        Some((key, entry)) => {
            Arc::make_mut(&mut state.relay_returns).remove(&key);
            destroy_away_from(from, key, entry)
        }
        None => Ok(OnionCircuitEffect::Teardown(
            OnionCircuitTeardown::Circuit { from, circuit_id },
        )),
    }
}

/// Reserve at most one sixteenth of the global relay-return capacity for any
/// authenticated previous hop. Therefore one peer cannot exclude honest peers
/// while the global table has free entries.
const fn max_relay_circuits_per_peer(max_relay_circuits: usize) -> usize {
    max_relay_circuits.div_ceil(16)
}

fn purge_expired_return_hops(state: &mut OnionCircuitState, now_ms: u128) {
    if state
        .relay_returns
        .values()
        .any(|entry| entry.expires_at_ms <= now_ms)
    {
        Arc::make_mut(&mut state.relay_returns).retain(|_, entry| entry.expires_at_ms > now_ms);
    }
}
