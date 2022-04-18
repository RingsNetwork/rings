use super::chord::PeerRing;
use super::peer::VirtualPeer;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::{Error, Result};
use std::str::FromStr;

#[derive(Clone, Debug)]
pub struct SubRing {
    name: String,
    ring: PeerRing,
}

impl TryFrom<SubRing> for VirtualPeer {
    type Error = Error;
    fn try_from(ring: SubRing) -> Result<Self> {
        let address: HashStr = ring.name.into();
        let data = serde_json::to_string(&ring.ring.finger)
            .map_err(|_| Error::SerializeToString)?;
        Ok(Self {
            address: Did::from_str(&address.inner())?,
            data: vec![data.into()],
        })
    }
}
