//! Chord algorithm implement.
#![warn(missing_docs)]
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_trait::async_trait;
use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

use super::did::BiasId;
use super::successor::SuccessorSeq;
use super::types::Chord;
use super::types::ChordStorage;
use super::vnode::VirtualNode;
use super::FingerTable;
use crate::dht::Did;
use crate::err::Error;
use crate::err::Result;
use crate::storage::MemStorage;
use crate::storage::PersistenceStorage;
use crate::storage::PersistenceStorageReadAndWrite;
use crate::storage::PersistenceStorageRemove;

/// PeerRing is used to help a node interact with other nodes.
/// All nodes in rings network form a clockwise ring in the order of Did.
/// This struct takes its name from that.
/// PeerRing implemented [Chord] algorithm.
/// PeerRing implemented [ChordStorage] protocol.
#[derive(Clone)]
pub struct PeerRing {
    /// The did of current node.
    pub did: Did,
    /// [FingerTable] help node to find successor quickly.
    pub finger: Arc<Mutex<FingerTable>>,
    /// The next node on the ring.
    /// The [SuccessorSeq] may contain multiple node dids for fault tolerance.
    /// The min did should be same as the first element in finger table.
    pub successor_seq: Arc<Mutex<SuccessorSeq>>,
    /// The did of previous node on the ring.
    pub predecessor: Arc<Mutex<Option<Did>>>,
    /// Local storage for [ChordStorage].
    pub storage: Arc<PersistenceStorage>,
    /// Local cache for [ChordStorage].
    pub cache: Arc<MemStorage<Did, VirtualNode>>,
}

/// `PeerRing` use this to describe the result of [Chord] algorithm. Sometimes it's a
/// direct result, sometimes it's an action that is continued externally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PeerRingAction {
    /// No result, the whole manipulation is done internally.
    None,
    /// Found some VirtualNode.
    SomeVNode(VirtualNode),
    /// Found some node.
    Some(Did),
    /// Trigger a remote action.
    RemoteAction(Did, RemoteAction),
    /// Trigger multiple remote actions.
    MultiActions(Vec<PeerRingAction>),
}

/// Some of the process needs to be done remotely. This enum is used to describe that.
/// Don't worry about leaving the context. There will be callback machinisim externally
/// that will invoke appropriate methods in `PeerRing` to continue the process.
///
/// To avoid ambiguity, in the following comments, `did_a` is the Did declared in
/// [PeerRingAction::RemoteAction]. Other dids are the fields declared in `RemoteAction`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RemoteAction {
    /// Need `did_a` to find `did_b`.
    FindSuccessor(Did),
    /// Need `did_a` to find virtual node `did_b`.
    FindVNode(Did),
    /// Need `did_a` to find node for storage.
    FindAndStore(VirtualNode),
    /// Need `did_a` to find virtual peer for subring joining.
    FindAndJoinSubRing(Did),
    /// Let `did_a` [notify](Chord::notify) `did_b`.
    Notify(Did),
    /// Let `did_a` sync data with it's successor.
    SyncVNodeWithSuccessor(Vec<VirtualNode>),

    // TODO: Only find_successor method can return FindSuccessor, otherwise should give
    // a FindSuccessor with flag. Such as `FindSuccessorForFix`.
    /// Need `did_a` to find `did_b` then send back with `for finger table fixing` flag.
    FindSuccessorForFix(Did),

    // TODO: The check_processor method is not using. Cannot give correct description.
    /// Check predecessor
    CheckPredecessor,
}

impl PeerRingAction {
    /// Returns `true` if the action is a [PeerRingAction::None] value.
    pub fn is_none(&self) -> bool {
        if let Self::None = self {
            return true;
        }
        false
    }

    /// Returns `true` if the action is a [PeerRingAction::Some] value.
    pub fn is_some(&self) -> bool {
        if let Self::Some(_) = self {
            return true;
        }
        false
    }

    /// Returns `true` if the action is a [PeerRingAction::RemoteAction] value.
    pub fn is_remote(&self) -> bool {
        if let Self::RemoteAction(..) = self {
            return true;
        }
        false
    }

    /// Returns `true` if the action is a [PeerRingAction::MultiActions] value.
    pub fn is_multi(&self) -> bool {
        if let Self::MultiActions(..) = self {
            return true;
        }
        false
    }
}

impl PeerRing {
    /// Create a new Chord ring.
    pub async fn new(did: Did) -> Result<Self> {
        Self::new_with_config(did, 3).await
    }

