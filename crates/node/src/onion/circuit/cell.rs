use bytes::Bytes;
use rand::CryptoRng;
use rand::RngCore;
use rings_core::ecc::elgamal::impls::secp256k1::encrypt_aead_with_rng;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;
use rings_core::ecc::PublicKey;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;

use super::codec::OnionWireMessage;
use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionRouteError;

const CELL_LENGTH_PREFIX_BYTES: usize = size_of::<u32>();
const ONION_CELL_AEAD_NAMESPACE: &[u8] = b"rings-node:onion-cell:v1";

/// Public size classes used by encrypted onion cells.
///
/// The class is intentionally visible while the direction, message discriminant, and exact
/// application length are encrypted. A small class set bounds padding overhead without exposing
/// a byte-accurate traffic fingerprint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OnionCellBucket {
    /// Up to four KiB of encrypted cell plaintext.
    KiB4,
    /// Up to sixteen KiB of encrypted cell plaintext.
    KiB16,
    /// Up to sixty-four KiB of encrypted cell plaintext.
    KiB64,
    /// Up to 256 KiB of encrypted cell plaintext.
    KiB256,
    /// Up to one MiB of encrypted cell plaintext.
    MiB1,
    /// Up to four MiB of encrypted cell plaintext.
    MiB4,
    /// Up to twelve MiB of encrypted cell plaintext. Admission charges the full visible bucket
    /// size, irrespective of the hidden encoded length, so an undersized payload cannot bypass
    /// the relay's per-peer or global byte budget.
    MiB12,
}

impl OnionCellBucket {
    const ALL: [Self; 7] = [
        Self::KiB4,
        Self::KiB16,
        Self::KiB64,
        Self::KiB256,
        Self::MiB1,
        Self::MiB4,
        Self::MiB12,
    ];

    /// Return the fixed plaintext length protected by this cell class.
    pub const fn plaintext_len(self) -> usize {
        match self {
            Self::KiB4 => 4 * 1024,
            Self::KiB16 => 16 * 1024,
            Self::KiB64 => 64 * 1024,
            Self::KiB256 => 256 * 1024,
            Self::MiB1 => 1024 * 1024,
            Self::MiB4 => 4 * 1024 * 1024,
            Self::MiB12 => 12 * 1024 * 1024,
        }
    }

    fn smallest_for(encoded_len: usize) -> Result<Self> {
        let required = encoded_len
            .checked_add(CELL_LENGTH_PREFIX_BYTES)
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::CellPayloadTooLarge))?;
        Self::ALL
            .into_iter()
            .find(|bucket| bucket.plaintext_len() >= required)
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::CellPayloadTooLarge))
    }

    fn accepts(self, encoded_len: usize) -> bool {
        encoded_len
            .checked_add(CELL_LENGTH_PREFIX_BYTES)
            .is_some_and(|required| required <= self.plaintext_len())
    }
}

/// Hop-to-hop ciphertext whose serialized size reveals only [`OnionCellBucket`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct OnionWireCell {
    pub(super) bucket: OnionCellBucket,
    pub(super) sealed: AeadCiphertext,
}

pub(super) fn encode_message(message: &OnionWireMessage) -> Result<Bytes> {
    bincode::serialize(message)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}

pub(super) fn seal_message(
    message: &OnionWireMessage,
    recipient: PublicKey<33>,
    bucket: Option<OnionCellBucket>,
) -> Result<Bytes> {
    let encoded = encode_message(message)?;
    seal_encoded_message(&encoded, recipient, bucket)
}

pub(super) fn seal_encoded_message(
    encoded: &[u8],
    recipient: PublicKey<33>,
    bucket: Option<OnionCellBucket>,
) -> Result<Bytes> {
    let mut rng = rand::thread_rng();
    seal_encoded_message_with_rng(encoded, recipient, bucket, &mut rng)
}

fn seal_encoded_message_with_rng<R: CryptoRng + RngCore>(
    encoded: &[u8],
    recipient: PublicKey<33>,
    bucket: Option<OnionCellBucket>,
    rng: &mut R,
) -> Result<Bytes> {
    let bucket = bucket.map_or_else(|| OnionCellBucket::smallest_for(encoded.len()), Ok)?;
    if !bucket.accepts(encoded.len()) {
        return Err(Error::OnionRouteError(OnionRouteError::CellPayloadTooLarge));
    }
    let encoded_len = u32::try_from(encoded.len())
        .map_err(|_| Error::OnionRouteError(OnionRouteError::CellPayloadTooLarge))?;
    let mut plaintext = vec![0_u8; bucket.plaintext_len()];
    let encoded_end = CELL_LENGTH_PREFIX_BYTES
        .checked_add(encoded.len())
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::CellPayloadTooLarge))?;
    plaintext
        .get_mut(..CELL_LENGTH_PREFIX_BYTES)
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::InvalidCell))?
        .copy_from_slice(&encoded_len.to_le_bytes());
    plaintext
        .get_mut(CELL_LENGTH_PREFIX_BYTES..encoded_end)
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::InvalidCell))?
        .copy_from_slice(encoded);
    rng.fill_bytes(
        plaintext
            .get_mut(encoded_end..)
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::InvalidCell))?,
    );
    let aad = cell_aad(bucket)?;
    let sealed =
        encrypt_aead_with_rng(&plaintext, &aad, recipient, rng).map_err(Error::CoreError)?;
    bincode::serialize(&OnionWireCell { bucket, sealed })
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}

