#![warn(missing_docs)]
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_trait::async_trait;
use itertools::Itertools;
use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

use super::did::BiasId;
use super::successor::Successor;
use super::types::Chord;
use super::types::ChordStabilize;
use super::types::ChordStorage;
use super::vnode::VirtualNode;
use super::FingerTable;
use crate::dht::Did;
use crate::err::Error;
use crate::err::Result;
use crate::storage::MemStorage;
use crate::storage::PersistenceStorage;
use crate::storage::PersistenceStorageReadAndWrite;
// use crate::storage::PersistenceStorageOperation;
use crate::storage::PersistenceStorageRemove;

/// Remote actions
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RemoteAction {
    /// Ask did_a to find did_b
    FindSuccessor(Did),
    /// Ask did_a to find virtual node did_b
    FindVNode(Did),
    /// Ask did_a to find virtual peer for storage
    FindAndStore(VirtualNode),
    /// Ask did_a to find virtual peer for subring joining
    FindAndJoinSubRing(Did),
    /// Ask Did_a to notify(did_b)
    Notify(Did),
    /// Async data with it's successor
    SyncVNodeWithSuccessor(Vec<VirtualNode>),
    /// Find a successor and fix the finger table
    FindSuccessorForFix(Did),
    /// Check predecessor
    CheckPredecessor,
}

/// Result of PeerRing algorithm
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PeerRingAction {
    /// Do noting
    None,
    /// Found some VNode
    SomeVNode(VirtualNode),
    /// Found some node
    Some(Did),
    /// Trigger remote action
    RemoteAction(Did, RemoteAction),
    /// Trigger Multiple Actions at sametime
    MultiActions(Vec<PeerRingAction>),
}

impl PeerRingAction {
    /// action is Self::None
    pub fn is_none(&self) -> bool {
        if let Self::None = self {
            return true;
        }
        false
    }

    /// Action is Self::Some
    pub fn is_some(&self) -> bool {
        if let Self::Some(_) = self {
            return true;
        }
        false
    }

    /// Action is Self::RemoteAction
    pub fn is_remote(&self) -> bool {
        if let Self::RemoteAction(..) = self {
            return true;
        }
        false
    }

    /// Action is Self::MultiActions
    pub fn is_multi(&self) -> bool {
        if let Self::MultiActions(..) = self {
            return true;
        }
        false
    }
}

/// Implementation of PeerRing
#[derive(Clone)]
pub struct PeerRing {
    /// PeerRing's id is address of Node
    pub id: Did,
    /// first node on circle that succeeds (n + 2 ^(k-1) ) mod 2^m , 1 <= k<= m
    /// for index start with 0, it should be (n+2^k) mod 2^m
    pub finger: Arc<Mutex<FingerTable>>,
    /// The next node on the identifier circle; finger[1].node
    pub successor: Arc<Mutex<Successor>>,
    /// The previous node on the identifier circle
    pub predecessor: Arc<Mutex<Option<Did>>>,
    /// LocalStorage for DHT Query
    pub storage: Arc<PersistenceStorage>,
    /// LocalCache
    pub cache: Arc<MemStorage<Did, VirtualNode>>,
}

impl PeerRing {
    /// Create a new Chord ring.
    pub async fn new(id: Did) -> Result<Self> {
        Self::new_with_config(id, 3).await
    }

    /// Create a new Chord Ring with given successor_max, and finger_size
    pub async fn new_with_config(id: Did, succ_max: u8) -> Result<Self> {
        Ok(Self {
            successor: Arc::new(Mutex::new(Successor::new(id, succ_max))),
            predecessor: Arc::new(Mutex::new(None)),
            // for Eth address, it's 160
            finger: Arc::new(Mutex::new(FingerTable::new(id, 160))),
            id,
            storage: Arc::new(PersistenceStorage::new().await?),
            cache: Arc::new(MemStorage::<Did, VirtualNode>::new()),
        })
    }

    /// Init with given Storage
    pub fn new_with_storage(id: Did, storage: Arc<PersistenceStorage>) -> Self {
        Self {
            successor: Arc::new(Mutex::new(Successor::new(id, 3))),
            predecessor: Arc::new(Mutex::new(None)),
            // for Eth address, it's 160
            finger: Arc::new(Mutex::new(FingerTable::new(id, 160))),
            storage: Arc::clone(&storage),
            cache: Arc::new(MemStorage::<Did, VirtualNode>::new()),
            id,
        }
    }

