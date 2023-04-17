//! Chord algorithm implement.
#![warn(missing_docs)]
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_trait::async_trait;
use num_bigint::BigUint;

use super::did::BiasId;
use super::successor::SuccessorSeq;
use super::types::Chord;
use super::types::ChordStorage;
use super::types::CorrectChord;
use super::vnode::VNodeOperation;
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
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteAction {
    /// Need `did_a` to find `did_b`.
    FindSuccessor(Did),
    /// Need `did_a` to find virtual node `did_b`.
    FindVNode(Did),
    /// Need `did_a` to find VirtualNode for operating.
    FindVNodeForOperate(VNodeOperation),
    /// Let `did_a` [notify](Chord::notify) `did_b`.
    Notify(Did),
    /// Let `did_a` sync data with it's successor.
    SyncVNodeWithSuccessor(Vec<VirtualNode>),

    /// Need `did_a` to find `did_b` then send back with `for connect` flag.
    FindSuccessorForConnect(Did),

    /// Need `did_a` to find `did_b` then send back with `for finger table fixing` flag.
    FindSuccessorForFix(Did),

    // TODO: The check_processor method is not using. Cannot give correct description.
    /// Check predecessor
    CheckPredecessor,

    /// Fetch successor_list from successor
    QueryForSuccessorList,
    /// Fetch successor_list and pred from successor
    QueryForSuccessorListAndPred,
}

/// Information about successor and predecessor
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopoInfo {
    succ_list: Vec<Did>,
    pred: Option<Did>,
}

impl TryFrom<PeerRing> for TopoInfo {
    type Error = Error;
    fn try_from(dht: PeerRing) -> Result<TopoInfo> {
        let succ_list = dht.lock_successor()?.list();
        let pred = *dht.lock_predecessor()?;
        Ok(TopoInfo { succ_list, pred })
    }
}

