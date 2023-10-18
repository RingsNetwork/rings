#![warn(missing_docs)]
use std::cmp::max;
use std::str::FromStr;

use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

use super::subring::Subring;
use crate::consts::VNODE_DATA_MAX_LEN;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;

/// VNode Types
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VNodeType {
    /// Encoded data stored in DHT
    Data,
    /// Finger table of a Subring
    Subring,
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
    /// This operation will create VirtualNode if it's not existed.
    Extend(VirtualNode),
    /// Extend data to a Data type VirtualNode uniquely.
    /// If any element is already existed, move it to the end of the data vector.
    /// This operation will create VirtualNode if it's not existed.
    Touch(VirtualNode),
    /// Join subring.
    JoinSubring(String, Did),
}

/// A `VirtualNode` is a piece of data with [VNodeType] and [Did]. You can save it to
/// [PeerRing](super::PeerRing) by [ChordStorage](super::ChordStorage) protocol.
///
/// The Did of a Virtual Node is in the following format:
/// * If type value is [VNodeType::Data], it's sha1 of data topic.
/// * If type value is [VNodeType::Subring], it's sha1 of Subring name.
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

impl VirtualNode {
    /// Generate did from topic.
    pub fn gen_did(topic: &str) -> Result<Did> {
        let hash: HashStr = topic.into();
        let did = Did::from_str(&hash.inner());
        tracing::debug!("gen_did: topic: {}, did: {:?}", topic, did);
        did
    }
}

impl VNodeOperation {
    /// Extract the did of target VirtualNode.
    pub fn did(&self) -> Result<Did> {
        Ok(match self {
            VNodeOperation::Overwrite(vnode) => vnode.did,
            VNodeOperation::Extend(vnode) => vnode.did,
            VNodeOperation::Touch(vnode) => vnode.did,
            VNodeOperation::JoinSubring(name, _) => VirtualNode::gen_did(name)?,
        })
    }

    /// Extract the kind of target VirtualNode.
    pub fn kind(&self) -> VNodeType {
        match self {
            VNodeOperation::Overwrite(vnode) => vnode.kind,
            VNodeOperation::Extend(vnode) => vnode.kind,
            VNodeOperation::Touch(vnode) => vnode.kind,
            VNodeOperation::JoinSubring(..) => VNodeType::Subring,
        }
    }

    /// Generate a target VirtualNode when it is not existed.
    pub fn gen_default_vnode(self) -> Result<VirtualNode> {
        match self {
            VNodeOperation::JoinSubring(name, did) => Subring::new(&name, did)?.try_into(),
            _ => Ok(VirtualNode {
                did: self.did()?,
                data: vec![],
                kind: self.kind(),
            }),
        }
    }
}

impl TryFrom<MessagePayload> for VirtualNode {
    type Error = Error;
    fn try_from(msg: MessagePayload) -> Result<Self> {
        let did = BigUint::from(msg.signer()) + BigUint::from(1u16);
        let data = msg.encode()?;
        Ok(Self {
            did: did.into(),
            data: vec![data],
            kind: VNodeType::RelayMessage,
        })
    }
}