    /// Create a new Chord Ring with given successor_seq max num, and finger_size.
    pub async fn new_with_config(did: Did, succ_max: u8) -> Result<Self> {
        Ok(Self {
            successor_seq: Arc::new(Mutex::new(SuccessorSeq::new(did, succ_max))),
            predecessor: Arc::new(Mutex::new(None)),
            // for Eth address, it's 160
            finger: Arc::new(Mutex::new(FingerTable::new(did, 160))),
            did,
            storage: Arc::new(PersistenceStorage::new().await?),
            cache: Arc::new(MemStorage::<Did, VirtualNode>::new()),
        })
    }

    /// Same as new with config, but with a given storage.
    pub fn new_with_storage(did: Did, succ_max: u8, storage: PersistenceStorage) -> Self {
        Self {
            successor_seq: Arc::new(Mutex::new(SuccessorSeq::new(did, succ_max))),
            predecessor: Arc::new(Mutex::new(None)),
            // for Eth address, it's 160
            finger: Arc::new(Mutex::new(FingerTable::new(did, 160))),
            storage: Arc::new(storage),
            cache: Arc::new(MemStorage::<Did, VirtualNode>::new()),
            did,
        }
    }

    /// Lock and return MutexGuard of successor sequence.
    pub fn lock_successor(&self) -> Result<MutexGuard<SuccessorSeq>> {
        self.successor_seq
            .lock()
            .map_err(|_| Error::DHTSyncLockError)
    }

    /// Lock and return MutexGuard of finger table.
    pub fn lock_finger(&self) -> Result<MutexGuard<FingerTable>> {
        self.finger.lock().map_err(|_| Error::DHTSyncLockError)
    }

    /// Lock and return MutexGuard of predecessor.
    pub fn lock_predecessor(&self) -> Result<MutexGuard<Option<Did>>> {
        self.predecessor.lock().map_err(|_| Error::DHTSyncLockError)
    }

    /// Remove a node from finger table.
    /// Also remove it from successor sequence.
    /// If successor_seq become empty, try setting the closest node to it.
    pub fn remove(&self, did: Did) -> Result<()> {
        let mut finger = self.lock_finger()?;
        let mut successor = self.lock_successor()?;
        let mut predecessor = self.lock_predecessor()?;
        if let Some(pid) = *predecessor {
            if pid == did {
                *predecessor = None;
            }
        }
        finger.remove(did);
        successor.remove(did);
        if successor.is_none() {
            if let Some(x) = finger.first() {
                successor.update(x);
            }
        }
        Ok(())
    }

    /// Calculate bias of the Did on the ring.
    pub fn bias(&self, did: Did) -> BiasId {
        BiasId::new(self.did, did)
    }
}

impl Chord<PeerRingAction> for PeerRing {
    /// Join another Did into the ring.
    fn join(&self, did: Did) -> Result<PeerRingAction> {
        if did == self.did {
            return Ok(PeerRingAction::None);
        }

        let mut finger = self.lock_finger()?;
        let mut successor = self.lock_successor()?;

        finger.join(did);

        if self.bias(did) < self.bias(successor.max()) || successor.is_none() {
            // 1) id should follows self.id
            // 2) #fff should follow #001 because id space is a Finate Ring
            // 3) #001 - #fff = #001 + -(#fff) = #001
            successor.update(did);
        }

        Ok(PeerRingAction::RemoteAction(
            did,
            // TODO: should be `FindSuccessorForConnect`.
            RemoteAction::FindSuccessor(self.did),
        ))
    }

    /// Find the successor of a Did.
    /// May return a remote action for the successor is recorded in another node.
    fn find_successor(&self, did: Did) -> Result<PeerRingAction> {
        let successor = self.lock_successor()?;
        let finger = self.lock_finger()?;

        if successor.is_none() || self.bias(did) <= self.bias(successor.min()) {
            // If the did is closer to self than successor, return successor as the
            // successor of that did.
            Ok(PeerRingAction::Some(successor.min()))
        } else {
            // Otherwise, find the closest preceding node and ask it to find the successor.
            let closest = finger.closest(did);
            Ok(PeerRingAction::RemoteAction(
                closest,
                RemoteAction::FindSuccessor(did),
            ))
        }
    }

