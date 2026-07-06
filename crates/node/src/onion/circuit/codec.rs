use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use super::OnionBackwardFrame;
use super::OnionCircuitId;
use super::OnionForwardFrame;
use super::OnionForwardLayer;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Reject;
use crate::extension::ext::Wire;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(super) enum OnionWireMessage {
    Forward(OnionForwardFrame),
    Backward(OnionBackwardFrame),
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(super) enum OnionLocalMessage {
    ForwardReady {
        from: Did,
        received_at_ms: u128,
        circuit_id: OnionCircuitId,
        layer: OnionForwardLayer,
    },
    BackwardReady {
        from: Did,
        received_at_ms: u128,
        frame: OnionBackwardFrame,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum OnionCircuitInput {
    ForwardObserved {
        from: Did,
        circuit_id: OnionCircuitId,
        layer: rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext,
    },
    BackwardObserved {
        from: Did,
        frame: OnionBackwardFrame,
    },
    ForwardReady {
        from: Did,
        received_at_ms: u128,
        circuit_id: OnionCircuitId,
        layer: OnionForwardLayer,
    },
    BackwardReady {
        from: Did,
        received_at_ms: u128,
        frame: OnionBackwardFrame,
    },
}

/// One typed onion-circuit input accepted by the pure reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionCircuitEvent {
    pub(super) input: OnionCircuitInput,
}

pub(super) fn decode_event(wire: Wire<'_>) -> std::result::Result<OnionCircuitEvent, Reject> {
    if wire.from == wire.me {
        decode_local_message(wire.payload)
    } else {
        decode_wire_message(wire.from, wire.payload)
    }
}

fn decode_wire_message(
    from: Did,
    payload: &[u8],
) -> std::result::Result<OnionCircuitEvent, Reject> {
    let message = bincode::deserialize::<OnionWireMessage>(payload)
        .map_err(|error| Reject(format!("bad onion circuit message: {error}")))?;
    let input = match message {
        OnionWireMessage::Forward(frame) => OnionCircuitInput::ForwardObserved {
            from,
            circuit_id: frame.circuit_id,
            layer: frame.layer,
        },
        OnionWireMessage::Backward(frame) => OnionCircuitInput::BackwardObserved { from, frame },
    };
    Ok(OnionCircuitEvent { input })
}

fn decode_local_message(payload: &[u8]) -> std::result::Result<OnionCircuitEvent, Reject> {
    let message = bincode::deserialize::<OnionLocalMessage>(payload)
        .map_err(|error| Reject(format!("bad local onion circuit message: {error}")))?;
    let input = match message {
        OnionLocalMessage::ForwardReady {
            from,
            received_at_ms,
            circuit_id,
            layer,
        } => OnionCircuitInput::ForwardReady {
            from,
            received_at_ms,
            circuit_id,
            layer,
        },
        OnionLocalMessage::BackwardReady {
            from,
            received_at_ms,
            frame,
        } => OnionCircuitInput::BackwardReady {
            from,
            received_at_ms,
            frame,
        },
    };
    Ok(OnionCircuitEvent { input })
}

pub(super) fn encode_wire_message(message: OnionWireMessage) -> Result<Bytes> {
    bincode::serialize(&message)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}

pub(super) fn encode_local_message(message: OnionLocalMessage) -> Result<Bytes> {
    bincode::serialize(&message)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}
