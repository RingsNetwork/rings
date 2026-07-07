use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::message::MessageVerification;
use rings_core::session::SessionSk;
use rings_core::utils::get_epoch_ms;
use serde::Serialize;

use super::codec::encode_wire_message;
use super::codec::OnionWireMessage;
use super::OnionAuthenticatedPayload;
use super::OnionBackwardFrame;
use super::OnionBackwardNonce;
use super::OnionCircuitId;
use super::OnionCircuitPayload;
use super::OnionClientReturn;
use super::OnionForwardFrame;
use super::OnionForwardLayer;
use super::OnionForwardNonce;
use super::OnionReturnId;
use super::OnionVerifiedPayload;
use super::ONION_AEAD_NAMESPACE;
use super::ONION_FORWARD_PAYLOAD_TTL_MS;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionRouteHop;
#[cfg(feature = "node")]
use crate::onion::OnionServiceName;

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
    validate_route_payload_service(route, &payload)?;
    let first = route_first_hop(route)?;
    let layer = build_forward_layers(client, route.encryption_hops(), circuit_id, payload)?;
    let frame = OnionForwardFrame { circuit_id, layer };
    encode_wire_message(OnionWireMessage::Forward(frame)).map(|payload| (first, payload))
}

/// Stable edge-id plan for a long-lived onion circuit.
///
/// Invariant: `edge_circuit_ids.len() == route.encryption_hops().len()` and
/// `first_circuit_id == edge_circuit_ids[0]`. Reusing one path for every payload in a stream
/// preserves the exit-side stream key and refreshes the same relay return edges.
#[cfg(feature = "node")]
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct OnionCircuitPath {
    route: OnionRoute,
    first_circuit_id: OnionCircuitId,
    edge_circuit_ids: Vec<OnionCircuitId>,
}

#[cfg(feature = "node")]
impl OnionCircuitPath {
    /// Build a stable circuit path for one route.
    pub(crate) fn new(route: OnionRoute, first_circuit_id: OnionCircuitId) -> Result<Self> {
        let edge_circuit_ids = edge_circuit_ids(route.encryption_hops().len(), first_circuit_id)?;
        Ok(Self {
            route,
            first_circuit_id,
            edge_circuit_ids,
        })
    }

    /// Encode one forward payload over this stable path.
    pub(crate) fn encode_forward(
        &self,
        client: OnionClientReturn,
        payload: OnionCircuitPayload,
    ) -> Result<(Did, Bytes)> {
        validate_route_payload_service(&self.route, &payload)?;
        let first = route_first_hop(&self.route)?;
        let layer = build_forward_layers_with_ids(
            client,
            self.route.encryption_hops(),
            self.edge_circuit_ids.as_slice(),
            payload,
        )?;
        let frame = OnionForwardFrame {
            circuit_id: self.first_circuit_id,
            layer,
        };
        encode_wire_message(OnionWireMessage::Forward(frame)).map(|payload| (first, payload))
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
    route
        .encryption_hops()
        .first()
        .map(|hop| hop.did)
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::RouteHasNoHops))
}

/// Send a response payload back to the immediate return peer.
pub async fn send_backward(
    scope: &Scope,
    signer: &SessionSk,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    client: OnionClientReturn,
    payload: OnionCircuitPayload,
) -> Result<()> {
    let frame = OnionBackwardFrame {
        circuit_id,
        payload: encrypt_client_payload(
            client.return_id,
            payload,
            client.session_public_key,
            signer,
        )?,
    };
    let payload = encode_wire_message(OnionWireMessage::Backward(frame))?;
    scope.send(return_peer, payload).await
}

fn build_forward_layers(
    client: OnionClientReturn,
    hops: &[OnionRouteHop],
    first_circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
) -> Result<AeadCiphertext> {
    let circuit_ids = edge_circuit_ids(hops.len(), first_circuit_id)?;
    build_forward_layers_with_ids(client, hops, circuit_ids.as_slice(), payload)
}

fn build_forward_layers_with_ids(
    client: OnionClientReturn,
    hops: &[OnionRouteHop],
    circuit_ids: &[OnionCircuitId],
    payload: OnionCircuitPayload,
) -> Result<AeadCiphertext> {
    let Some(exit) = hops.last().copied() else {
        return Err(Error::OnionRouteError(OnionRouteError::RouteHasNoHops));
    };
    if hops.len() != circuit_ids.len() {
        return Err(Error::OnionRouteError(
            OnionRouteError::CircuitPathLengthMismatch {
                hop_count: hops.len(),
                edge_count: circuit_ids.len(),
            },
        ));
    }
    let expires_at_ms = get_epoch_ms().saturating_add(ONION_FORWARD_PAYLOAD_TTL_MS);
    let exit_circuit_id = *circuit_ids
        .last()
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::RouteHasNoHops))?;
    let mut layer = encrypt_forward_layer(
        exit_circuit_id,
        OnionForwardLayer::Exit {
            client,
            expires_at_ms,
            forward_nonce: OnionForwardNonce::random(),
            payload,
        },
        exit.session_public_key,
    )?;

    for (index, hop) in hops.iter().copied().enumerate().rev().skip(1) {
        let next_index = index.saturating_add(1);
        let next_hop = hops
            .get(next_index)
            .map(|next| next.did)
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::MissingNextHop))?;
        let current_circuit_id = circuit_ids
            .get(index)
            .copied()
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::MissingNextHop))?;
        let next_circuit_id = circuit_ids
            .get(next_index)
            .copied()
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::MissingNextHop))?;
        let remaining_hops = u8::try_from(hops.len().saturating_sub(index + 1)).map_err(|_| {
            Error::OnionRouteError(OnionRouteError::HopCountOutOfBounds {
                hop_count: hops.len(),
                max_hops: super::MAX_ONION_CIRCUIT_HOPS,
            })
        })?;
        layer = encrypt_forward_layer(
            current_circuit_id,
            OnionForwardLayer::Relay {
                next_hop,
                next_circuit_id,
                remaining_hops,
                inner: layer,
            },
            hop.session_public_key,
        )?;
    }
    Ok(layer)
}