impl TryFrom<&PeerRing> for TopoInfo {
    type Error = Error;
    fn try_from(dht: &PeerRing) -> Result<TopoInfo> {
        let succ_list = dht.lock_successor()?.list();
        let pred = *dht.lock_predecessor()?;
        Ok(TopoInfo { succ_list, pred })
    }
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
        if successor.is_empty() {
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
    /// Join a ring containing a node identified by `did`.
    /// This method is usually invoked to maintain successor sequence and finger table
    /// after connect to another node.
    ///
    /// This method will return a [RemoteAction::FindSuccessorForConnect] to the caller.
    /// The caller will send it to the node identified by `did`, and let the node find
    /// the successor of current node and make current node connect to that successor.
    fn join(&self, did: Did) -> Result<PeerRingAction> {
        if did == self.did {
            return Ok(PeerRingAction::None);
        }

        let mut finger = self.lock_finger()?;
        let mut successor = self.lock_successor()?;

        finger.join(did);

        if self.bias(did) < self.bias(successor.max()) || !successor.is_full() {
            // 1) id should follows self.id
            // 2) #fff should follow #001 because id space is a Finate Ring
            // 3) #001 - #fff = #001 + -(#fff) = #001
            successor.update(did);
        }

        Ok(PeerRingAction::RemoteAction(
            did,
            RemoteAction::FindSuccessorForConnect(self.did),
        ))
    }

    /// Find the successor of a Did.
    /// May return a remote action for the successor is recorded in another node.
    fn find_successor(&self, did: Did) -> Result<PeerRingAction> {
        let successor = self.lock_successor()?;
        let finger = self.lock_finger()?;

        let succ = {
            if successor.is_empty() || self.bias(did) <= self.bias(successor.min()) {
                // If the did is closer to self than successor, return successor as the
                // successor of that did.
                Ok(PeerRingAction::Some(successor.min()))
            } else {
                // Otherwise, find the closest preceding node and ask it to find the successor.
                let closest_predecessor = finger.closest_predecessor(did);
                Ok(PeerRingAction::RemoteAction(
                    closest_predecessor,
                    RemoteAction::FindSuccessor(did),
                ))
            }
        };

        tracing::debug!(
            "find_successor: self: {}, did: {}, successor: {:?}, result: {:?}",
            self.did,
            did,
            successor,
            succ
        );

        succ
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
        fix_finger_index = (fix_finger_index + 1) % 160;

        // Get finger did.
        let finger_did = Did::from(BigUint::from(2u16).pow(fix_finger_index as u32));

        // Caution here that there are also locks in find_successor.
        // You cannot lock finger table before calling find_successor.
        // Have to lock_finger in each branch of the match.
        match self.find_successor(finger_did) {
            Ok(res) => match res {
                PeerRingAction::Some(v) => {
                    let mut finger = self.lock_finger()?;
                    finger.fix_finger_index = fix_finger_index;
                    finger.set_fix(v);
                    Ok(PeerRingAction::None)
                }
                PeerRingAction::RemoteAction(
                    closest_predecessor,
                    RemoteAction::FindSuccessor(finger_did),
                ) => {
                    let mut finger = self.lock_finger()?;
                    finger.fix_finger_index = fix_finger_index;
                    Ok(PeerRingAction::RemoteAction(
                        closest_predecessor,
                        RemoteAction::FindSuccessorForFix(finger_did),
                    ))
                }
                _ => {
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
    async fn vnode_lookup(&self, vid: Did) -> Result<PeerRingAction> {
        match self.find_successor(vid) {
            // Resource should be stored in current node.
            Ok(PeerRingAction::Some(succ)) => match self.storage.get(&vid).await {
                Ok(Some(v)) => Ok(PeerRingAction::SomeVNode(v)),
                Ok(None) => {
                    tracing::debug!(
                        "Cannot find vnode in local storage, try to query from successor"
                    );
                    // If cannot find and has successor, try to query it from successor.
                    // This is useful when the node is just joined and has not stabilized yet.
                    if succ == self.did {
                        Ok(PeerRingAction::None)
                    } else {
                        Ok(PeerRingAction::RemoteAction(
                            succ,
                            RemoteAction::FindVNode(vid),
                        ))
                    }
                }
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

    /// Handle [VNodeOperation] if the target vnode between current node and the
    /// successor of current node, otherwise find the responsible node and return
    /// as Action.
    async fn vnode_operate(&self, op: VNodeOperation) -> Result<PeerRingAction> {
        let vid = op.did()?;
        let op1 = op.clone();
        match self.find_successor(vid) {
            // `vnode` should be on current node.
            Ok(PeerRingAction::Some(_)) => {
                let this = if let Ok(Some(this)) = self.storage.get(&vid).await {
                    Ok(this)
                } else {
                    op1.gen_default_vnode()
                }?;
                let vnode = this.operate(op)?;
                self.storage.put(&vid, &vnode).await?;

                Ok(PeerRingAction::None)
            }
            // `vnode` should be on other nodes.
            // Return an action to describe how to store it.
            Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(_))) => Ok(
                PeerRingAction::RemoteAction(n, RemoteAction::FindVNodeForOperate(op)),
            ),
            Ok(a) => Err(Error::PeerRingUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    /// When the successor of a node is updated, it needs to check if there are
    /// `VirtualNode`s that are no longer between current node and `new_successor`,
    /// and sync them to the new successor.
    async fn sync_vnode_with_successor(&self, new_successor: Did) -> Result<PeerRingAction> {
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
impl CorrectChord<PeerRingAction> for PeerRing {
    /// Join Operation
    fn join_then_sync(&self, did: Did) -> Result<PeerRingAction> {
        let act = self.join(did)?;
        Ok(PeerRingAction::MultiActions(vec![
            act,
            PeerRingAction::RemoteAction(did, RemoteAction::QueryForSuccessorList),
        ]))
    }

    /// Rectify Operation, should precheck that Did is connected
    fn rectify(&self, pred: Did) -> Result<()> {
        self.notify(pred)?;
        Ok(())
    }

    fn pre_stabilize(&self) -> Result<PeerRingAction> {
        let successor = self.lock_successor()?;
        if successor.is_empty() {
            return Ok(PeerRingAction::None);
        }
        let head = successor.min();
        Ok(PeerRingAction::RemoteAction(
            head,
            RemoteAction::QueryForSuccessorListAndPred,
        ))
    }

    /// Get topologic info
    fn topoinfo(&self) -> Result<TopoInfo> {
        self.try_into()
    }

    /// Stabilize operation for successor list
    fn stabilize(&self, succ: TopoInfo) -> Result<PeerRingAction> {
        let mut ret = vec![];
        let mut successors = self.lock_successor()?;
        let but_last = &succ.succ_list[..succ.succ_list.len() - 1].to_vec();
        if let Some(new_succ) = succ.pred {
            successors.update(new_succ);
        }

        successors.extend(but_last);
        // between(myIdent, newSucc, head(succList))
        if let Some(new_succ) = succ.pred {
            if self.bias(new_succ) < self.bias(successors.min()) {
                // query newSucc for newSucc.succList
                ret.push(PeerRingAction::RemoteAction(
                    new_succ,
                    RemoteAction::QueryForSuccessorList,
                ));
            }
            ret.push(PeerRingAction::RemoteAction(
                successors.min(),
                RemoteAction::Notify(self.did),
            ));
        }
        Ok(PeerRingAction::MultiActions(ret))
    }
}

#[cfg(not(feature = "wasm"))]
#[cfg(test)]
mod tests {
    use std::iter::repeat;
    use std::str::FromStr;

    use super::*;
    use crate::ecc::SecretKey;
    use crate::tests::default::gen_sorted_dht;

    #[tokio::test]
    async fn test_chord_finger() -> Result<()> {
        let db_path_a = PersistenceStorage::random_path("./tmp");
        let db_path_b = PersistenceStorage::random_path("./tmp");
        let db_path_c = PersistenceStorage::random_path("./tmp");

        let db_1 = PersistenceStorage::new_with_path(db_path_a.as_str())
            .await
            .unwrap();
        let db_2 = PersistenceStorage::new_with_path(db_path_b.as_str())
            .await
            .unwrap();
        let db_3 = PersistenceStorage::new_with_path(db_path_c.as_str())
            .await
            .unwrap();

        // Setup did a, b, c, d in a clockwise order.
        let a = Did::from_str("0x00E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0x119999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xccffee254729296a45a3885639AC7E10F9d54979").unwrap();
        let d = Did::from_str("0xffffee254729296a45a3885639AC7E10F9d54979").unwrap();

        // This assertion tells you the order of a, b, c, d on the ring.
        // Note that this vec only describes the order, not the absolute position.
        // Since they are all on the ring, you cannot say a is the first element or d is
        // the last. You can only describe their bias based on the same node and a
        // clockwise order.
        //
        // a --> b --> c --> d
        // ^                 |
        // |-----------------|
        //
        let mut seq = vec![a, b, c, d];
        seq.sort();
        assert_eq!(seq, vec![a, b, c, d]);

        // Setup node_a and ensure its successor sequence and finger table is empty.
        let node_a = PeerRing::new_with_storage(a, 3, db_1);
        assert!(node_a.lock_successor()?.is_empty());
        assert!(node_a.lock_finger()?.is_empty());

        // Test a node won't set itself to successor sequence and finger table.
        assert_eq!(node_a.join(a)?, PeerRingAction::None);
        assert!(node_a.lock_successor()?.is_empty());
        assert!(node_a.lock_finger()?.is_empty());

        // Test join ring with node_b.
        // We don't need to setup node_b here, we just use its did.
        let result = node_a.join(b)?;

        // After join, node_a should ask node_b to find its successor on the ring for
        // connecting.
        assert_eq!(
            result,
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessorForConnect(a))
        );

        // This assertion tells you the position of node_b on the ring.
        // Hint: The Did type is a 160-bit unsigned integer.
        assert!(BigUint::from(b) > BigUint::from(2u16).pow(156));
        assert!(BigUint::from(b) < BigUint::from(2u16).pow(157));

        // After join, the finger table of node_a should be like:
        // [b] * 157 + [None] * 3
        let mut expected_finger_list = repeat(Some(b)).take(157).collect::<Vec<_>>();
        expected_finger_list.extend(repeat(None).take(3));
        assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);

        // After join, the successor sequence of node_a should be [b].
        assert_eq!(node_a.lock_successor()?.list(), vec![b]);

        // Test repeated join.
        node_a.join(b)?;
        assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
        assert_eq!(node_a.lock_successor()?.list(), vec![b]);
        node_a.join(b)?;
        assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
        assert_eq!(node_a.lock_successor()?.list(), vec![b]);

        // Test join ring with node_c.
        // We don't need to setup node_c here, we just use its did.
        let result = node_a.join(c)?;

        // Again, after join, node_a should ask node_c to find its successor on the ring
        // for connecting.
        assert_eq!(
            result,
            PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessorForConnect(a))
        );

        // This assertion tells you the position of node_c on the ring.
        // Hint: The Did type is a 160-bit unsigned integer.
        assert!(BigUint::from(c) > BigUint::from(2u16).pow(159));
        assert!(BigUint::from(c) < BigUint::from(2u16).pow(160));

        // After join, the finger table of node_a should be like:
        // [b] * 157 + [c] * 3
        let mut expected_finger_list = repeat(Some(b)).take(157).collect::<Vec<_>>();
        expected_finger_list.extend(repeat(Some(c)).take(3));
        assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);

        // After join, the successor sequence of node_a should be [b, c].
        // Because although node_b is closer to node_a, the sequence is not full.
        assert_eq!(node_a.lock_successor()?.list(), vec![b, c]);

        // When try to find_successor of node_d, node_a will send query to node_c.
        assert_eq!(
            node_a.find_successor(d).unwrap(),
            PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
        );
        // When try to find_successor of node_c, node_a will send query to node_b.
        assert_eq!(
            node_a.find_successor(c).unwrap(),
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(c))
        );

        // Since the test above is clockwise, we need to test anti-clockwise situation.
        let node_a = PeerRing::new_with_storage(a, 3, db_2);

        // Test join ring with node_c.
        assert_eq!(
            node_a.join(c)?,
            PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessorForConnect(a))
        );
        let expected_finger_list = repeat(Some(c)).take(160).collect::<Vec<_>>();
        assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
        assert_eq!(node_a.lock_successor()?.list(), vec![c]);

        // Test join ring with node_b.
        assert_eq!(
            node_a.join(b)?,
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessorForConnect(a))
        );
        let mut expected_finger_list = repeat(Some(b)).take(157).collect::<Vec<_>>();
        expected_finger_list.extend(repeat(Some(c)).take(3));
        assert_eq!(node_a.lock_finger()?.list(), &expected_finger_list);
        assert_eq!(node_a.lock_successor()?.list(), vec![b, c]);

        // Test join over half ring.
        let node_d = PeerRing::new_with_storage(d, 1, db_3);
        assert_eq!(
            node_d.join(a)?,
            PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessorForConnect(d))
        );

        // This assertion tells you that node_a is over 2^151 far away from node_d.
        // And node_a is also less than 2^152 far away from node_d.
        assert!(d + Did::from(BigUint::from(2u16).pow(151)) < a);
        assert!(d + Did::from(BigUint::from(2u16).pow(152)) > a);

        // After join, the finger table of node_d should be like:
        // [a] * 152 + [None] * 8
        let mut expected_finger_list = repeat(Some(a)).take(152).collect::<Vec<_>>();
        expected_finger_list.extend(repeat(None).take(8));
        assert_eq!(node_d.lock_finger()?.list(), &expected_finger_list);

        // After join, the successor sequence of node_a should be [a].
        assert_eq!(node_d.lock_successor()?.list(), vec![a]);

        // Test join ring with node_b.
        assert_eq!(
            node_d.join(b)?,
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessorForConnect(d))
        );

        // This assertion tells you that node_b is over 2^156 far away from node_d.
        // And node_b is also less than 2^157 far away from node_d.
        assert!(d + Did::from(BigUint::from(2u16).pow(156)) < b);
        assert!(d + Did::from(BigUint::from(2u16).pow(157)) > b);

        // After join, the finger table of node_d should be like:
        // [a] * 152 + [b] * 5 + [None] * 3
        let mut expected_finger_list = repeat(Some(a)).take(152).collect::<Vec<_>>();
        expected_finger_list.extend(repeat(Some(b)).take(5));
        expected_finger_list.extend(repeat(None).take(3));
        assert_eq!(node_d.lock_finger()?.list(), &expected_finger_list);

        // Note the max successor sequence size of node_d is set to 1 when created.
        // After join, the successor sequence of node_a should still be [a].
        // Because node_a is closer to node_d, and the sequence is full.
        assert_eq!(node_d.lock_successor()?.list(), vec![a]);

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
            node1.lock_finger()?.contains(Some(did2)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
        assert!(
            node2.lock_finger()?.contains(Some(did1)),
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
            node1.lock_finger()?.contains(Some(did2)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
        assert!(
            node2.lock_finger()?.contains(Some(did1)),
            "did2:{:?} dont contains did1:{:?}",
            did2,
            did1
        );
        tokio::fs::remove_dir_all("./tmp").await.ok();

        Ok(())
    }

    /// Test Correct Chord implementation
    #[tokio::test]
    async fn test_correct_chord_impl() -> Result<()> {
        fn assert_successor(dht: &PeerRing, did: &Did) -> bool {
            let succ_list = dht.lock_successor().unwrap();
            succ_list.list().contains(did)
        }

        /// check that two dht is mutual successors
        fn check_is_mutual_successors(dht1: &PeerRing, dht2: &PeerRing) {
            let succ_list_1 = dht1.lock_successor().unwrap();
            let succ_list_2 = dht2.lock_successor().unwrap();
            assert_eq!(succ_list_1.min(), dht2.did);
            assert_eq!(succ_list_2.min(), dht1.did);
        }

        fn check_succ_is_including(dht: &PeerRing, dids: Vec<Did>) {
            let succ_list = dht.lock_successor().unwrap();
            for did in dids {
                assert!(succ_list.list().contains(&did));
            }
        }

        let dhts = gen_sorted_dht(5).await;
        let (n1, n2, n3, n4, n5) = (
            dhts[0].clone(),
            dhts[1].clone(),
            dhts[2].clone(),
            dhts[3].clone(),
            dhts[4].clone(),
        );
        // we now have:
        // n1 < n2 < n3 < n4

        // n1 join n2
        n1.join(n2.did).unwrap();
        n2.join(n1.did).unwrap();
        // for now n1, n2 are `mutual successors`.
        check_is_mutual_successors(&n1, &n2);
        // n1 join n3

        n1.join(n3.did).unwrap();
        n1.join(n4.did).unwrap();
        // for now n1's successor should include n1 and n3
        check_succ_is_including(&n1, vec![n2.did, n3.did, n4.did]);

        n1.join(n5.did).unwrap();
        // n5 is not in n1's successor list
        assert!(!assert_successor(&n1, &n5.did));
        if let PeerRingAction::MultiActions(rets) = n5.join_then_sync(n1.did).unwrap() {
            for r in rets {
                if let PeerRingAction::RemoteAction(t, _) = r {
                    assert_eq!(t, n1.did.clone())
                } else {
                    panic!("wrong remote");
                }
            }
        } else {
            panic!("Wrong ret");
        }
        Ok(())
    }
}
