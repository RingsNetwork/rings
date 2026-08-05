use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use super::cell::OnionCellBucket;
use super::cell::OnionWireCell;
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
    /// One-hop link padding. It is authenticated to the immediate neighbor and never forwarded.
    Cover,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(super) enum OnionLocalMessage {
    CellReady {
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        message: OnionWireMessage,
    },
    ForwardReady {
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        circuit_id: OnionCircuitId,
        layer: OnionForwardLayer,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum OnionCircuitInput {
    CellObserved {
        from: Did,
        bucket: OnionCellBucket,
        sealed: rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext,
    },
    CellReady {
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        message: OnionWireMessage,
    },
    ForwardReady {
        from: Did,
        received_at_ms: u128,
        bucket: OnionCellBucket,
        circuit_id: OnionCircuitId,
        layer: OnionForwardLayer,
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
    const MAX_SERIALIZED_CELL_OVERHEAD: usize = 4 * 1024;
    const AEAD_TAG_BYTES: usize = 16;
    let max_wire_len = OnionCellBucket::MiB12
        .plaintext_len()
        .saturating_add(MAX_SERIALIZED_CELL_OVERHEAD);
    if payload.len() > max_wire_len {
        return Err(Reject(
            "encrypted onion cell exceeds wire bound".to_string(),
        ));
    }
    let cell = bincode::deserialize::<OnionWireCell>(payload)
        .map_err(|error| Reject(format!("bad encrypted onion cell: {error}")))?;
    let expected_ciphertext_len = cell
        .bucket
        .plaintext_len()
        .checked_add(AEAD_TAG_BYTES)
        .ok_or_else(|| Reject("encrypted onion cell length overflow".to_string()))?;
    if cell.sealed.ciphertext.len() != expected_ciphertext_len {
        return Err(Reject(
            "encrypted onion cell does not match its size class".to_string(),
        ));
    }
    Ok(OnionCircuitEvent {
        input: OnionCircuitInput::CellObserved {
            from,
            bucket: cell.bucket,
            sealed: cell.sealed,
        },
    })
}

fn decode_local_message(payload: &[u8]) -> std::result::Result<OnionCircuitEvent, Reject> {
    let message = bincode::deserialize::<OnionLocalMessage>(payload)
        .map_err(|error| Reject(format!("bad local onion circuit message: {error}")))?;
    let input = match message {
        OnionLocalMessage::CellReady {
            from,
            received_at_ms,
            bucket,
            message,
        } => OnionCircuitInput::CellReady {
            from,
            received_at_ms,
            bucket,
            message,
        },
        OnionLocalMessage::ForwardReady {
            from,
            received_at_ms,
            bucket,
            circuit_id,
            layer,
        } => OnionCircuitInput::ForwardReady {
            from,
            received_at_ms,
            bucket,
            circuit_id,
            layer,
        },
    };
    Ok(OnionCircuitEvent { input })
}

pub(super) fn encode_local_message(message: OnionLocalMessage) -> Result<Bytes> {
    bincode::serialize(&message)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}
