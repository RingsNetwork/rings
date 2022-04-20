/// implementation of CHORD DHT
/// ref: https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf
/// With high probability, the number of nodes that must be contacted to find a successor in an N-node network is O(log N).
use super::peer::VirtualPeer;
use super::types::{Chord, ChordStablize, ChordStorage};
use crate::dht::Did;
use crate::err::{Error, Result};
use crate::storage::{MemStorage, Storage};
use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RemoteAction {
    // Ask did_a to find did_b
    FindSuccessor(Did),
    // Ask did_a to find virtual node did_b
    FindVNode(Did),
    // Ask did_a to find virtual peer for storage
    FindAndStore(VirtualPeer),
    // ask Did_a to notify(did_b)
    Notify(Did),
    SyncVNodeWithSuccessor(Vec<VirtualPeer>),
    FindSuccessorForFix(Did),
    CheckPredecessor,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PeerRingAction {
    None,
    SomeVNode(VirtualPeer),
    Some(Did),
    RemoteAction(Did, RemoteAction),
    MultiActions(Vec<PeerRingAction>),
}

#[derive(Clone, Debug)]
pub struct PeerRing {
    // first node on circle that succeeds (n + 2 ^(k-1) ) mod 2^m , 1 <= k<= m
    // for index start with 0, it should be (n+2^k) mod 2^m
    pub finger: Vec<Option<Did>>,
    // The next node on the identifier circle; finger[1].node
    pub successor: Did,
    // The previous node on the identifier circle
    pub predecessor: Option<Did>,
    pub id: Did,
    pub fix_finger_index: u8,
    pub storage: Arc<MemStorage<Did, VirtualPeer>>,
}

impl PeerRing {
    // create a new Chord ring.
    pub fn new(id: Did) -> Self {
        Self {
            successor: id,
            predecessor: None,
            // for Eth address, it's 160
            finger: vec![None; 160],
            id,
            fix_finger_index: 0,
            storage: Arc::new(MemStorage::<Did, VirtualPeer>::new()),
        }
    }

    pub fn new_with_storage(id: Did, storage: Arc<MemStorage<Did, VirtualPeer>>) -> Self {
        Self {
            successor: id,
            predecessor: None,
            // for Eth address, it's 160
            finger: vec![None; 160],
            storage: Arc::clone(&storage),
            id,
            fix_finger_index: 0,
        }
    }

    pub fn number_of_fingers(&self) -> usize {
        self.finger.iter().flatten().count() as usize
    }
}

impl Chord<PeerRingAction> for PeerRing {
    // join a PeerRing ring containing node id .
    fn join(&mut self, id: Did) -> PeerRingAction {
        if id == self.id {
            return PeerRingAction::None;
        }
        for k in 0u32..159u32 {
            // (n + 2^k) % 2^m >= n
            // pos >= id
            // from n to n + 2^160
            let pos = Did::from(BigUint::from(2u16).pow(k));

            // pos less than id or id is on another side of ring
            if pos <= id - self.id {
                match self.finger[k as usize] {
                    Some(v) => {
                        // for a existed value v
                        // if id is more close to self.id than v
                        if id - self.id < v - self.id {
                            //                        if id < v || id > -v {
                            self.finger[k as usize] = Some(id);
                            // if id is more close to successor
                        }
                    }
                    None => {
                        self.finger[k as usize] = Some(id);
                    }
                }
            }
        }
        if (id - self.id) < (id - self.successor) || self.id == self.successor {
            // 1) id should follows self.id
            // 2) #fff should follow #001 because id space is a Finate Ring
            // 3) #001 - #fff = #001 + -(#fff) = #001
            self.successor = id;
            // only triger if successor is updated
        }
        PeerRingAction::RemoteAction(self.successor, RemoteAction::FindSuccessor(self.id))
    }

    // Fig.5 n.find_successor(id)
    fn find_successor(&self, id: Did) -> Result<PeerRingAction> {
        // if (id \in (n; successor]); return successor
        // if ID = N63, Successor = N10
        // N9
        if id - self.id <= self.successor - self.id || self.id == self.successor {
            //if self.id < id && id <= self.successor {
            Ok(PeerRingAction::Some(id))
        } else {
            // n = closest preceding node(id);
            // return n.find_successor(id);
            match self.closest_preceding_node(id) {
                Ok(n) => Ok(PeerRingAction::RemoteAction(
                    n,
                    RemoteAction::FindSuccessor(id),
                )),
                Err(e) => Err(e),
            }
        }
    }
}

impl ChordStablize<PeerRingAction> for PeerRing {
    // called periodically. verifies n’s immediate
    // successor, and tells the successor about n.
    fn stablilize(&mut self) -> PeerRingAction {
        // x = successor:predecessor;
        // if (x in (n, successor)) { successor = x; successor:notify(n); }
        if let Some(x) = self.predecessor {
            if x - self.id < self.successor - self.id {
                self.successor = x;
                return PeerRingAction::RemoteAction(x, RemoteAction::Notify(self.id));
                // successor.notify(n)
            }
        }
        PeerRingAction::None
    }

    // n' thinks it might be our predecessor.
    fn notify(&mut self, id: Did) -> Option<Did> {
        // if (predecessor is nil or n' /in (predecessor; n)); predecessor = n';
        match self.predecessor {
            Some(pre) => {
                // if id <- [pre, self]
                if self.id - pre > self.id - id {
                    self.predecessor = Some(id);
                    Some(id)
                } else {
                    None
                }
            }
            None => {
                self.predecessor = Some(id);
                Some(id)
            }
        }
    }

    // called periodically. refreshes finger table entries.
    // next stores the index of the next finger to fix.
    fn fix_fingers(&mut self) -> Result<PeerRingAction> {
        // next = next + 1;
        //if (next > m) next = 1;
        // finger[next] = find_successor(n + 2^(next-1) );
        // for index start with 0
        // finger[next] = find_successor(n + 2^(next) );
        self.fix_finger_index += 1;
        if self.fix_finger_index >= 159 {
            self.fix_finger_index = 0;
        }
        let did: BigUint = (BigUint::from(self.id)
            + BigUint::from(2u16).pow(self.fix_finger_index.into()))
            % BigUint::from(2u16).pow(160);
        match self.find_successor(did.into()) {
            Ok(res) => match res {
                PeerRingAction::Some(v) => {
                    self.finger[self.fix_finger_index as usize] = Some(v);
                    Ok(PeerRingAction::None)
                }
                PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessor(b)) => Ok(
                    PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessorForFix(b)),
                ),
                _ => {
                    log::error!("Invalid PeerRing Action");
                    Err(Error::PeerRingInvalidAction)
                }
            },
            Err(e) => Err(Error::PeerRingFindSuccessor(e.to_string())),
        }
    }

    // called periodically. checks whether predecessor has failed.
    fn check_predecessor(&self) -> PeerRingAction {
        match self.predecessor {
            Some(p) => PeerRingAction::RemoteAction(p, RemoteAction::CheckPredecessor),
            None => PeerRingAction::None,
        }
    }

    /// Fig.5. n.cloest_preceding_node(id)
    /// for i = m downto1
    ///    if (finger[i] <- (n, id))
    ///        return finger[i]
    /// return n
    fn closest_preceding_node(&self, id: Did) -> Result<Did> {
        for i in (0..159).rev() {
            if let Some(v) = self.finger[i] {
                if v - self.id < v - id {
                    // check a recorded did x in (self.id, target_id)
                    return Ok(v);
                }
            }
        }
        Ok(self.id)
    }
}

