#![warn(missing_docs)]
use std::str::FromStr;

use num_bigint::BigUint;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::Error;
use crate::err::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;

/// VNode Types
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VNodeType {
    /// Encoded data stored in DHT
    Data,
    /// Finger table of a SubRing
    SubRing,
    /// A relayed but unreached message, which should be stored on
    /// the successor of the destination Did.
    RelayMessage,
}

/// VNode Operations
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VNodeOperation {
    /// Create or update a VirtualNode
    Overwrite(VirtualNode),
    /// Extend data to a Data type VirtualNode.
    /// This operation will not append data to not existed VirtualNode.
    Extend(VirtualNode),
}

/// A `VirtualNode` is a piece of data with [VNodeType] and [Did]. You can save it to
/// [PeerRing](super::PeerRing) by [ChordStorage](super::ChordStorage) protocol.
///
/// The Did of a Virtual Node is in the following format:
/// * If type value is [VNodeType::Data], it's sha1 of data topic.
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

impl VNodeOperation {
    /// Extract the did of target VirtualNode.
    pub fn did(&self) -> Did {
        match self {
            VNodeOperation::Overwrite(vnode) => vnode.did,
            VNodeOperation::Extend(vnode) => vnode.did,
        }
    }

    /// Extract the kind of target VirtualNode.
    pub fn kind(&self) -> VNodeType {
        match self {
            VNodeOperation::Overwrite(vnode) => vnode.kind,
            VNodeOperation::Extend(vnode) => vnode.kind,
        }
    }

    /// Generate a target VirtualNode when it is not existed.
    pub fn gen_default_vnode(self) -> VirtualNode {
        VirtualNode {
            did: self.did(),
            data: vec![],
            kind: self.kind(),
        }
    }
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

impl TryFrom<String> for VirtualNode {
    type Error = Error;
    fn try_from(topic: String) -> Result<Self> {
        let hash: HashStr = topic.into();
        let did = Did::from_str(&hash.inner())?;
        Ok(Self {
            did,
            data: vec![],
            kind: VNodeType::Data,
        })
    }
}

impl TryFrom<(String, Encoded)> for VirtualNode {
    type Error = Error;
    fn try_from((topic, e): (String, Encoded)) -> Result<Self> {
        let hash: HashStr = topic.into();
        let did = Did::from_str(&hash.inner())?;
        Ok(Self {
            did,
            data: vec![e],
            kind: VNodeType::Data,
        })
    }
}

impl TryFrom<(String, String)> for VirtualNode {
    type Error = Error;
    fn try_from((topic, s): (String, String)) -> Result<Self> {
        let encoded_message = s.encode()?;
        (topic, encoded_message).try_into()
    }
}

impl VirtualNode {
    /// The entry point of VNode operations.
    /// Will dispatch to different operation handlers according to the variant.
    pub fn operate(&self, op: VNodeOperation) -> Result<Self> {
        match op {
            VNodeOperation::Overwrite(vnode) => self.overwrite(vnode),
            VNodeOperation::Extend(vnode) => self.extend(vnode),
        }
    }

    /// Overwrite current data with new data
    pub fn overwrite(&self, other: Self) -> Result<Self> {
        if self.kind != VNodeType::Data {
            return Err(Error::VNodeNotOverwritable);
        }
        if self.kind != other.kind {
            return Err(Error::VNodeKindNotEqual);
        }
        if self.did != other.did {
            return Err(Error::VNodeDidNotEqual);
        }
        Ok(other)
    }

    /// This method is used to extend data to a Data type VNode.
    pub fn extend(&self, other: Self) -> Result<Self> {
        if self.kind != VNodeType::Data {
            return Err(Error::VNodeNotAppendable);
        }
        if self.kind != other.kind {
            return Err(Error::VNodeKindNotEqual);
        }
        if self.did != other.did {
            return Err(Error::VNodeDidNotEqual);
        }
        Ok(Self {
            did: self.did,
            data: [&self.data[..], &other.data[..]].concat(),
            kind: self.kind,
        })
    }
}