pub(super) fn open_cell(
    session_sk: &SessionSk,
    bucket: OnionCellBucket,
    sealed: &AeadCiphertext,
) -> Result<OnionWireMessage> {
    let aad = cell_aad(bucket)?;
    let plaintext = session_sk
        .decrypt_elgamal_aead(sealed, &aad)
        .map_err(Error::CoreError)?;
    if plaintext.len() != bucket.plaintext_len() {
        return Err(Error::OnionRouteError(OnionRouteError::InvalidCell));
    }
    let encoded_len = u32::from_le_bytes(
        plaintext
            .get(..CELL_LENGTH_PREFIX_BYTES)
            .ok_or_else(|| Error::OnionRouteError(OnionRouteError::InvalidCell))?
            .try_into()
            .map_err(|_| Error::OnionRouteError(OnionRouteError::InvalidCell))?,
    ) as usize;
    if !bucket.accepts(encoded_len) {
        return Err(Error::OnionRouteError(OnionRouteError::InvalidCell));
    }
    let encoded_end = CELL_LENGTH_PREFIX_BYTES
        .checked_add(encoded_len)
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::InvalidCell))?;
    let encoded = plaintext
        .get(CELL_LENGTH_PREFIX_BYTES..encoded_end)
        .ok_or_else(|| Error::OnionRouteError(OnionRouteError::InvalidCell))?;
    bincode::deserialize(encoded).map_err(|_| Error::DecodeError)
}

fn cell_aad(bucket: OnionCellBucket) -> Result<Vec<u8>> {
    bincode::serialize(&(ONION_CELL_AEAD_NAMESPACE, bucket)).map_err(|_| Error::EncodeError)
}

#[cfg(test)]
mod tests {
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;

    use super::*;
    use crate::onion::circuit::OnionBackwardFrame;
    use crate::onion::circuit::OnionCircuitId;

    fn session() -> SessionSk {
        SessionSk::new_with_seckey(&SecretKey::random()).expect("session key")
    }

    fn backward_message(payload_len: usize) -> OnionWireMessage {
        let recipient = session();
        let sealed = encrypt_aead_with_rng(
            &vec![7_u8; payload_len],
            b"cell-test",
            recipient.session_public_key(),
            &mut rand::thread_rng(),
        )
        .expect("encrypt fixture");
        OnionWireMessage::Backward(OnionBackwardFrame {
            circuit_id: OnionCircuitId::new([1; 16]),
            payload: sealed,
        })
    }

    #[test]
    fn small_messages_share_one_observable_cell_size() {
        let recipient = session();
        let short = seal_message(&backward_message(1), recipient.session_public_key(), None)
            .expect("seal short");
        let longer = seal_message(
            &backward_message(1_000),
            recipient.session_public_key(),
            None,
        )
        .expect("seal longer");
        assert_eq!(short.len(), longer.len());
    }

    #[test]
    fn cell_round_trip_rejects_wrong_recipient() {
        let recipient = session();
        let wrong = session();
        let message = backward_message(1);
        let encoded =
            seal_message(&message, recipient.session_public_key(), None).expect("seal message");
        let cell: OnionWireCell = bincode::deserialize(&encoded).expect("decode cell");
        assert_eq!(
            open_cell(&recipient, cell.bucket, &cell.sealed).expect("open cell"),
            message
        );
        assert!(open_cell(&wrong, cell.bucket, &cell.sealed).is_err());
    }

    #[test]
    fn one_hop_cover_is_authenticated_inside_the_same_cell_algebra() {
        let recipient = session();
        let encoded = seal_message(
            &OnionWireMessage::Cover,
            recipient.session_public_key(),
            Some(OnionCellBucket::KiB4),
        )
        .expect("seal cover");
        let cell: OnionWireCell = bincode::deserialize(&encoded).expect("decode cover cell");

        assert_eq!(
            open_cell(&recipient, cell.bucket, &cell.sealed).expect("open cover cell"),
            OnionWireMessage::Cover
        );
    }
}
