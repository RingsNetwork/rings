use std::iter;
#[cfg(rings_native)]
use std::sync::atomic::AtomicU64;
#[cfg(rings_native)]
use std::sync::atomic::Ordering;

use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::SigningDomain;
use rings_core::utils::get_epoch_ms;
use serde::Serialize;

use super::cell::seal_message;
use super::codec::OnionWireMessage;
use super::OnionAuthenticatedPayload;
use super::OnionBackwardFrame;
use super::OnionBackwardNonce;
use super::OnionBackwardPath;
use super::OnionBackwardSequence;
use super::OnionCircuitId;
use super::OnionCircuitPayload;
use super::OnionClientReturn;
use super::OnionForwardFrame;
use super::OnionForwardLayer;
use super::OnionForwardNonce;
use super::OnionForwardSequence;
use super::OnionLink;
use super::OnionLinkSender;
use super::OnionReturnId;
use super::OnionVerifiedPayload;
use super::ONION_AEAD_NAMESPACE;
use super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use super::ONION_FORWARD_PAYLOAD_TTL_MS;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::pipeline::OnionPipeline;
use crate::onion::signature::OnionSymbolRole;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitEpoch;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionRouteHop;
#[cfg(rings_native)]
use crate::onion::OnionServiceName;

/// Message family of the exit's backward-payload signature.
const ONION_BACKWARD_PAYLOAD_DOMAIN_TAG: DomainTag =
    domain_tag!("rings-node:onion-backward-payload");

/// Encode the first forward frame for `route`.
///
/// Pre: `payload.service` names the same service that selected `route`.
/// Post: the encrypted exit layer cannot carry a payload for a service different from the selected
/// exit descriptor service.
pub fn encode_initial_forward(
    client: OnionClientReturn,
    route: &OnionRoute,
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
) -> Result<(Did, Bytes)> {
    encode_initial_forward_link(client, route, circuit_id, payload)
        .map(|(link, payload)| (link.peer, payload))
}

/// Encode the first forward frame while preserving its authenticated link as one value.
pub(crate) fn encode_initial_forward_link(
    client: OnionClientReturn,
    route: &OnionRoute,
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
) -> Result<(OnionLink, Bytes)> {
    let pipeline = route.pipeline(payload)?;
    let first = route_first_link(route)?;
    let layer = build_forward_layers(
        client,
        &pipeline,
        route.encryption_hops(),
        route.exit().process_epoch,
        circuit_id,
        OnionForwardSequence::FIRST,
    )?;
    let frame = OnionForwardFrame { circuit_id, layer };
    seal_message(&OnionWireMessage::Forward(frame), first.recipient, None)
        .map(|payload| (first, payload))
}

/// Stable edge-id plan for a long-lived onion circuit.
///
/// Invariant: `edge_circuit_ids.len() == route.encryption_hops().len()` and
/// `first_circuit_id == edge_circuit_ids[0]`. Reusing one path for every payload in a stream
/// preserves the exit-side stream key and refreshes the same relay return edges.
#[cfg(rings_native)]
#[derive(Debug)]
pub(crate) struct OnionCircuitPath {
    route: OnionRoute,
    first_circuit_id: OnionCircuitId,
    edge_circuit_ids: Vec<OnionCircuitId>,
    next_forward_sequence: AtomicU64,
}

#[cfg(rings_native)]
impl OnionCircuitPath {
    /// Build a stable circuit path for one route.
    pub(crate) fn new(route: OnionRoute, first_circuit_id: OnionCircuitId) -> Result<Self> {
        let edge_circuit_ids = edge_circuit_ids(route.encryption_hops().len(), first_circuit_id)?;
        Ok(Self {
            route,
            first_circuit_id,
            edge_circuit_ids,
            next_forward_sequence: AtomicU64::new(0),
        })
    }

    /// Encode one forward payload over this stable path.
    pub(crate) fn encode_forward(
        &self,
        client: OnionClientReturn,
        payload: OnionCircuitPayload,
    ) -> Result<(OnionLink, Bytes)> {
        let pipeline = self.route.pipeline(payload)?;
        let sequence = self
            .next_forward_sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map(OnionForwardSequence::new)
            .map_err(|_| Error::OnionRouteError(OnionRouteError::SequenceExhausted))?;
        let first = route_first_link(&self.route)?;
        let layer = build_forward_layers_with_ids(
            client,
            &pipeline,
            self.route.encryption_hops(),
            self.route.exit().process_epoch,
            self.edge_circuit_ids.as_slice(),
            sequence,
        )?;
        let frame = OnionForwardFrame {
            circuit_id: self.first_circuit_id,
            layer,
        };
        seal_message(&OnionWireMessage::Forward(frame), first.recipient, None)
            .map(|payload| (first, payload))
    }