    /// Lock and return MutexGuard of Successor
    pub fn lock_successor(&self) -> Result<MutexGuard<Successor>> {
        self.successor.lock().map_err(|_| Error::DHTSyncLockError)
    }

    /// Lock and return MutexGuard of Finger Table
    pub fn lock_finger(&self) -> Result<MutexGuard<FingerTable>> {
        self.finger.lock().map_err(|_| Error::DHTSyncLockError)
    }

    /// Lock and return MutexGuard of Predecessor
    pub fn lock_predecessor(&self) -> Result<MutexGuard<Option<Did>>> {
        self.predecessor.lock().map_err(|_| Error::DHTSyncLockError)
    }

    /// Get first element from Finger Table
    pub fn first(&self) -> Result<Option<Did>> {
        let finger = self.lock_finger()?;
        Ok(finger.first())
    }

    /// remove a node from dht finger table
    /// remove a node from dht successor table
    /// if suuccessor is empty, set it to the cloest node
    pub fn remove(&self, id: Did) -> Result<()> {
        let mut finger = self.lock_finger()?;
        let mut successor = self.lock_successor()?;
        let mut predecessor = self.lock_predecessor()?;
        if let Some(pid) = *predecessor {
            if pid == id {
                *predecessor = None;
            }
        }
        finger.remove(id);
        successor.remove(id);
        if successor.is_none() {
            if let Some(x) = finger.first() {
                successor.update(x);
            }
        }
        Ok(())
    }

    /// Calculate Bias of the Did on the Ring
    pub fn bias(&self, id: Did) -> BiasId {
        BiasId::new(&self.id, &id)
    }

    /// finger length
    pub fn number_of_fingers(&self) -> Result<usize> {
        let finger = self.lock_finger()?;
        Ok(finger.len())
    }
}

impl Chord<PeerRingAction> for PeerRing {
    /// join a PeerRing ring containing node id .
    fn join(&self, id: Did) -> Result<PeerRingAction> {
        let mut finger = self.lock_finger()?;
        let mut successor = self.lock_successor()?;
        if id == self.id {
            return Ok(PeerRingAction::None);
        }
        finger.join(id);
        if self.bias(id) < self.bias(successor.max()) || successor.is_none() {
            // 1) id should follows self.id
            // 2) #fff should follow #001 because id space is a Finate Ring
            // 3) #001 - #fff = #001 + -(#fff) = #001
            successor.update(id);
            // only triger if successor is updated
        }
        Ok(PeerRingAction::RemoteAction(
            id,
            RemoteAction::FindSuccessor(self.id),
        ))
    }

    /// Fig.5 n.find_successor(id)
    fn find_successor(&self, id: Did) -> Result<PeerRingAction> {
        let successor = self.lock_successor()?;
        let finger = self.lock_finger()?;
        // if (id \in (n; successor]); return successor
        // if ID = N63, Successor = N10
        // N9
        if self.bias(id) <= self.bias(successor.min()) || successor.is_none() {
            //if self.id < id && id <= self.successor {
            // response the closest one
            Ok(PeerRingAction::Some(successor.min()))
        } else {
            // n = closest preceding node(id);
            // return n.find_successor(id);
            let closest = finger.closest(id);
            match closest {
                Ok(n) => Ok(PeerRingAction::RemoteAction(
                    n,
                    RemoteAction::FindSuccessor(id),
                )),
                Err(e) => Err(e),
            }
        }
    }
}

impl ChordStabilize<PeerRingAction> for PeerRing {
    /// n' thinks it might be our predecessor.
    fn notify(&self, id: Did) -> Result<Option<Did>> {
        let mut predecessor = self.lock_predecessor()?;
        let predecessor_value = *predecessor;
        // if (predecessor is nil or n' /in (predecessor; n)); predecessor = n';
        match predecessor_value {
            Some(pre) => {
                // if id <- [pre, self]
                if self.bias(pre) < self.bias(id) {
                    *predecessor = Some(id);
                    Ok(Some(id))
                } else {
                    Ok(None)
                }
            }
            None => {
                *predecessor = Some(id);
                Ok(Some(id))
            }
        }
    }

