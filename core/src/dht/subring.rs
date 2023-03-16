#![warn(missing_docs)]

use serde::Deserialize;
use serde::Serialize;

use super::vnode::VNodeType;
use super::vnode::VirtualNode;
use super::FingerTable;
use crate::dht::Did;
use crate::err::Error;
use crate::err::Result;

/// A Subring is like a [super::PeerRing] without storage functional.
/// Subring also have two extra fields: `name` and `creator`.
/// Subring can be stored on the a [super::PeerRing].
/// The did of a subring is the hash of its name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subring {
    /// name of subring
    pub name: String,
    /// finger table
    pub finger: FingerTable,
    /// creator
    pub creator: Did,
}

impl Subring {
    /// Create a new Subring
    pub fn new(name: &str, creator: Did) -> Result<Self> {
        let did = VirtualNode::gen_did(name)?;
        Ok(Self {
            name: name.to_string(),
            finger: FingerTable::new(did, 1),
            creator,
        })
    }
}

impl TryFrom<Subring> for VirtualNode {
    type Error = Error;
    fn try_from(ring: Subring) -> Result<Self> {
        let data = serde_json::to_string(&ring).map_err(|_| Error::SerializeToString)?;
        Ok(Self {
            did: Self::gen_did(&ring.name)?,
            data: vec![data.into()],
            kind: VNodeType::Subring,
        })
    }
}

impl TryFrom<VirtualNode> for Subring {
    type Error = Error;
    fn try_from(vnode: VirtualNode) -> Result<Self> {
        match &vnode.kind {
            VNodeType::Subring => {
                let decoded: String = vnode.data[0].decode()?;
                let subring: Subring =
                    serde_json::from_str(&decoded).map_err(Error::Deserialize)?;
                Ok(subring)
            }
            _ => Err(Error::InvalidVNodeType),
        }
    }
}