impl ChordStorage<PeerRingAction> for PeerRing {
    fn lookup(&self, id: Did) -> Result<PeerRingAction> {
        match self.find_successor(id) {
            Ok(PeerRingAction::Some(id)) => match self.storage.get(&id) {
                Some(v) => Ok(PeerRingAction::SomeVNode(v)),
                None => Ok(PeerRingAction::None),
            },
            Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindSuccessor(id))) => {
                Ok(PeerRingAction::RemoteAction(n, RemoteAction::FindVNode(id)))
            }
            Ok(a) => Err(Error::PeerRingUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    fn store(&self, peer: VirtualPeer) -> Result<PeerRingAction> {
        let id = peer.did();
        match self.find_successor(id) {
            Ok(PeerRingAction::Some(id)) => match self.storage.get(&id) {
                Some(v) => {
                    let _ = self.storage.set(&id, VirtualPeer::concat(&v, &peer)?);
                    Ok(PeerRingAction::None)
                }
                None => {
                    let _ = self.storage.set(&id, peer);
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

    fn sync_with_successor(&self) -> Result<PeerRingAction> {
        let mut data = Vec::<VirtualPeer>::new();
        for k in self.storage.keys() {
            // k in (self, self.successor)
            // k is more close to self.successor
            if k - self.successor > k - self.id {
                if let Some(v) = self.storage.remove(&k) {
                    data.push(v.1);
                }
            }
        }
        Ok(PeerRingAction::RemoteAction(
            self.successor,
            RemoteAction::SyncVNodeWithSuccessor(data),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;
    use std::str::FromStr;

    #[test]
    fn test_chord_finger() {
        let a = Did::from_str("0x00E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0x119999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xccffee254729296a45a3885639AC7E10F9d54979").unwrap();
        let d = Did::from_str("0xffffee254729296a45a3885639AC7E10F9d54979").unwrap();

        assert!(a < b && b < c);
        // distence between (a, d) is less than (b, d)
        assert!((a - d) < (b - d));

        let mut node_a = PeerRing::new(a);
        assert_eq!(node_a.successor, a);

        // for increase seq join
        node_a.join(a);
        // Node A wont add self to finder
        assert_eq!(node_a.finger, [None; 160]);
        node_a.join(b);
        // b is very far away from a
        // a.finger should store did as range
        // [(a, a+2), (a+2, a+4), (a+4, a+8), ..., (a+2^159, a + 2^160)]
        // b is in range(a+2^156, a+2^157)
        assert!(BigUint::from(b) > BigUint::from(2u16).pow(156));
        assert!(BigUint::from(b) < BigUint::from(2u16).pow(157));
        // Node A's finter should be [None, .., B]
        assert!(node_a.finger.contains(&Some(b)));
        assert!(node_a.finger.contains(&None));

        // Node A starts to query node b for it's successor
        assert_eq!(
            node_a.join(b),
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(a))
        );
        assert_eq!(node_a.successor, b);
        // Node A keep querying node b for it's successor
        assert_eq!(
            node_a.join(c),
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(a))
        );
        // Node A's finter should be [None, ..B, C]
        assert!(node_a.finger.contains(&Some(c)), "{:?}", node_a.finger);
        // c is in range(a+2^159, a+2^160)
        assert!(BigUint::from(c) > BigUint::from(2u16).pow(159));
        assert!(BigUint::from(c) < BigUint::from(2u16).pow(160));

        assert_eq!(node_a.finger[158], Some(c));
        assert_eq!(node_a.finger[155], Some(b));
        assert_eq!(node_a.finger[156], Some(b));

        assert_eq!(node_a.successor, b);
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
        let mut node_d = PeerRing::new(d);
        assert_eq!(
            node_d.join(c),
            PeerRingAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_d.join(b),
            PeerRingAction::RemoteAction(b, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_d.join(a),
            PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
        );

        // for over half ring join
        let mut node_d = PeerRing::new(d);
        assert_eq!(node_d.successor, d);
        assert_eq!(
            node_d.join(a),
            PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
        );
        // for a ring a, a is over 2^152 far away from d
        assert!(d + Did::from(BigUint::from(2u16).pow(152)) > a);
        assert!(d + Did::from(BigUint::from(2u16).pow(151)) < a);
        assert!(node_d.finger.contains(&Some(a)));
        assert_eq!(node_d.finger[151], Some(a));
        assert_eq!(node_d.finger[152], None);
        assert_eq!(node_d.finger[0], Some(a));
        // when b insearted a is still more close to d
        assert_eq!(
            node_d.join(b),
            PeerRingAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
        );
        assert!(d + Did::from(BigUint::from(2u16).pow(159)) > b);
        assert_eq!(node_d.successor, a);
    }

    #[test]
    fn test_two_node_finger() {
        let mut key1 = SecretKey::random();
        let mut key2 = SecretKey::random();
        if key1.address() > key2.address() {
            (key1, key2) = (key2, key1)
        }
        let did1: Did = key1.address().into();
        let did2: Did = key2.address().into();
        let mut node1 = PeerRing::new(did1);
        let mut node2 = PeerRing::new(did2);

        node1.join(did2);
        node2.join(did1);
        assert_eq!(node1.successor, did2);
        assert_eq!(node2.successor, did1);

        assert!(
            node1.finger.contains(&Some(did2)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
        assert!(
            node2.finger.contains(&Some(did1)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
    }

    #[test]
    fn test_two_node_finger_failed_case() {
        let did1 = Did::from_str("0x051cf4f8d020cb910474bef3e17f153fface2b5f").unwrap();
        let did2 = Did::from_str("0x54baa7dc9e28f41da5d71af8fa6f2a302be1c1bf").unwrap();
        let max = Did::from(BigUint::from(2u16).pow(160) - 1u16);
        let zero = Did::from(BigUint::from(2u16).pow(160));

        let mut node1 = PeerRing::new(did1);
        let mut node2 = PeerRing::new(did2);

        node1.join(did2);
        node2.join(did1);
        assert_eq!(node1.successor, did2);
        assert_eq!(node2.successor, did1);
        let pos_159 = did2 + Did::from(BigUint::from(2u16).pow(159));
        assert!(pos_159 > did2);
        assert!(pos_159 < max, "{:?};{:?}", pos_159, max);
        let pos_160 = did2 + zero;
        assert_eq!(pos_160, did2);
        assert!(pos_160 > did1);

        assert!(
            node1.finger.contains(&Some(did2)),
            "did1:{:?}; did2:{:?}",
            did1,
            did2
        );
        assert!(
            node2.finger.contains(&Some(did1)),
            "did2:{:?} dont contains did1:{:?}",
            did2,
            did1
        );
    }
}
