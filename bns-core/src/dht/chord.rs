use super::peer::VirtualPeer;
/// implementation of CHORD DHT
/// ref: https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf
/// With high probability, the number of nodes that must be contacted to find a successor in an N-node network is O(log N).
use crate::dht::Did;
use crate::err::{Error, Result};
use crate::storage::{MemStorage, Storage};
use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

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
    NotifyWithVNode(Did, Vec<VirtualPeer>),
    FindSuccessorForFix(Did),
    CheckPredecessor,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChordAction {
    None,
    SomeVNode(VirtualPeer),
    Some(Did),
    RemoteAction(Did, RemoteAction),
}

#[derive(Clone, Debug)]
pub struct Chord {
    // first node on circle that succeeds (n + 2 ^(k-1) ) mod 2^m , 1 <= k<= m
    // for index start with 0, it should be (n+2^k) mod 2^m
    pub finger: Vec<Option<Did>>,
    // The next node on the identifier circle; finger[1].node
    pub successor: Did,
    // The previous node on the identifier circle
    pub predecessor: Option<Did>,
    pub id: Did,
    pub fix_finger_index: u8,
    pub storage: MemStorage<Did, VirtualPeer>,
}

impl Chord {
    // create a new Chord ring.
    pub fn new(id: Did) -> Self {
        Self {
            successor: id,
            predecessor: None,
            // for Eth address, it's 160
            finger: vec![None; 160],
            storage: MemStorage::<Did, VirtualPeer>::new(),
            id,
            fix_finger_index: 0,
        }
    }

    // join a Chord ring containing node id .
    pub fn join(&mut self, id: Did) -> ChordAction {
        if id == self.id {
            // TODO: Do we allow multiple chord instances of the same node id?
            return ChordAction::None;
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
        }
        ChordAction::RemoteAction(self.successor, RemoteAction::FindSuccessor(self.id))
    }

    // called periodically. verifies n’s immediate
    // successor, and tells the successor about n.
    pub fn stablilize(&mut self) -> ChordAction {
        // x = successor:predecessor;
        // if (x in (n, successor)) { successor = x; successor:notify(n); }
        if let Some(x) = self.predecessor {
            if x - self.id < self.successor - self.id {
                self.successor = x;
                return ChordAction::RemoteAction(x, RemoteAction::Notify(self.id));
                // successor.notify(n)
            }
        }
        ChordAction::None
    }

    // n' thinks it might be our predecessor.
    pub fn notify(&mut self, id: Did) {
        // if (predecessor is nil or n' /in (predecessor; n)); predecessor = n';
        match self.predecessor {
            Some(pre) => {
                // if id <- [pre, self]
                if self.id - pre > self.id - id {
                    self.predecessor = Some(id)
                }
            }
            None => self.predecessor = Some(id),
        }
    }

    // called periodically. refreshes finger table entries.
    // next stores the index of the next finger to fix.
    pub fn fix_fingers(&mut self) -> Result<ChordAction> {
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
                ChordAction::Some(v) => {
                    self.finger[self.fix_finger_index as usize] = Some(v);
                    Ok(ChordAction::None)
                }
                ChordAction::RemoteAction(a, RemoteAction::FindSuccessor(b)) => Ok(
                    ChordAction::RemoteAction(a, RemoteAction::FindSuccessorForFix(b)),
                ),
                _ => {
                    log::error!("Invalid Chord Action");
                    Err(Error::ChordInvalidAction)
                }
            },
            Err(e) => Err(Error::ChordFindSuccessor(e.to_string())),
        }
    }

    // called periodically. checks whether predecessor has failed.
    pub fn check_predecessor(&self) -> ChordAction {
        match self.predecessor {
            Some(p) => ChordAction::RemoteAction(p, RemoteAction::CheckPredecessor),
            None => ChordAction::None,
        }
    }

    /// Fig.5. n.cloest_preceding_node(id)
    /// for i = m downto1
    ///    if (finger[i] <- (n, id))
    ///        return finger[i]
    /// return n
    pub fn closest_preceding_node(&self, id: Did) -> Result<Did> {
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

    // Fig.5 n.find_successor(id)
    pub fn find_successor(&self, id: Did) -> Result<ChordAction> {
        // if (id \in (n; successor]); return successor
        // if ID = N63, Successor = N10
        // N9
//        if id - self.id <= self.successor - self.id {
        if self.id < id && id <= self.successor {
            Ok(ChordAction::Some(id))
        } else {
            // n = closest preceding node(id);
            // return n.find_successor(id);
            match self.closest_preceding_node(id) {
                Ok(n) => Ok(ChordAction::RemoteAction(
                    n,
                    RemoteAction::FindSuccessor(id),
                )),
                Err(e) => Err(e),
            }
        }
    }
}

