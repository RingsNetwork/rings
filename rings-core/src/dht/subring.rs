#![warn(missing_docs)]
use super::chord::PeerRing;
use super::vnode::VNodeType;
use super::vnode::VirtualNode;
use super::FingerTable;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::{Error, Result};
use crate::storage::MemStorage;
use serde::Deserialize;
use serde::Serialize;
use std::str::FromStr;

/// A SubRing is a full functional Ring, but with a name and it's finger table can be
/// stored on Main Rings DHT, For a SubRing, it's virtual address is `sha1(name)`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubRing {
    /// name of subring
    pub name: String,
    /// did of subring, generate with hash(name)
    pub did: Did,
    /// finger table
    pub finger: FingerTable,
    /// admin of ring, for verify that a message is come from ring
    pub admin: Option<Did>,
    /// creator
    pub creator: Did,
}

/// SubRing manager is a HashTable of SubRing
pub struct SubRingManager {
    id: Did,
    table: MemStorage<Did, SubRing>,
}

impl SubRingManager {
    /// new instance
    pub fn new(id: Did) -> Self {
        Self {
            id,
            table: MemStorage::<Did, SubRing>::new(),
        }
    }

    /// create a new SubRing and store in table
    pub fn create_subring(&self, name: &str) -> Result<SubRing> {
        let subring = SubRing::new(name, &self.id)?;
        self.table.set(&subring.did.clone(), subring.clone());
        Ok(subring)
    }

    /// get subring by id
    pub fn get(&self, id: &Did) -> Option<SubRing> {
        self.table.get(id)
    }

    /// get subring by id
    pub fn set(&self, subring: &SubRing) {
        let id = subring.did;
        self.table.set(&id, subring.clone());
    }

    /// get subring by name
    pub fn get_by_name(&self, name: &str) -> Result<Option<SubRing>> {
        let address: HashStr = name.to_owned().into();
        let did = Did::from_str(&address.inner())?;
        Ok(self.get(&did))
    }

    /// get subring, update and putback
    pub fn get_for_update(&self, id: Did, callback: Box<dyn FnOnce(Option<SubRing>) -> SubRing>) {
        let subring = callback(self.get(&id));
        self.set(&subring);
    }

    /// get subring, update and putback
    pub fn get_for_update_by_name(
        &self,
        name: &str,
        callback: Box<dyn FnOnce(Option<SubRing>) -> SubRing>,
    ) -> Result<()> {
        let address: HashStr = name.to_owned().into();
        let did = Did::from_str(&address.inner())?;
        self.get_for_update(did, callback);
        Ok(())
    }
}

impl SubRing {
    /// Create a new SubRing
    pub fn new(name: &str, creator: &Did) -> Result<Self> {
        let address: HashStr = name.to_owned().into();
        let did = Did::from_str(&address.inner())?;
        Ok(Self {
            name: name.to_owned(),
            did,
            finger: FingerTable::new(did, 1),
            admin: None,
            creator: *creator,
        })
    }

    /// Create a SubRing from Ring
    pub fn from_ring(name: &str, ring: &PeerRing) -> Result<Self> {
        let address: HashStr = name.to_owned().into();
        let did = Did::from_str(&address.inner())?;
        Ok(Self {
            name: name.to_owned(),
            did,
            finger: ring.finger.clone(),
            admin: None,
            creator: ring.id,
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
        // set finger[0] to successor
        if let Some(id) = ring.finger.first() {
            pr.successor.update(id);
        }
        pr.finger = ring.finger;
        pr
    }
}
