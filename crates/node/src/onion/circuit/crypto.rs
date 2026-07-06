use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::session::SessionSk;
use serde::Serialize;

use super::codec::encode_wire_message;
use super::codec::OnionWireMessage;
use super::OnionBackwardFrame;
use super::OnionCircuitId;
use super::OnionCircuitPayload;
use super::OnionClientReturn;
use super::OnionForwardFrame;
use super::OnionForwardLayer;
use super::MAX_ONION_CIRCUIT_HOPS;
use super::ONION_AEAD_NAMESPACE;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteHop;

/// Encode the first forward frame for `route`.
pub fn encode_initial_forward(
    client: OnionClientReturn,
    route: &OnionRoute,
    circuit_id: OnionCircuitId,
    payload: OnionCircuitPayload,
) -> Result<(Did, Bytes)> {
    let Some(first) = route.encryption_hops.first().copied() else {
        return Err(Error::OnionRouteError(
            "onion route has no hops".to_string(),
        ));
    };
    validate_route_hop_count(route.encryption_hops.len())?;
    let layer = build_forward_layers(client, &route.encryption_hops, circuit_id, payload)?;
    let frame = OnionForwardFrame { circuit_id, layer };
    encode_wire_message(OnionWireMessage::Forward(frame)).map(|payload| (first.did, payload))
}

/// Send a response payload back to the immediate return peer.
pub async fn send_backward(
    scope: &Scope,
    circuit_id: OnionCircuitId,
    return_peer: Did,
    client: OnionClientReturn,
    payload: OnionCircuitPayload,
) -> Result<()> {
    let frame = OnionBackwardFrame {
        circuit_id,
        terminal: payload_closes_circuit(&payload),
        payload: encrypt_client_payload(circuit_id, payload, client.session_public_key)?,
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
        return Err(Error::OnionRouteError(
            "onion route has no hops".to_string(),
        ));
    };
    let mut layer = encrypt_forward_layer(
        circuit_id,
        OnionForwardLayer::Exit { client, payload },
        exit.session_public_key,
    )?;

    for (index, hop) in hops.iter().copied().enumerate().rev().skip(1) {
        let next_hop = hops
            .get(index.saturating_add(1))
            .map(|next| next.did)
            .ok_or_else(|| Error::OnionRouteError("missing next onion hop".to_string()))?;
        let remaining_hops = u8::try_from(hops.len().saturating_sub(index + 1))
            .map_err(|_| Error::OnionRouteError("onion route is too long".to_string()))?;
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
) -> Result<AeadCiphertext> {
    let plaintext = bincode::serialize(&payload).map_err(|_| Error::EncodeError)?;
    let aad = onion_aead_context(OnionAeadDirection::Backward, circuit_id)?;
    let mut rng = rand::thread_rng();
    encrypt_aead_with_rng(&plaintext, &aad, recipient, &mut rng).map_err(Error::CoreError)
}

pub(super) fn decrypt_client_payload(
    session_sk: &SessionSk,
    circuit_id: OnionCircuitId,
    sealed: &AeadCiphertext,
) -> Result<OnionCircuitPayload> {
    let aad = onion_aead_context(OnionAeadDirection::Backward, circuit_id)?;
    let plaintext = session_sk
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    bincode::deserialize(&plaintext).map_err(|_| Error::DecodeError)
}

#[derive(Serialize)]
struct OnionAeadContext {
    namespace: &'static str,
    direction: OnionAeadDirection,
    circuit_id: OnionCircuitId,
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

fn validate_route_hop_count(hops: usize) -> Result<()> {
    if hops == 0 || hops > MAX_ONION_CIRCUIT_HOPS as usize {
        return Err(Error::OnionRouteError(format!(
            "onion route hop count {hops} exceeds limit {MAX_ONION_CIRCUIT_HOPS}"
        )));
    }
    Ok(())
}

fn payload_closes_circuit(payload: &OnionCircuitPayload) -> bool {
    matches!(
        payload,
        OnionCircuitPayload::HttpsResponse(_)
            | OnionCircuitPayload::HttpsError(_)
            | OnionCircuitPayload::TcpClose
            | OnionCircuitPayload::TcpError { .. }
    )
}
