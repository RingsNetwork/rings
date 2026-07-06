use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::message::MessageVerification;
use rings_core::session::SessionSk;
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
use super::OnionVerifiedPayload;
use super::ONION_AEAD_NAMESPACE;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionRouteHop;

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
        payload: encrypt_client_payload(circuit_id, payload, client.session_public_key, signer)?,
    };
    let payload = encode_wire_message(OnionWireMessage::Backward(frame))?;
    scope.send(return_peer, payload).await
}

fn build_forward_layers(
    client: OnionClientReturn,
    hops: &[OnionRouteHop],
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
) -> Result<AeadCiphertext> {
    let Some(exit) = hops.last().copied() else {
        return Err(Error::OnionRouteError(OnionRouteError::RouteHasNoHops));
    };
    let mut layer = encrypt_forward_layer(
        circuit_id,
        OnionForwardLayer::Exit {
            client,
            forward_nonce: OnionForwardNonce::random(),
            payload,
        },
        exit.session_public_key,
    )?;

    for (index, hop) in hops.iter().copied().enumerate().rev().skip(1) {
        let next_hop = hops
            .get(index.saturating_add(1))
            .map(|next| next.did)
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::MissingNextHop))?;
        let remaining_hops = u8::try_from(hops.len().saturating_sub(index + 1)).map_err(|_| {
            Error::OnionRouteError(OnionRouteError::HopCountOutOfBounds {
                hop_count: hops.len(),
                max_hops: super::MAX_ONION_CIRCUIT_HOPS,
            })
        })?;
        layer = encrypt_forward_layer(
            circuit_id,
            OnionForwardLayer::Relay {
                next_hop,
                remaining_hops,
                inner: layer,
            },
            hop.session_public_key,
        )?;
    }
    Ok(layer)
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
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
    recipient: PublicKey<33>,
    signer: &SessionSk,
) -> Result<AeadCiphertext> {
    let authenticated = OnionAuthenticatedPayload::new_signed(circuit_id, payload, signer)?;
    let plaintext = bincode::serialize(&authenticated).map_err(|_| Error::EncodeError)?;
    let aad = onion_aead_context(OnionAeadDirection::Backward, circuit_id)?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

pub(super) fn decrypt_client_payload(
    session_sk: &SessionSk,
    circuit_id: OnionCircuitId,
    sealed: &AeadCiphertext,
) -> Result<OnionAuthenticatedPayload> {
    let aad = onion_aead_context(OnionAeadDirection::Backward, circuit_id)?;
    let plaintext = session_sk
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    bincode::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

impl OnionAuthenticatedPayload {
    /// Sign one backward payload with a fresh replay nonce.
    pub fn new_signed(
        circuit_id: OnionCircuitId,
        payload: OnionCircuitPayload,
        signer: &SessionSk,
    ) -> Result<Self> {
        let nonce = OnionBackwardNonce::random();
        let authentication = MessageVerification::new(
            &backward_payload_authentication_data(
                circuit_id,
                nonce,
                signer.session_public_key(),
                &payload,
            )?,
            signer,
        )
        .map_err(Error::CoreError)?;
        Ok(Self {
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
    /// transcript also binds the circuit id, per-frame nonce, exit session public key, and payload.
    pub fn into_verified_payload(
        self,
        circuit_id: OnionCircuitId,
        expected_exit: &OnionExitDescriptor,
    ) -> Result<OnionVerifiedPayload> {
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
            circuit_id,
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
    circuit_id: OnionCircuitId,
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

fn backward_payload_authentication_data(
    circuit_id: OnionCircuitId,
    nonce: OnionBackwardNonce,
    exit_session_public_key: PublicKey<33>,
    payload: &OnionCircuitPayload,
) -> Result<Vec<u8>> {
    bincode::serialize(&OnionBackwardAuthenticationData {
        namespace: ONION_AEAD_NAMESPACE,
        direction: OnionAeadDirection::Backward,
        circuit_id,
        nonce,
        exit_session_public_key,
        payload,
    })
    .map_err(|_| Error::EncodeError)
}

fn validate_route_payload_service(route: &OnionRoute, payload: &OnionCircuitPayload) -> Result<()> {
    if !payload.is_service(route.service()) {
        return Err(Error::OnionRouteError(
            OnionRouteError::PayloadServiceMismatch {
                payload_service: payload.service.clone(),
                route_service: route.service().to_string(),
            },
        ));
    }
    Ok(())
}