    /// Return the canonical service selected by this path's route.
    pub(crate) fn service_name(&self) -> &OnionServiceName {
        self.route.service_name()
    }
}

/// Return the first overlay hop of a route that was validated at construction.
///
/// Pre: `route` was built by the route module constructor.
/// Post: result is the first encrypted hop DID used by forward encoding.
pub fn route_first_hop(route: &OnionRoute) -> Result<Did> {
    route_first_link(route).map(|link| link.peer)
}

/// Return the first overlay peer and hop encryption recipient as one inseparable link value.
pub(crate) fn route_first_link(route: &OnionRoute) -> Result<OnionLink> {
    route
        .encryption_hops()
        .first()
        .map(|hop| OnionLink::new(hop.did, hop.delegatee_public_key))
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::RouteHasNoHops))
}

/// Send a response payload back to the immediate return peer.
pub async fn send_backward(
    link_sender: &OnionLinkSender,
    scope: &Scope,
    signer: MessageSigner<&DelegateeKey>,
    path: OnionBackwardPath,
    sequence: OnionBackwardSequence,
    payload: OnionCircuitPayload,
) -> Result<()> {
    let frame = OnionBackwardFrame {
        circuit_id: path.circuit_id,
        payload: encrypt_client_payload_at_sequence(
            path.client.return_id,
            sequence,
            payload,
            path.client.delegatee_public_key,
            signer,
        )?,
    };
    let payload = seal_message(
        &OnionWireMessage::Backward(frame),
        path.return_delegatee_public_key,
        None,
    )?;
    link_sender
        .send_sealed(
            scope.clone(),
            OnionLink::new(path.return_peer, path.return_delegatee_public_key),
            payload,
        )
        .await
}

/// Seal `pipeline` over freshly allocated edge ids starting at `first_circuit_id`.
fn build_forward_layers(
    client: OnionClientReturn,
    pipeline: &OnionPipeline,
    hops: &[OnionRouteHop],
    process_epoch: OnionExitEpoch,
    first_circuit_id: OnionCircuitId,
    sequence: OnionForwardSequence,
) -> Result<AeadCiphertext> {
    let circuit_ids = edge_circuit_ids(hops.len(), first_circuit_id)?;
    build_forward_layers_with_ids(
        client,
        pipeline,
        hops,
        process_epoch,
        circuit_ids.as_slice(),
        sequence,
    )
}

/// One pipeline position bound to its hop: the node, its inbound edge id, and the key it answers
/// backward to.
#[derive(Clone, Copy)]
struct OnionForwardPosition {
    /// Node evaluating this position.
    hop: OnionRouteHop,
    /// Edge-local circuit id of the edge into `hop`.
    circuit_id: OnionCircuitId,
    /// Delegatee key of the previous hop, or of the client at position zero.
    return_delegatee_public_key: PublicKey<33>,
}

