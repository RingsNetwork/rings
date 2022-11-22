#![warn(missing_docs)]
use std::str::FromStr;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

use super::chord::PeerRing;
use super::chord::PeerRingAction;
use super::chord::RemoteAction;
use super::types::Chord;
use super::types::SubRingManager;
use super::vnode::VNodeType;
use super::vnode::VirtualNode;
use super::FingerTable;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::err::Error;
use crate::err::Result;
use crate::storage::PersistenceStorageReadAndWrite;
// use crate::storage::PersistenceStorageOperation;

/// A SubRing is a full functional Ring.
/// But with a name and it's finger table can be
/// stored on Main Rings DHT, For a SubRing, it's virtual address is `sha1(name)`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl SubRingManager<PeerRingAction> for PeerRing {
    async fn join_subring(&self, id: &Did, rid: &Did) -> Result<PeerRingAction> {
        match self.find_successor(*rid) {
            Ok(PeerRingAction::Some(_)) => {
                let id = id.to_owned();
                if let Ok(subring) = self.get_subring(rid).await {
                    let mut sr = subring;
                    sr.finger.join(id);
                    self.store_subring(&sr).await?;
                }
                Ok(PeerRingAction::None)
            }
            Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(_))) => Ok(
                PeerRingAction::RemoteAction(n, RemoteAction::FindAndJoinSubRing(*rid)),
            ),
            Ok(a) => Err(Error::PeerRingUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    async fn cloest_preceding_node_for_subring(&self, id: &Did, rid: &Did) -> Option<Result<Did>> {
        let id = id.to_owned();
        if let Ok(subring) = self.get_subring(rid).await {
            Some(subring.finger.closest(id))
        } else {
            None
        }
    }

    async fn get_subring(&self, id: &Did) -> Result<SubRing> {
        let vn: VirtualNode = self.storage.get(id).await?;
        Ok(vn.try_into()?)
    }

    async fn store_subring(&self, subring: &SubRing) -> Result<()> {
        let id = subring.did;
        let vn: VirtualNode = subring.clone().try_into()?;
        self.storage.put(&id, &vn).await?;
        Ok(())
    }

    async fn get_subring_by_name(&self, name: &str) -> Result<SubRing> {
        let address: HashStr = name.to_owned().into();
        // trans Result to Option here
        let did = Did::from_str(&address.inner())?;
        self.get_subring(&did).await
    }
    // get subring, update and putback
    // async fn get_subring_for_update(
    //     &self,
    //     id: &Did,
    //     callback: Arc<dyn FnOnce(SubRing) -> SubRing>,
    // ) -> Result<bool> {
    //     if let Ok(subring) = self.get_subring(id).await {
    //         let sr = callback(subring);
    //         self.store_subring(&sr).await?;
    //         Ok(true)
    //     } else {
    //         Ok(false)
    //     }
    // }

    // /// get subring, update and putback
    // async fn get_subring_for_update_by_name(
    //     &self,
    //     name: &str,
    //     callback: Box<dyn FnOnce(SubRing) -> SubRing>,
    // ) -> Result<bool> {
    //     let address: HashStr = name.to_owned().into();
    //     let did = Did::from_str(&address.inner())?;
    //     self.get_subring_for_update(&did, callback)
    // }
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
        let finger = ring.lock_finger()?;
        Ok(Self {
            name: name.to_owned(),
            did,
            finger: (*finger).clone(),
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
            did: ring.did,
            data: vec![data.into()],
            kind: VNodeType::SubRing,
        })
    }
}

impl TryFrom<VirtualNode> for SubRing {
    type Error = Error;
    fn try_from(vnode: VirtualNode) -> Result<Self> {
        match &vnode.kind {
            VNodeType::SubRing => {
                let decoded: String = vnode.data[0].decode()?;
                let subring: SubRing =
                    serde_json::from_str(&decoded).map_err(Error::Deserialize)?;
                Ok(subring)
            }
            _ => Err(Error::InvalidVNodeType),
        }
    }
}