impl TryFrom<(String, Encoded)> for VirtualNode {
    type Error = Error;
    fn try_from((topic, e): (String, Encoded)) -> Result<Self> {
        Ok(Self {
            did: Self::gen_did(&topic)?,
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

impl TryFrom<String> for VirtualNode {
    type Error = Error;
    fn try_from(topic: String) -> Result<Self> {
        (topic.clone(), topic).try_into()
    }
}

impl VirtualNode {
    /// Affine Transport vnode to a list of affined did
    pub fn affine(&self, scalar: u16) -> Vec<VirtualNode> {
        self.did
            .rotate_affine(scalar)
            .iter()
            .map(|did| self.clone_with_did(did.to_owned()))
            .collect()
    }

    /// Clone and setup with new DID
    pub fn clone_with_did(&self, did: Did) -> Self {
        let mut vnode = self.clone();
        vnode.did = did;
        vnode
    }

    /// The entry point of [VNodeOperation].
    /// Will dispatch to different operation handlers according to the variant.
    pub fn operate(&self, op: VNodeOperation) -> Result<Self> {
        match op {
            VNodeOperation::Overwrite(vnode) => self.overwrite(vnode),
            VNodeOperation::Extend(vnode) => self.extend(vnode),
            VNodeOperation::Touch(vnode) => self.touch(vnode),
            VNodeOperation::JoinSubring(_, did) => self.join_subring(did),
        }
    }

    /// Overwrite current data with new data.
    /// The handler of [VNodeOperation::Overwrite].
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

    /// This method is used to extend data to a Data type VirtualNode.
    /// The handler of [VNodeOperation::Extend].
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

        let trim_num = max(
            0,
            (self.data.len() + other.data.len()) as i64 - VNODE_DATA_MAX_LEN as i64,
        ) as usize;

        let mut data = self.data.iter().skip(trim_num).cloned().collect::<Vec<_>>();
        data.extend_from_slice(&other.data);

        Ok(Self {
            did: self.did,
            data,
            kind: self.kind,
        })
    }

    /// This method is used to extend data to a Data type VirtualNode uniquely.
    /// If any element is already existed, move it to the end of the data vector.
    /// The handler of [VNodeOperation::Touch].
    pub fn touch(&self, other: Self) -> Result<Self> {
        if self.kind != VNodeType::Data {
            return Err(Error::VNodeNotAppendable);
        }
        if self.kind != other.kind {
            return Err(Error::VNodeKindNotEqual);
        }
        if self.did != other.did {
            return Err(Error::VNodeDidNotEqual);
        }

        let remains = self
            .data
            .iter()
            .filter(|e| !other.data.contains(e))
            .collect::<Vec<_>>();

        let trim_num = max(
            0,
            (remains.len() + other.data.len()) as i64 - VNODE_DATA_MAX_LEN as i64,
        ) as usize;

        let mut data = remains
            .into_iter()
            .skip(trim_num)
            .cloned()
            .collect::<Vec<_>>();
        data.extend_from_slice(&other.data);

        Ok(Self {
            did: self.did,
            data,
            kind: self.kind,
        })
    }

    /// This method is used to join a subring.
    /// The handler of [VNodeOperation::JoinSubring].
    pub fn join_subring(&self, did: Did) -> Result<Self> {
        if self.kind != VNodeType::Subring {
            return Err(Error::VNodeNotJoinable);
        }

        let mut subring: Subring = self.clone().try_into()?;
        subring.finger.join(did);
        subring.try_into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vnode_extend_over_max_len() {
        let topic = "test0".to_string();
        let mut vnode: VirtualNode = topic.clone().try_into().unwrap();
        assert_eq!(vnode.data.len(), 1);

        for i in 1..VNODE_DATA_MAX_LEN {
            let topic = topic.clone();
            let data = format!("test{}", i);

            let other = (topic, data).try_into().unwrap();
            vnode = vnode.extend(other).unwrap();

            assert_eq!(vnode.data.len(), i + 1);
        }

        for i in VNODE_DATA_MAX_LEN..VNODE_DATA_MAX_LEN + 10 {
            let topic = topic.clone();
            let data = format!("test{}", i);

            let other = (topic, data.clone()).try_into().unwrap();
            vnode = vnode.extend(other).unwrap();

            // The length should be trimmed to max length.
            assert_eq!(vnode.data.len(), VNODE_DATA_MAX_LEN);

            // The first data should be trimmed.
            assert_eq!(
                vnode.data[0].decode::<String>().unwrap(),
                format!("test{}", i - VNODE_DATA_MAX_LEN + 1)
            );

            // The last data should be the latest one.
            assert_eq!(
                vnode.data[VNODE_DATA_MAX_LEN - 1]
                    .decode::<String>()
                    .unwrap(),
                data
            );
        }
    }
}
