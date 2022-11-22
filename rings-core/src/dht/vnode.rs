#![warn(missing_docs)]
use std::str::FromStr;

use num_bigint::BigUint;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::subring::SubRing;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::Error;
use crate::err::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;

/// VNode Types
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VNodeType {
    /// Encoded data stored in DHT
    Data,
    /// Finger table of a SubRing
    SubRing,
    /// A relayed but unreached message, which should be stored on
    /// the successor of the destination Did.
    RelayMessage,
}

/// A `VirtualNode` is a piece of data with [VNodeType] and [Did]. You can save it to
/// [PeerRing](super::PeerRing) by [ChordStorage](super::ChordStorage) protocol.
///
/// The Did of a Virtual Node is in the following format:
/// * If type value is [VNodeType::Data], it's sha1 of data field.
/// * If type value is [VNodeType::SubRing], it's sha1 of SubRing name.
/// * If type value is [VNodeType::RelayMessage], it's the destination Did of
/// message plus 1 (to ensure that the message is sent to the successor of destination),
/// thus while destination node going online, it will sync message from its successor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualNode {
    /// The did of `VirtualNode` make it unique, and can be stored and retrieved on DHT.
    pub did: Did,
    /// The data entity of `VirtualNode`, encoded by [Encoder].
    pub data: Vec<Encoded>,
    /// The type indicates how the data is encoded and how the Did is generated.
    pub kind: VNodeType,
}

impl<T> TryFrom<MessagePayload<T>> for VirtualNode
where T: Serialize + DeserializeOwned
{
    type Error = Error;
    fn try_from(msg: MessagePayload<T>) -> Result<Self> {
        let did = BigUint::from(msg.addr) + BigUint::from(1u16);
        let data = msg.encode()?;
        Ok(Self {
            did: did.into(),
            data: vec![data],
            kind: VNodeType::RelayMessage,
        })
    }
}

impl TryFrom<Encoded> for VirtualNode {
    type Error = Error;
    fn try_from(e: Encoded) -> Result<Self> {
        let did: HashStr = e.value().into();
        Ok(Self {
            did: Did::from_str(&did.inner())?,
            data: vec![e],
            kind: VNodeType::Data,
        })
    }
}

impl TryFrom<String> for VirtualNode {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        let encoded_message = s.encode()?;
        encoded_message.try_into()
    }
}

impl VirtualNode {
    /// concat data of a virtual Node
    /// We do not needs to check the type of VNode because two VNode with same did but
    /// has different Type is incapable
    pub fn concat(a: &Self, b: &Self) -> Result<Self> {
        match &a.kind {
            VNodeType::RelayMessage => {
                if a.did != b.did {
                    Err(Error::DidNotEqual)
                } else {
                    Ok(Self {
                        did: a.did,
                        data: [&a.data[..], &b.data[..]].concat(),
                        kind: a.kind.clone(),
                    })
                }
            }
            VNodeType::Data => Ok(a.clone()),
            VNodeType::SubRing => {
                // if subring exists, just join creator to new subring
                let decoded_a: String = a.data[0].decode()?;
                let decoded_b: String = a.data[0].decode()?;
                let mut subring_a: SubRing =
                    serde_json::from_str(&decoded_a).map_err(Error::Deserialize)?;
                let subring_b: SubRing =
                    serde_json::from_str(&decoded_b).map_err(Error::Deserialize)?;
                subring_a.finger.join(subring_b.creator);
                subring_a.try_into()
            }
        }
    }
}