/// Seal `pipeline` over `hops` as nested forward layers, one layer per position.
///
/// A right fold over the positions `pᵢ = (hopᵢ, idᵢ, backᵢ)` zipped with the applications
/// `(fᵢ, āᵢ)` of `pipeline`, where `back₀` is the client key and `backᵢ = hopᵢ₋₁.key`:
///
/// ```text
/// layer = foldr step base (zip [p₀ … pₙ₋₂] [(f₀, ā₀) … (fₙ₋₂, āₙ₋₂)])
///
///   base                       = seal(hopₙ₋₁, idₙ₋₁, Exit { backₙ₋₁, (fₙ₋₁, āₙ₋₁), … })
///   step (pᵢ, relay) (inner, q) = (seal(hopᵢ, idᵢ, Relay { q.hop, q.id, backᵢ, inner }), pᵢ)
///   step (pᵢ, f)     _          = ⊥   f world-facing but not last
/// ```
///
/// The world-facing layer is sealed first and each relay wraps its successor's layer, so the
/// result is addressed to `hop₀`. Each layer plaintext is one pinned `OnionForwardLayer` shape.
fn build_forward_layers_with_ids(
    client: OnionClientReturn,
    pipeline: &OnionPipeline,
    hops: &[OnionRouteHop],
    process_epoch: OnionExitEpoch,
    circuit_ids: &[OnionCircuitId],
    sequence: OnionForwardSequence,
) -> Result<AeadCiphertext> {
    if hops.len() != circuit_ids.len() {
        return Err(Error::OnionRouteError(
            OnionRouteError::CircuitPathLengthMismatch {
                hop_count: hops.len(),
                edge_count: circuit_ids.len(),
            },
        ));
    }
    if pipeline.applications().len() != hops.len() {
        return Err(Error::OnionRouteError(
            OnionRouteError::PipelineLengthMismatch {
                hop_count: hops.len(),
                application_count: pipeline.applications().len(),
            },
        ));
    }
    let (relay_applications, world_facing) = pipeline.closed()?;
    let return_keys = iter::once(client.delegatee_public_key)
        .chain(hops.iter().map(|hop| hop.delegatee_public_key));
    let positions = hops
        .iter()
        .copied()
        .zip(circuit_ids.iter().copied())
        .zip(return_keys)
        .map(
            |((hop, circuit_id), return_delegatee_public_key)| OnionForwardPosition {
                hop,
                circuit_id,
                return_delegatee_public_key,
            },
        )
        .collect::<Vec<_>>();
    let Some((exit, relays)) = positions.split_last() else {
        return Err(Error::OnionRouteError(OnionRouteError::RouteHasNoHops));
    };
    let base = encrypt_forward_layer(
        exit.circuit_id,
        OnionForwardLayer::Exit {
            process_epoch,
            client,
            return_delegatee_public_key: exit.return_delegatee_public_key,
            expires_at_ms: quantized_forward_expiry(get_epoch_ms()),
            forward_nonce: OnionForwardNonce::random(),
            forward_sequence: sequence,
            payload: OnionCircuitPayload::new(
                world_facing.symbol().clone(),
                world_facing.args().clone(),
            ),
        },
        exit.hop.delegatee_public_key,
    )?;
    relays
        .iter()
        .zip(relay_applications)
        .rev()
        .try_fold(
            (base, exit),
            |(inner, next), (position, application)| match application.spec().role() {
                OnionSymbolRole::Identity => encrypt_forward_layer(
                    position.circuit_id,
                    OnionForwardLayer::Relay {
                        next_hop: next.hop.did,
                        next_circuit_id: next.circuit_id,
                        next_delegatee_public_key: next.hop.delegatee_public_key,
                        return_delegatee_public_key: position.return_delegatee_public_key,
                        inner,
                    },
                    position.hop.delegatee_public_key,
                )
                .map(|layer| (layer, position)),
                OnionSymbolRole::WorldFacing => Err(Error::OnionRouteError(
                    OnionRouteError::WorldFacingSymbolNotLast {
                        symbol: application.symbol().as_str().to_string(),
                    },
                )),
            },
        )
        .map(|(layer, _)| layer)
}

/// Quantize authenticated expiry to a coarse wall-clock boundary.
///
/// Law: every timestamp in one quantum maps to the same advertised boundary, so exit validation
/// retains a finite TTL while the encrypted layer does not preserve byte-accurate client clock
/// skew. Saturation remains fail-closed at the maximum representable instant.
fn quantized_forward_expiry(now_ms: u128) -> u128 {
    let deadline = now_ms.saturating_add(ONION_FORWARD_PAYLOAD_TTL_MS);
    deadline
        .saturating_add(ONION_FORWARD_EXPIRY_QUANTUM_MS - 1)
        .checked_div(ONION_FORWARD_EXPIRY_QUANTUM_MS)
        .and_then(|bucket| bucket.checked_mul(ONION_FORWARD_EXPIRY_QUANTUM_MS))
        .unwrap_or(u128::MAX)
}

fn edge_circuit_ids(
    hop_count: usize,
    first_circuit_id: OnionCircuitId,
) -> Result<Vec<OnionCircuitId>> {
    edge_circuit_ids_with(hop_count, first_circuit_id, OnionCircuitId::random)
}