fn edge_circuit_ids(
    hop_count: usize,
    first_circuit_id: OnionCircuitId,
) -> Result<Vec<OnionCircuitId>> {
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
        ids.push(OnionCircuitId::random());
    }
    Ok(ids)
}

fn encrypt_forward_layer(
    circuit_id: OnionCircuitId,
    layer: OnionForwardLayer,
    recipient: PublicKey<33>,
) -> Result<AeadCiphertext> {
    let plaintext = bincode::serialize(&layer).map_err(|_| Error::EncodeError)?;
    let aad = onion_aead_context(OnionAeadDirection::Forward, circuit_id)?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

pub(super) fn decrypt_forward_layer(
    session_sk: &SessionSk,
    circuit_id: OnionCircuitId,
    sealed: &AeadCiphertext,
) -> Result<OnionForwardLayer> {
    let aad = onion_aead_context(OnionAeadDirection::Forward, circuit_id)?;
    let plaintext = session_sk
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    bincode::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

pub(super) fn encrypt_client_payload(
    return_id: OnionReturnId,
    payload: OnionCircuitPayload,
    recipient: PublicKey<33>,
    signer: &SessionSk,
) -> Result<AeadCiphertext> {
    let authenticated = OnionAuthenticatedPayload::new_signed(return_id, payload, signer)?;
    let plaintext = bincode::serialize(&authenticated).map_err(|_| Error::EncodeError)?;
    let aad = backward_aead_context()?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

pub(super) fn decrypt_client_payload(
    session_sk: &SessionSk,
    sealed: &AeadCiphertext,
) -> Result<OnionAuthenticatedPayload> {
    let aad = backward_aead_context()?;
    let plaintext = session_sk
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    bincode::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

impl OnionAuthenticatedPayload {
    /// Sign one backward payload with a fresh replay nonce.
    pub fn new_signed(
        return_id: OnionReturnId,
        payload: OnionCircuitPayload,
        signer: &SessionSk,
    ) -> Result<Self> {
        let nonce = OnionBackwardNonce::random();
        let authentication = MessageVerification::new(
            &backward_payload_authentication_data(
                return_id,
                nonce,
                signer.session_public_key(),
                &payload,
            )?,
            signer,
        )
        .map_err(Error::CoreError)?;
        Ok(Self {
            return_id,
            nonce,
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
    /// and payload.
    pub fn into_verified_payload(
        self,
        return_id: OnionReturnId,
        expected_exit: &OnionExitDescriptor,
    ) -> Result<OnionVerifiedPayload> {
        if self.return_id != return_id {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardReturnIdMismatch,
            ));
        }
        let signer = &self.authentication.session;
        if signer.account_did() != expected_exit.did {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardSignerMismatch,
            ));
        }
        let public_key = signer
            .account_verification_pubkey()
            .map_err(Error::CoreError)?;
        if public_key != expected_exit.public_key {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardAccountKeyMismatch,
            ));
        }
        if signer.session_did() != Did::from(expected_exit.session_public_key.address()) {
            return Err(Error::OnionRouteError(
                OnionRouteError::BackwardSessionKeyMismatch,
            ));
        }
        let data = backward_payload_authentication_data(
            return_id,
            self.nonce,
            expected_exit.session_public_key,
            &self.payload,
        )?;
        if !self.authentication.verify_unexpired(&data) {
            return Err(Error::OnionRouteError(
                OnionRouteError::InvalidBackwardSignature,
            ));
        }
        Ok(OnionVerifiedPayload {
            return_id: self.return_id,
            nonce: self.nonce,
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
    exit_session_public_key: PublicKey<33>,
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
    bincode::serialize(&OnionAeadContext {
        namespace: ONION_AEAD_NAMESPACE,
        direction,
        circuit_id,
    })
    .map_err(|_| Error::EncodeError)
}

fn backward_aead_context() -> Result<Vec<u8>> {
    bincode::serialize(&OnionAeadDirectionContext {
        namespace: ONION_AEAD_NAMESPACE,
        direction: OnionAeadDirection::Backward,
    })
    .map_err(|_| Error::EncodeError)
}

fn backward_payload_authentication_data(
    return_id: OnionReturnId,
    nonce: OnionBackwardNonce,
    exit_session_public_key: PublicKey<33>,
    payload: &OnionCircuitPayload,
) -> Result<Vec<u8>> {
    bincode::serialize(&OnionBackwardAuthenticationData {
        namespace: ONION_AEAD_NAMESPACE,
        direction: OnionAeadDirection::Backward,
        return_id,
        nonce,
        exit_session_public_key,
        payload,
    })
    .map_err(|_| Error::EncodeError)
}

#[derive(Serialize)]
struct OnionAeadDirectionContext {
    namespace: &'static str,
    direction: OnionAeadDirection,
}

fn validate_route_payload_service(route: &OnionRoute, payload: &OnionCircuitPayload) -> Result<()> {
    if !payload.is_service(route.service_name()) {
        return Err(Error::OnionRouteError(
            OnionRouteError::PayloadServiceMismatch {
                payload_service: payload.service().to_string(),
                route_service: route.service().to_string(),
            },
        ));
    }
    Ok(())
}
