#![warn(missing_docs)]
use std::str::FromStr;

use super::chord::PeerRing;
use super::vnode::VNodeType;
use super::vnode::VirtualNode;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::Error;
use crate::err::Result;

/// A SubRing is a full functional Ring, but with a name and it's finger table can be
/// stored on Main Rings DHT, For a SubRing, it's virtual address is `sha1(name)`
#[derive(Clone, Debug)]
pub struct SubRing {
    name: String,
    ring: PeerRing,
}

impl TryFrom<SubRing> for VirtualNode {
    type Error = Error;
    fn try_from(ring: SubRing) -> Result<Self> {
        let address: HashStr = ring.name.into();
        let data =
            serde_json::to_string(&ring.ring.finger).map_err(|_| Error::SerializeToString)?;
        Ok(Self {
            address: Did::from_str(&address.inner())?,
            data: vec![data.into()],
            kind: VNodeType::SubRing,
        })
    }
}