    /// Handle notification from a node that thinks it is the predecessor of current node.
    /// The `did` in parameters is the Did of that node.
    /// If that node is closer to current node or current node has no predecessor, set it to the did.
    /// This method will return that did if it is set to the predecessor.
    fn notify(&self, did: Did) -> Result<Option<Did>> {
        let mut predecessor = self.lock_predecessor()?;

        match *predecessor {
            Some(pre) => {
                // If the did is closer to self than predecessor, set it to the predecessor.
                if self.bias(pre) < self.bias(did) {
                    *predecessor = Some(did);
                    Ok(Some(did))
                } else {
                    Ok(None)
                }
            }
            None => {
                // Self has no predecessor, set it to the did directly.
                *predecessor = Some(did);
                Ok(Some(did))
            }
        }
    }

    /// Fix finger table by finding the successor for each finger.
    /// According to the paper, this method should be called periodically.
    /// According to the paper, only one finger should be fixed at a time.
    fn fix_fingers(&self) -> Result<PeerRingAction> {
        let mut fix_finger_index = self.lock_finger()?.fix_finger_index;

        // Only one finger should be fixed at a time.
        fix_finger_index += 1;
        if fix_finger_index >= 159 {
            fix_finger_index = 0;
        }

        // Get finger did.
        let did: BigUint = (BigUint::from(self.did)
            + BigUint::from(2u16).pow(fix_finger_index.into()))
            % BigUint::from(2u16).pow(160);

        // Caution here that there are also locks in find_successor.
        // You cannot lock finger table before calling find_successor.
        // Have to lock_finger in each branch of the match.
        match self.find_successor(did.into()) {
            Ok(res) => match res {
                PeerRingAction::Some(v) => {
                    let mut finger = self.lock_finger()?;
                    finger.fix_finger_index = fix_finger_index;
                    finger.set_fix(v);
                    Ok(PeerRingAction::None)
                }
                PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessor(b)) => {
                    let mut finger = self.lock_finger()?;
                    finger.fix_finger_index = fix_finger_index;
                    Ok(PeerRingAction::RemoteAction(
                        a,
                        RemoteAction::FindSuccessorForFix(b),
                    ))
                }
                _ => {
                    let mut finger = self.lock_finger()?;
                    finger.fix_finger_index = fix_finger_index;
                    tracing::error!("Invalid PeerRing Action");
                    Err(Error::PeerRingInvalidAction)
                }
            },
            Err(e) => {
                let mut finger = self.lock_finger()?;
                finger.fix_finger_index = fix_finger_index;
                Err(Error::PeerRingFindSuccessor(e.to_string()))
            }
        }
    }

    /// called periodically. checks whether predecessor has failed.
    fn check_predecessor(&self) -> Result<PeerRingAction> {
        let predecessor = *self.lock_predecessor()?;
        Ok(match predecessor {
            Some(p) => PeerRingAction::RemoteAction(p, RemoteAction::CheckPredecessor),
            None => PeerRingAction::None,
        })
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorage<PeerRingAction> for PeerRing {
    /// Look up a VirtualNode by its Did.
    /// Always finds resource by finger table, ignoring the local cache.
    /// If the `vid` is between current node and its successor, its resource should be
    /// stored in current node.
    async fn lookup(&self, vid: Did) -> Result<PeerRingAction> {
        match self.find_successor(vid) {
            // Resource should be stored in current node.
            Ok(PeerRingAction::Some(_)) => match self.storage.get(&vid).await {
                Ok(v) => Ok(PeerRingAction::SomeVNode(v)),
                Err(_) => Ok(PeerRingAction::None),
            },
            // Resource is stored in other nodes.
            // Return an action to describe how to find it.
            Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(id))) => {
                Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindVNode(id)))
            }
            Ok(a) => Err(Error::PeerRingUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    /// Store `vnode` if it's between current node and the successor of current node,
    /// otherwise find the responsible node and return as Action.
    async fn store(&self, vnode: VirtualNode) -> Result<PeerRingAction> {
        let vid = vnode.did;
        match self.find_successor(vid) {
            // `vnode` should be stored in current node.
            Ok(PeerRingAction::Some(_)) => match self.storage.get(&vid).await {
                Ok(v) => {
                    let _ = self
                        .storage
                        .put(&vid, &VirtualNode::concat(&v, &vnode)?)
                        .await?;
                    Ok(PeerRingAction::None)
                }
                Err(_) => {
                    let _ = self.storage.put(&vid, &vnode).await?;
                    Ok(PeerRingAction::None)
                }
            },
            // `vnode` should be stored in other nodes.
            // Return an action to describe how to store it.
            Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(_))) => Ok(
                PeerRingAction::RemoteAction(n, RemoteAction::FindAndStore(vnode)),
            ),
            Ok(a) => Err(Error::PeerRingUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    /// When the successor of a node is updated, it needs to check if there are
    /// `VirtualNode`s that are no longer between current node and `new_successor`,
    /// and sync them to the new successor.
    async fn sync_with_successor(&self, new_successor: Did) -> Result<PeerRingAction> {
        let mut data = Vec::<VirtualNode>::new();
        let all_items: Vec<(Did, VirtualNode)> = self.storage.get_all().await?;

        // Pop out all items that are not between current node and `new_successor`.
        for (vid, vnode) in all_items.iter() {
            if self.bias(*vid) > self.bias(new_successor) && self.storage.remove(vid).await.is_ok()
            {
                data.push(vnode.clone());
            }
        }

        if !data.is_empty() {
            Ok(PeerRingAction::RemoteAction(
                new_successor,
                RemoteAction::SyncVNodeWithSuccessor(data), // TODO: This might be too large.
            ))
        } else {
            Ok(PeerRingAction::None)
        }
    }

    /// Cache fetched `vnode` locally.
    fn local_cache_set(&self, vnode: VirtualNode) {
        self.cache.set(&vnode.did.clone(), vnode);
    }

    /// Get vnode from local cache.
    fn local_cache_get(&self, vid: Did) -> Option<VirtualNode> {
        self.cache.get(&vid)
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::ecc::SecretKey;

    #[tokio::test]
    async fn test_chord_finger() -> Result<()> {
        let db_path_a = PersistenceStorage::random_path("./tmp");
        let db_path_b = PersistenceStorage::random_path("./tmp");
        let db_path_c = PersistenceStorage::random_path("./tmp");
        let a = Did::from_str("0x00E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0x119999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xccffee254729296a45a3885639AC7E10F9d54979").unwrap();
        let d = Did::from_str("0xffffee254729296a45a3885639AC7E10F9d54979").unwrap();

        let db_1 = PersistenceStorage::new_with_path(db_path_a.as_str())
            .await
            .unwrap();
        let db_2 = PersistenceStorage::new_with_path(db_path_b.as_str())
            .await
            .unwrap();
        let db_3 = PersistenceStorage::new_with_path(db_path_c.as_str())
            .await
            .unwrap();

        assert!(a < b && b < c);
        // distence between (a, d) is less than (b, d)
        assert!((a - d) < (b - d));

        let node_a = PeerRing::new_with_storage(a, 3, db_1);
        assert_eq!(
            node_a.lock_successor()?.list(),
            vec![],
            "{:?}",
            node_a.lock_successor()?.list()
        );
        // for increase seq join
        node_a.join(a)?;
        // Node A wont add self to finder
        assert!(node_a.lock_finger()?.is_empty());
        node_a.join(b)?;
        // b is very far away from a
        // a.finger should store did as range
        // [(a, a+2), (a+2, a+4), (a+4, a+8), ..., (a+2^159, a + 2^160)]
        // b is in range(a+2^156, a+2^157)
        assert!(BigUint::from(b) > BigUint::from(2u16).pow(156));
        assert!(BigUint::from(b) < BigUint::from(2u16).pow(157));
        // Node A's finter should be [None, .., B]
        assert!(node_a.lock_finger()?.contains(&Some(b)));
        assert!(
            node_a.lock_finger()?.contains(&None),
            "{:?}",
            node_a.lock_finger()?.list()
        );

        assert_eq!(
            node_a.lock_successor()?.list(),
            vec![b],
            "{:?}",
            node_a.lock_successor()?.list()
        );

        // Node A starts to query node b for it's successor
        assert_eq!(
            node_a.join(b)?,
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(a))
        );
        assert!(node_a.lock_successor()?.list().contains(&b));
        // Node A keep querying node b for it's successor
        assert_eq!(
            node_a.join(c)?,
            PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessor(a))
        );

        // Node A's finter should be [None, ..B, C]
        assert!(
            node_a.lock_finger()?.contains(&Some(c)),
            "{:?}",
            node_a.finger
        );
        // c is in range(a+2^159, a+2^160)
        assert!(BigUint::from(c) > BigUint::from(2u16).pow(159));
        assert!(BigUint::from(c) < BigUint::from(2u16).pow(160));

        assert_eq!(node_a.lock_finger()?[158], Some(c));
        assert_eq!(node_a.lock_finger()?[155], Some(b));
        assert_eq!(node_a.lock_finger()?[156], Some(b));

        assert!(node_a.lock_successor()?.list().contains(&b));
        // Node A will query c to find d
        assert_eq!(
            node_a.find_successor(d).unwrap(),
            PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_a.find_successor(c).unwrap(),
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(c))
        );

        // for decrease seq join
        let node_d = PeerRing::new_with_storage(d, 3, db_2);
        assert_eq!(
            node_d.join(c)?,
            PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_d.join(b)?,
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_d.join(a)?,
            PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
        );

        // for over half ring join
        let node_d = PeerRing::new_with_storage(d, 3, db_3);
        assert_eq!(
            node_d.join(a)?,
            PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
        );
        // for a ring a, a is over 2^152 far away from d
        assert!(d + Did::from(BigUint::from(2u16).pow(152)) > a);
        assert!(d + Did::from(BigUint::from(2u16).pow(151)) < a);
        assert!(node_d.lock_finger()?.contains(&Some(a)));
        assert_eq!(node_d.lock_finger()?[151], Some(a));
        assert_eq!(node_d.lock_finger()?[152], None);
        assert_eq!(node_d.lock_finger()?[0], Some(a));
        // when b insearted a is still more close to d
        assert_eq!(
            node_d.join(b)?,
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(d))
        );
        assert!(d + Did::from(BigUint::from(2u16).pow(159)) > b);
        assert!(node_d.lock_successor()?.list().contains(&a));
        tokio::fs::remove_dir_all("./tmp").await.ok();

        Ok(())
    }

    #[tokio::test]
    async fn test_two_node_finger() -> Result<()> {
        let mut key1 = SecretKey::random();
        let mut key2 = SecretKey::random();
        if key1.address() > key2.address() {
            (key1, key2) = (key2, key1)
        }
        let did1: Did = key1.address().into();
        let did2: Did = key2.address().into();
        let db_path1 = PersistenceStorage::random_path("./tmp");
        let db_path2 = PersistenceStorage::random_path("./tmp");
        let db_1 = PersistenceStorage::new_with_path(db_path1.as_str())
            .await
            .unwrap();
        let db_2 = PersistenceStorage::new_with_path(db_path2.as_str())
            .await
            .unwrap();
        let node1 = PeerRing::new_with_storage(did1, 3, db_1);
        let node2 = PeerRing::new_with_storage(did2, 3, db_2);

        node1.join(did2)?;
        node2.join(did1)?;
        assert!(node1.lock_successor()?.list().contains(&did2));
        assert!(node2.lock_successor()?.list().contains(&did1));

        assert!(
            node1.lock_finger()?.contains(&Some(did2)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
        assert!(
            node2.lock_finger()?.contains(&Some(did1)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
        tokio::fs::remove_dir_all("./tmp").await.ok();

        Ok(())
    }

    #[tokio::test]
    async fn test_two_node_finger_failed_case() -> Result<()> {
        let did1 = Did::from_str("0x051cf4f8d020cb910474bef3e17f153fface2b5f").unwrap();
        let did2 = Did::from_str("0x54baa7dc9e28f41da5d71af8fa6f2a302be1c1bf").unwrap();
        let max = Did::from(BigUint::from(2u16).pow(160) - 1u16);
        let zero = Did::from(BigUint::from(2u16).pow(160));

        let db_path1 = PersistenceStorage::random_path("./tmp");
        let db_path2 = PersistenceStorage::random_path("./tmp");
        let db_1 = PersistenceStorage::new_with_path(db_path1.as_str())
            .await
            .unwrap();
        let db_2 = PersistenceStorage::new_with_path(db_path2.as_str())
            .await
            .unwrap();
        let node1 = PeerRing::new_with_storage(did1, 3, db_1);
        let node2 = PeerRing::new_with_storage(did2, 3, db_2);

        node1.join(did2)?;
        node2.join(did1)?;
        assert!(node1.lock_successor()?.list().contains(&did2));
        assert!(node2.lock_successor()?.list().contains(&did1));
        let pos_159 = did2 + Did::from(BigUint::from(2u16).pow(159));
        assert!(pos_159 > did2);
        assert!(pos_159 < max, "{:?};{:?}", pos_159, max);
        let pos_160 = did2 + zero;
        assert_eq!(pos_160, did2);
        assert!(pos_160 > did1);

        assert!(
            node1.lock_finger()?.contains(&Some(did2)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
        assert!(
            node2.lock_finger()?.contains(&Some(did1)),
            "did2:{:?} dont contains did1:{:?}",
            did2,
            did1
        );
        tokio::fs::remove_dir_all("./tmp").await.ok();

        Ok(())
    }
}