pub(super) fn edge_circuit_ids_with(
    hop_count: usize,
    first_circuit_id: OnionCircuitId,
    mut next_id: impl FnMut() -> OnionCircuitId,
) -> Result<Vec<OnionCircuitId>> {
    const MAX_ALLOCATION_ATTEMPTS_PER_EDGE: usize = 16;
    if hop_count == 0 || hop_count > usize::from(super::MAX_ONION_CIRCUIT_HOPS) {
        return Err(Error::OnionRouteError(
            OnionRouteError::HopCountOutOfBounds {
                hop_count,
                max_hops: super::MAX_ONION_CIRCUIT_HOPS,
            },
        ));
    }
    let mut ids = Vec::with_capacity(hop_count);
    ids.push(first_circuit_id);
    while ids.len() < hop_count {
        let next = (0..MAX_ALLOCATION_ATTEMPTS_PER_EDGE)
            .map(|_| next_id())
            .find(|candidate| !ids.contains(candidate))
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::CircuitIdAllocationFailed))?;
        ids.push(next);
    }
    Ok(ids)
}

fn encrypt_forward_layer(
    circuit_id: OnionCircuitId,
    layer: OnionForwardLayer,
    recipient: PublicKey<33>,
) -> Result<AeadCiphertext> {
    let plaintext = rings_codec::serialize(&layer).map_err(|_| Error::EncodeError)?;
    let aad = onion_aead_context(OnionAeadDirection::Forward, circuit_id)?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

pub(super) fn decrypt_forward_layer(
    delegatee_key: &DelegateeKey,
    circuit_id: OnionCircuitId,
    sealed: &AeadCiphertext,
) -> Result<OnionForwardLayer> {
    let aad = onion_aead_context(OnionAeadDirection::Forward, circuit_id)?;
    let plaintext = delegatee_key
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    rings_codec::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

#[cfg(all(test, rings_native))]
pub(super) fn encrypt_client_payload(
    return_id: OnionReturnId,
    payload: OnionCircuitPayload,
    recipient: PublicKey<33>,
    signer: MessageSigner<&DelegateeKey>,
) -> Result<AeadCiphertext> {
    encrypt_client_payload_at_sequence(
        return_id,
        OnionBackwardSequence::FIRST,
        payload,
        recipient,
        signer,
    )
}

pub(super) fn encrypt_client_payload_at_sequence(
    return_id: OnionReturnId,
    sequence: OnionBackwardSequence,
    payload: OnionCircuitPayload,
    recipient: PublicKey<33>,
    signer: MessageSigner<&DelegateeKey>,
) -> Result<AeadCiphertext> {
    let authenticated =
        OnionAuthenticatedPayload::new_signed_at_sequence(return_id, sequence, payload, signer)?;
    let plaintext = rings_codec::serialize(&authenticated).map_err(|_| Error::EncodeError)?;
    // The outer hop cell authenticates the edge-local circuit and direction. This inner payload
    // deliberately remains stable while relays rewrite edge ids; its signed transcript binds the
    // client-only return id, nonce, monotonic sequence, exit delegatee key, and payload bytes.
    let aad = backward_aead_context()?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

pub(super) fn decrypt_client_payload(
    delegatee_key: &DelegateeKey,
    sealed: &AeadCiphertext,
) -> Result<OnionAuthenticatedPayload> {
    let aad = backward_aead_context()?;
    let plaintext = delegatee_key
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    rings_codec::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

impl OnionAuthenticatedPayload {
    /// Sign one backward payload with a fresh replay nonce.
    pub fn new_signed(
        return_id: OnionReturnId,
        payload: OnionCircuitPayload,
        signer: MessageSigner<&DelegateeKey>,
    ) -> Result<Self> {
        Self::new_signed_at_sequence(return_id, OnionBackwardSequence::FIRST, payload, signer)
    }

    /// Sign one backward payload at a caller-owned monotonic circuit sequence.
    pub fn new_signed_at_sequence(
        return_id: OnionReturnId,
        sequence: OnionBackwardSequence,
        payload: OnionCircuitPayload,
        signer: MessageSigner<&DelegateeKey>,
    ) -> Result<Self> {
        let nonce = OnionBackwardNonce::random();
        let authentication = signer
            .sign(
                ONION_BACKWARD_PAYLOAD_DOMAIN_TAG,
                &backward_payload_authentication_data(
                    return_id,
                    nonce,
                    sequence,
                    signer.delegatee_public_key(),
                    &payload,
                )?,
            )
            .map_err(Error::CoreError)?;
        Ok(Self {
            return_id,
            nonce,
            sequence,
            authentication,
            payload,
        })
    }

    /// Verify that a client-decrypted backward payload was signed by the selected exit session.
    ///
    /// Invariant: accepted backward payloads satisfy all three identity equalities:
    /// signer account DID equals descriptor DID, signer account public key equals descriptor public
    /// key, and signer session DID equals the descriptor session encryption key DID. The signed
    /// transcript also binds the client/exit return id, per-frame nonce, exit session public key,
    /// and payload, and its signing domain binds the receiver's overlay `network_id`, never a
    /// value carried by the exit.
    pub fn into_verified_payload(
        self,
        return_id: OnionReturnId,
        expected_exit: &OnionExitDescriptor,
        network_id: u32,
    ) -> Result<OnionVerifiedPayload> {
        if self.return_id != return_id {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardReturnIdMismatch,
            ));
        }
        let signer = &self.authentication.delegation;
        if signer.delegator_did() != expected_exit.did {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardSignerMismatch,
            ));
        }
        let public_key = signer
            .delegator_verification_pubkey()
            .map_err(Error::CoreError)?;
        if public_key != expected_exit.public_key {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardAccountKeyMismatch,
            ));
        }
        if signer.delegatee_did() != Did::from(expected_exit.delegatee_public_key.address()) {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardSessionKeyMismatch,
            ));
        }
        let data = backward_payload_authentication_data(
            return_id,
            self.nonce,
            self.sequence,
            expected_exit.delegatee_public_key,
            &self.payload,
        )?;
        let domain = SigningDomain::new(ONION_BACKWARD_PAYLOAD_DOMAIN_TAG, network_id);
        if !self.authentication.verify_live(domain, &data) {
            return Err(Error::OnionRouteError(
                OnionRouteError::InvalidBackwardSignature,
            ));
        }
        Ok(OnionVerifiedPayload {
            return_id: self.return_id,
            nonce: self.nonce,
            sequence: self.sequence,
            payload: self.payload,
        })
    }
}

