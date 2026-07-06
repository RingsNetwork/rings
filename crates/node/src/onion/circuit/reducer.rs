use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use serde::Deserialize;
use serde::Serialize;

use super::codec::encode_wire_message;
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
use super::MAX_ONION_CIRCUIT_HOPS;
use super::MAX_ONION_RELAY_CIRCUITS;
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
struct RelayReturnEntry {
    previous_hop: Did,
    previous_circuit_id: OnionCircuitId,
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
    relay_returns: BTreeMap<RelayReturnKey, RelayReturnEntry>,
}

impl OnionCircuitState {
    #[cfg(test)]
    pub(super) fn relay_return_count(&self) -> usize {
        self.relay_returns.len()
    }
}

/// Effects emitted by the route-aware circuit reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionCircuitEffect {
    /// Run forward-layer crypto at the shell boundary and re-inject the decoded layer.
    DecryptForward {
        /// Authenticated immediate sender.
        from: Did,
        /// Random circuit correlation id.
        circuit_id: OnionCircuitId,
        /// AEAD-encrypted layer for this hop.
        payload: AeadCiphertext,
    },
    /// Timestamp a backward frame at the shell boundary and re-inject it for pure reduction.
    TimestampBackward {
        /// Authenticated immediate sender.
        from: Did,
        /// Backward frame from an exit or relay.
        frame: OnionBackwardFrame,
    },
    /// Send a frame to the next hop.
    Send {
        /// Next hop.
        to: Did,
        /// Encoded frame.
        payload: Bytes,
    },
    /// A forward frame reached the exit.
    Exit {
        /// Authenticated immediate sender.
        from: Did,
        /// Random circuit correlation id.
        circuit_id: OnionCircuitId,
        /// Immediate return peer.
        return_peer: Did,
        /// Client return key.
        client: OnionClientReturn,
        /// Random per-frame nonce consumed by the exit adapter.
        forward_nonce: OnionForwardNonce,
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
/// ForwardObserved(encrypted)   -> [DecryptForward]
/// ForwardReady(relay layer)    -> state' with return edge, [Send next]
/// ForwardReady(exit layer)     -> state, [Exit]
/// BackwardObserved(encrypted)  -> [TimestampBackward]
/// BackwardReady(return match)  -> state' with refreshed edge, [Send previous]
/// BackwardReady(no match)      -> state, [DecryptClient]
/// ```
///
/// Law: replaying `apply(state, input)` with the same values returns the same `(state', effects)`.
/// Clocks, crypto, IO, and locks are represented by effects and live in the shell.
#[derive(Clone, Debug)]
pub(super) struct OnionCircuitReducer {
    capabilities: OnionCircuitCapabilities,
    max_hops: u8,
    max_relay_circuits: usize,
    relay_return_ttl_ms: u128,
}

impl OnionCircuitReducer {
    pub(super) const fn new(capabilities: OnionCircuitCapabilities) -> Self {
        Self {
            capabilities,
            max_hops: MAX_ONION_CIRCUIT_HOPS,
            max_relay_circuits: MAX_ONION_RELAY_CIRCUITS,
            relay_return_ttl_ms: ONION_RELAY_RETURN_TTL_MS,
        }
    }

    pub(super) fn apply(
        &self,
        state: &OnionCircuitState,
        input: OnionCircuitInput,
    ) -> Transition<OnionCircuitState, OnionCircuitEffect> {
        let mut state = state.clone();
        let effect = match input {
            OnionCircuitInput::ForwardObserved {
                from,
                circuit_id,
                layer,
            } => self.observe_forward(from, circuit_id, layer),
            OnionCircuitInput::BackwardObserved { from, frame } => {
                Ok(OnionCircuitEffect::TimestampBackward { from, frame })
            }
            OnionCircuitInput::ForwardReady {
                from,
                received_at_ms,
                circuit_id,
                layer,
            } => self.advance_forward(from, received_at_ms, circuit_id, layer, &mut state),
            OnionCircuitInput::BackwardReady {
                from,
                received_at_ms,
                frame,
            } => self.advance_backward(from, received_at_ms, frame, &mut state),
        };

        match effect {
            Ok(effect) => Transition::with(state, vec![effect]),
            Err(error) => {
                tracing::debug!("drop onion circuit message: {error}");
                Transition::pure(state)
            }
        }
    }

    fn observe_forward(
        &self,
        from: Did,
        circuit_id: OnionCircuitId,
        layer: AeadCiphertext,
    ) -> Result<OnionCircuitEffect> {
        if !self.capabilities.accepts_forward_layers() {
            return Err(Error::NoPermission);
        }
        Ok(OnionCircuitEffect::DecryptForward {
            from,
            circuit_id,
            payload: layer,
        })
    }

    fn advance_forward(
        &self,
        from: Did,
        received_at_ms: u128,
        circuit_id: OnionCircuitId,
        layer: OnionForwardLayer,
        state: &mut OnionCircuitState,
    ) -> Result<OnionCircuitEffect> {
        match layer {
            OnionForwardLayer::Relay {
                next_hop,
                next_circuit_id,
                remaining_hops,
                inner,
            } => {
                self.validate_relay_forward(remaining_hops)?;
                remember_return_hop(
                    state,
                    self.max_relay_circuits,
                    self.relay_return_ttl_ms,
                    RelayReturnKey {
                        circuit_id: next_circuit_id,
                        next_hop,
                    },
                    from,
                    circuit_id,
                    received_at_ms,
                )?;
                encode_wire_message(OnionWireMessage::Forward(OnionForwardFrame {
                    circuit_id: next_circuit_id,
                    layer: inner,
                }))
                .map(|payload| OnionCircuitEffect::Send {
                    to: next_hop,
                    payload,
                })
            }
            OnionForwardLayer::Exit {
                client,
                expires_at_ms,
                forward_nonce,
                payload,
            } => {
                if !self.capabilities.permits_exit_layer() {
                    return Err(Error::NoPermission);
                }
                if expires_at_ms <= received_at_ms {
                    return Err(Error::OnionRouteError(
                        OnionRouteError::ForwardPayloadExpired,
                    ));
                }
                Ok(OnionCircuitEffect::Exit {
                    from,
                    circuit_id,
                    return_peer: from,
                    client,
                    forward_nonce,
                    payload,
                })
            }
        }
    }

    fn advance_backward(
        &self,
        from: Did,
        received_at_ms: u128,
        frame: OnionBackwardFrame,
        state: &mut OnionCircuitState,
    ) -> Result<OnionCircuitEffect> {
        purge_expired_return_hops(state, received_at_ms);
        let key = RelayReturnKey {
            circuit_id: frame.circuit_id,
            next_hop: from,
        };
        if let Some(entry) = state.relay_returns.get_mut(&key) {
            let previous_hop = entry.previous_hop;
            let previous_circuit_id = entry.previous_circuit_id;
            entry.expires_at_ms = received_at_ms.saturating_add(self.relay_return_ttl_ms);
            let payload = encode_wire_message(OnionWireMessage::Backward(OnionBackwardFrame {
                circuit_id: previous_circuit_id,
                payload: frame.payload,
            }))?;
            return Ok(OnionCircuitEffect::Send {
                to: previous_hop,
                payload,
            });
        }

        Ok(OnionCircuitEffect::DecryptClient {
            from,
            circuit_id: frame.circuit_id,
            payload: frame.payload,
        })
    }

    fn validate_relay_forward(&self, remaining_hops: u8) -> Result<()> {
        if !self.capabilities.permits_relay_layer() {
            return Err(Error::NoPermission);
        }
        // Pre: `remaining_hops` is authenticated inside this decrypted layer but authored by the
        // client route constructor. A relay can bound the next layer budget; it cannot prove the
        // hidden global route length without breaking onion path privacy.
        if remaining_hops == 0 || remaining_hops > self.max_hops {
            return Err(Error::OnionRouteError(
                OnionRouteError::InvalidRelayHopBudget {
                    remaining_hops,
                    max_hops: self.max_hops,
                },
            ));
        }
        Ok(())
    }
}

pub(super) fn remember_return_hop(
    state: &mut OnionCircuitState,
    max_relay_circuits: usize,
    ttl_ms: u128,
    key: RelayReturnKey,
    previous_hop: Did,
    previous_circuit_id: OnionCircuitId,
    now_ms: u128,
) -> Result<()> {
    purge_expired_return_hops(state, now_ms);
    let table_is_full = state.relay_returns.len() >= max_relay_circuits;
    match state.relay_returns.entry(key) {
        Entry::Occupied(mut entry) => {
            if entry.get().previous_hop != previous_hop
                || entry.get().previous_circuit_id != previous_circuit_id
            {
                return Err(Error::OnionRouteError(OnionRouteError::ReturnEdgeConflict));
            }
            entry.get_mut().expires_at_ms = now_ms.saturating_add(ttl_ms);
        }
        Entry::Vacant(entry) => {
            if table_is_full {
                return Err(Error::OnionRouteError(OnionRouteError::RelayTableFull));
            }
            entry.insert(RelayReturnEntry {
                previous_hop,
                previous_circuit_id,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            });
        }
    }
    Ok(())
}

fn purge_expired_return_hops(state: &mut OnionCircuitState, now_ms: u128) {
    state
        .relay_returns
        .retain(|_, entry| entry.expires_at_ms > now_ms);
}
