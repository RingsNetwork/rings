#![warn(missing_docs)]
use super::chord::PeerRing;
use super::vnode::VNodeType;
use super::vnode::VirtualNode;
use super::FingerTable;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::{Error, Result};
use serde::Deserialize;
use serde::Serialize;
use std::str::FromStr;

/// A SubRing is a full functional Ring, but with a name and it's finger table can be
/// stored on Main Rings DHT, For a SubRing, it's virtual address is `sha1(name)`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubRing {
    /// name of subring
    name: String,
    /// did of subring, generate with hash(name)
    did: Did,
    /// finger table
    ring: FingerTable,
    /// admin of ring, for verify that a message is come from ring
    admin: Option<Did>,
}

impl SubRing {
    /// Create a new SubRing
    pub fn new(name: String) -> Result<Self> {
        let address: HashStr = name.clone().into();
        let did = Did::from_str(&address.inner())?;
        Ok(Self {
            name,
            did,
            ring: FingerTable::new(did, 1),
            admin: None,
        })
    }
}

impl TryFrom<SubRing> for VirtualNode {
    type Error = Error;
    fn try_from(ring: SubRing) -> Result<Self> {
        let data = serde_json::to_string(&ring).map_err(|_| Error::SerializeToString)?;
        Ok(Self {
            address: ring.did,
            data: vec![data.into()],
            kind: VNodeType::SubRing,
        })
    }
}

impl From<SubRing> for PeerRing {
    fn from(ring: SubRing) -> Self {
        let mut pr = PeerRing::new_with_config(ring.did, 1);
        pr.finger = ring.ring.clone();
        pr
    }
}