#[derive(Serialize)]
struct OnionAeadContext {
    namespace: &'static str,
    direction: OnionAeadDirection,
    circuit_id: OnionCircuitId,
}

#[derive(Serialize)]
struct OnionBackwardAuthenticationData<'a> {
    namespace: &'static str,
    direction: OnionAeadDirection,
    return_id: OnionReturnId,
    nonce: OnionBackwardNonce,
    sequence: OnionBackwardSequence,
    exit_delegatee_public_key: PublicKey<33>,
    payload: &'a OnionCircuitPayload,
}

#[derive(Clone, Copy, Serialize)]
pub(super) enum OnionAeadDirection {
    Forward,
    Backward,
}

fn onion_aead_context(
    direction: OnionAeadDirection,
    circuit_id: OnionCircuitId,
) -> Result<Vec<u8>> {
    rings_codec::serialize(&OnionAeadContext {
        namespace: ONION_AEAD_NAMESPACE,
        direction,
        circuit_id,
    })
    .map_err(|_| Error::EncodeError)
}

fn backward_aead_context() -> Result<Vec<u8>> {
    rings_codec::serialize(&OnionAeadDirectionContext {
        namespace: ONION_AEAD_NAMESPACE,
        direction: OnionAeadDirection::Backward,
    })
    .map_err(|_| Error::EncodeError)
}

fn backward_payload_authentication_data(
    return_id: OnionReturnId,
    nonce: OnionBackwardNonce,
    sequence: OnionBackwardSequence,
    exit_delegatee_public_key: PublicKey<33>,
    payload: &OnionCircuitPayload,
) -> Result<Vec<u8>> {
    rings_codec::serialize(&OnionBackwardAuthenticationData {
        namespace: ONION_AEAD_NAMESPACE,
        direction: OnionAeadDirection::Backward,
        return_id,
        nonce,
        sequence,
        exit_delegatee_public_key,
        payload,
    })
    .map_err(|_| Error::EncodeError)
}

#[derive(Serialize)]
struct OnionAeadDirectionContext {
    namespace: &'static str,
    direction: OnionAeadDirection,
}
