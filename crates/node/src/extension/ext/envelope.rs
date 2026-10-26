//! Wire envelope — the namespaced message carried over the P2P transport.

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// Namespaced message envelope carried over the P2P transport codec, in place of
/// the old closed `BackendMessage` enum. `payload` is opaque to the core.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol namespace this payload belongs to.
    pub namespace: String,
    /// Opaque protocol payload; the inner codec is the protocol's own business.
    pub payload: Bytes,
}

impl Envelope {
    /// Build an envelope.
    pub fn new(namespace: impl Into<String>, payload: Bytes) -> Self {
        Self {
            namespace: namespace.into(),
            payload,
        }
    }

    /// Encode for the P2P transport. `encode : Envelope → [u8]`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        rings_codec::serialize(self).map_err(|_| Error::EncodeError)
    }

    /// Decode from the P2P transport. `decode : [u8] ⇀ Envelope` (partial).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        rings_codec::deserialize(bytes).map_err(|_| Error::DecodeError)
    }

    /// Read an encoded envelope in place, `decode` without copying: the namespace and the
    /// payload borrowed from `bytes` (#938).
    pub(crate) fn view(bytes: &[u8]) -> Result<EnvelopeView<'_>> {
        match rings_codec::deserialize_prefix(bytes) {
            Ok((view, [])) => Ok(view),
            Ok(_) | Err(_) => Err(Error::DecodeError),
        }
    }

    /// The encoded prefix every envelope of `namespace` starts with. The encoding is
    /// length-prefixed, so the prefixes of distinct namespaces never prefix each other.
    /// `namespace_prefix : str → [u8]`.
    pub(crate) fn namespace_prefix(namespace: &str) -> Result<Vec<u8>> {
        rings_codec::serialize(&namespace).map_err(|_| Error::EncodeError)
    }
}

/// An [`Envelope`] read in place: the same encoding, its namespace and payload borrowed from
/// the bytes it was read from, so routing an envelope copies neither.
///
/// ```text
///   Envelope::view(b) = Ok(v)  ⟺  Envelope::decode(b) = Ok(e),
///                                  v.namespace = e.namespace, v.payload = e.payload
/// ```
#[derive(Deserialize)]
pub(crate) struct EnvelopeView<'a> {
    /// Protocol namespace this payload belongs to.
    pub(crate) namespace: &'a str,
    /// Opaque protocol payload.
    pub(crate) payload: &'a [u8],
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::Envelope;

    /// Viewing reads what decoding reads, without copying.
    #[test]
    fn test_viewing_an_envelope_reads_what_decoding_reads() {
        let envelope = Envelope::new("relay", Bytes::from_static(b"payload"));
        let bytes = envelope.encode().expect("an envelope encodes");
        let view = Envelope::view(&bytes).expect("an encoded envelope is read in place");
        assert_eq!(view.namespace, envelope.namespace);
        assert_eq!(view.payload, envelope.payload.as_ref());
        assert!(Envelope::view(&bytes[..bytes.len() - 1]).is_err());
    }
}