    /// called periodically. refreshes finger table entries.
    /// next stores the index of the next finger to fix.
    fn fix_fingers(&self) -> Result<PeerRingAction> {
        let mut fix_finger_index = self.lock_finger()?.fix_finger_index;

        // next = next + 1;
        //if (next > m) next = 1;
        // finger[next] = find_successor(n + 2^(next-1) );
        // for index start with 0
        // finger[next] = find_successor(n + 2^(next) );
        fix_finger_index += 1;
        if fix_finger_index >= 159 {
            fix_finger_index = 0;
        }

        let did: BigUint = (BigUint::from(self.id)
            + BigUint::from(2u16).pow(fix_finger_index.into()))
            % BigUint::from(2u16).pow(160);

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
                    log::error!("Invalid PeerRing Action");
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

    /// Fig.5. n.cloest_preceding_node(id)
    /// for i = m downto1
    ///    if (finger\[i\] <- (n, id))
    ///        return finger\[i\]
    /// return n
    fn closest_preceding_node(&self, id: Did) -> Result<Did> {
        let finger = self.lock_finger()?;
        finger.closest(id)
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl ChordStorage<PeerRingAction> for PeerRing {
    /// lookup always check data via finger table
    async fn lookup(&self, vid: &Did) -> Result<PeerRingAction> {
        match self.find_successor(*vid) {
            // if vid is in [self, successor]
            Ok(PeerRingAction::Some(_)) => match self.storage.get(vid).await {
                Ok(v) => Ok(PeerRingAction::SomeVNode(v)),
                Err(_) => Ok(PeerRingAction::None),
            },
            Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(id))) => {
                Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindVNode(id)))
            }
            Ok(a) => Err(Error::PeerRingUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    /// When a vnode data is fetched from remote, it should be cache at local
    fn cache(&self, vnode: VirtualNode) {
        self.cache.set(&vnode.did(), vnode);
    }

    /// When a VNode data is fetched from remote, it should be cache at local
    fn fetch_cache(&self, id: &Did) -> Option<VirtualNode> {
        self.cache.get(id)
    }

    /// If address of VNode is in range(self, successor), it should store locally,
    /// otherwise, it should on remote successor
    async fn store(&self, peer: VirtualNode) -> Result<PeerRingAction> {
        let vid = peer.did();
        // find VNode's closest successor
        match self.find_successor(vid) {
            // if vid is in range(self, successor)
            // self should store it
            Ok(PeerRingAction::Some(_)) => match self.storage.get(&vid).await {
                Ok(v) => {
                    let _ = self
                        .storage
                        .put(&vid, &VirtualNode::concat(&v, &peer)?)
                        .await?;
                    Ok(PeerRingAction::None)
                }
                Err(_) => {
                    let _ = self.storage.put(&vid, &peer).await?;
                    Ok(PeerRingAction::None)
                }
            },
            Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(_))) => Ok(
                PeerRingAction::RemoteAction(n, RemoteAction::FindAndStore(peer)),
            ),
            Ok(a) => Err(Error::PeerRingUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    /// store a vec of data
    async fn store_vec(&self, vps: Vec<VirtualNode>) -> Result<PeerRingAction> {
        let acts: Vec<PeerRingAction> =
            futures::future::join_all(vps.iter().map(|v| self.store(v.clone())).collect_vec())
                .await
                .into_iter()
                // ignore faiure here
                //.filter(|v| v.is_ok())
                .flatten()
                //.map(|v| v.unwrap())
                //.filter(|v| !v.is_none())
                .collect();
        match acts.len() {
            0 => Ok(PeerRingAction::None),
            _ => Ok(PeerRingAction::MultiActions(acts)),
        }
    }

    /// This function should call when successor is updated
    async fn sync_with_successor(&self, new_successor: Did) -> Result<PeerRingAction> {
        let mut data = Vec::<VirtualNode>::new();
        let all_items: Vec<(Did, VirtualNode)> = self.storage.get_all().await?;
        for (k, v) in all_items.iter() {
            // k < self.successor
            if self.bias(*k) < self.bias(new_successor) && self.storage.remove(k).await.is_ok() {
                data.push(v.clone());
            }
        }
        if !data.is_empty() {
            Ok(PeerRingAction::RemoteAction(
                new_successor,
                RemoteAction::SyncVNodeWithSuccessor(data),
            ))
        } else {
            Ok(PeerRingAction::None)
        }
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

        let node_a = PeerRing::new_with_storage(a, Arc::new(db_1));
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
        let node_d = PeerRing::new_with_storage(d, Arc::new(db_2));
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
        let node_d = PeerRing::new_with_storage(d, Arc::new(db_3));
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
        let node1 = PeerRing::new_with_storage(did1, Arc::new(db_1));
        let node2 = PeerRing::new_with_storage(did2, Arc::new(db_2));

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
        let node1 = PeerRing::new_with_storage(did1, Arc::new(db_1));
        let node2 = PeerRing::new_with_storage(did2, Arc::new(db_2));

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