impl Chord {
    pub fn lookup(&self, id: Did) -> Result<ChordAction> {
        match self.find_successor(id) {
            Ok(ChordAction::Some(id)) => match self.storage.get(&id) {
                Some(v) => Ok(ChordAction::SomeVNode(v)),
                None => Ok(ChordAction::None),
            },
            Ok(ChordAction::RemoteAction(n, RemoteAction::FindSuccessor(id))) => {
                Ok(ChordAction::RemoteAction(n, RemoteAction::FindVNode(id)))
            }
            Ok(a) => Err(Error::ChordUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    pub fn store(&self, peer: VirtualPeer) -> Result<ChordAction> {
        let id = peer.did();
        match self.find_successor(id) {
            Ok(ChordAction::Some(id)) => match self.storage.set(&id, peer) {
                Some(v) => Ok(ChordAction::SomeVNode(v)),
                None => Ok(ChordAction::None),
            },
            Ok(ChordAction::RemoteAction(n, RemoteAction::FindSuccessor(_))) => Ok(
                ChordAction::RemoteAction(n, RemoteAction::FindAndStore(peer)),
            ),
            Ok(a) => Err(Error::ChordUnexpectedAction(a)),
            Err(e) => Err(e),
        }
    }

    pub fn stablilize_with_vnode(&mut self) -> Result<ChordAction> {
        match self.stablilize() {
            ChordAction::RemoteAction(x, RemoteAction::Notify(id)) => {
                Ok(ChordAction::RemoteAction(
                    x,
                    RemoteAction::NotifyWithVNode(id, self.storage.values()),
                ))
            }
            ChordAction::None => Ok(ChordAction::None),
            x => Err(Error::ChordUnexpectedAction(x)),
        }
    }

    pub fn notify_with_vnode(&mut self, id: Did, vnodes: Vec<VirtualPeer>) {
        self.notify(id);
        for v in vnodes {
            self.storage.set(&v.did(), v);
        }
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

        let mut node_a = Chord::new(a);
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
            ChordAction::RemoteAction(b, RemoteAction::FindSuccessor(a))
        );
        assert_eq!(node_a.successor, b);
        // Node A keep querying node b for it's successor
        assert_eq!(
            node_a.join(c),
            ChordAction::RemoteAction(b, RemoteAction::FindSuccessor(a))
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
            ChordAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_a.find_successor(c).unwrap(),
            ChordAction::RemoteAction(b, RemoteAction::FindSuccessor(c))
        );

        // for decrease seq join
        let mut node_d = Chord::new(d);
        assert_eq!(
            node_d.join(c),
            ChordAction::RemoteAction(c, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_d.join(b),
            ChordAction::RemoteAction(b, RemoteAction::FindSuccessor(d))
        );
        assert_eq!(
            node_d.join(a),
            ChordAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
        );

        // for over half ring join
        let mut node_d = Chord::new(d);
        assert_eq!(node_d.successor, d);
        assert_eq!(
            node_d.join(a),
            ChordAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
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
            ChordAction::RemoteAction(a, RemoteAction::FindSuccessor(d))
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
        let mut node1 = Chord::new(did1);
        let mut node2 = Chord::new(did2);

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

        let mut node1 = Chord::new(did1);
        let mut node2 = Chord::new(did2);

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
