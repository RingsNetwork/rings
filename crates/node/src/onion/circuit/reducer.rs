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
    pub(super) previous_session_public_key: PublicKey<33>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RelayReturnEntry {
    previous_hop: Did,
    previous_circuit_id: OnionCircuitId,
    previous_session_public_key: PublicKey<33>,
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
    #[cfg(test)]
    pub(super) fn relay_return_count(&self) -> usize {
        self.relay_returns.len()
    }

    #[cfg(test)]
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
        /// Next hop session key authenticated inside the current layer.
        recipient: PublicKey<33>,
        /// Padding class preserved across relay edges.
        bucket: OnionCellBucket,
        /// Encoded direction and frame protected by the cell AEAD.
        encoded_message: Bytes,
    },
    /// A forward frame reached the exit.
    Exit {
        /// Authenticated immediate sender.
        from: Did,
        /// Random circuit correlation id.
        circuit_id: OnionCircuitId,
        /// Immediate return peer.
        return_peer: Did,
        /// Immediate return peer session key for the first backward cell.
        return_session_public_key: PublicKey<33>,
        /// Client return key.
        client: OnionClientReturn,
        /// Replay token consumed by one-shot exit operations; stream frames use `forward_sequence`.
        forward_nonce: OnionForwardNonce,
        /// Monotonic client-to-exit sequence within this circuit.
        forward_sequence: OnionForwardSequence,
        /// Application payload.
        payload: OnionCircuitPayload,
    },
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

/// Pure state relation for onion circuits.
///
/// ```text
/// CellObserved(encrypted)      -> [DecryptCell]
/// CellReady(forward relay)     -> state' with return edge, [SealAndSend next]
/// CellReady(forward exit)      -> state, [Exit]
/// CellReady(backward match)    -> state' with refreshed edge, [SealAndSend previous]
/// CellReady(backward no match) -> state, [DecryptClient]
/// CellReady(cover)             -> state, []
/// ```
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
                if !self.capabilities.accepts_forward_layers() {
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
        if !self.capabilities.accepts_forward_layers() {
            return Err(Error::NoPermission);
        }
        match layer {
            OnionForwardLayer::Relay {
                next_hop,
                next_circuit_id,
                next_session_public_key,
                return_session_public_key,
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
                        previous_session_public_key: return_session_public_key,
                    },
                    received_at_ms,
                )?;
                encode_message(&OnionWireMessage::Forward(OnionForwardFrame {
                    circuit_id: next_circuit_id,
                    layer: inner,
                }))
                .map(|encoded_message| OnionCircuitEffect::SealAndSend {
                    to: next_hop,
                    recipient: next_session_public_key,
                    bucket,
                    encoded_message,
                })
            }
            OnionForwardLayer::Exit {
                client,
                return_session_public_key,
                expires_at_ms,
                forward_nonce,
                forward_sequence,
                payload,
            } => {
                if !self.capabilities.permits_exit_layer() {
                    return Err(Error::NoPermission);
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
                    return_session_public_key,
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
            let previous_session_public_key = entry.previous_session_public_key;
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
                recipient: previous_session_public_key,
                bucket,
                encoded_message,
            });
        }

        Ok(OnionCircuitEffect::DecryptClient {
            from,
            circuit_id: frame.circuit_id,
            payload: frame.payload,
        })
    }

    fn validate_relay_forward(&self) -> Result<()> {
        if !self.capabilities.permits_relay_layer() {
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
        previous_session_public_key,
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
                || entry.get().previous_session_public_key != previous_session_public_key
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
                previous_session_public_key,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            });
        }
    }
    Ok(())
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
