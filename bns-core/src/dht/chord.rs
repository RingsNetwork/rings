/// implementation of CHORD DHT
/// ref: https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf
/// With high probability, the number of nodes that must be contacted to ﬁnd a successor in an N-node network is O(log N).
use crate::did::Did;
use anyhow::anyhow;
use anyhow::Result;
use num_bigint::BigUint;

#[derive(Clone, Debug, PartialEq)]
pub enum ChordAction {
    None,
    Some(Did),
    // Ask did__a to find did_b
    FindSuccessor((Did, Did)),
    // ask Did_a to notify(did_b)
    Notify((Did, Did)),
    FindSuccessorAndAddToFinger((u8, Did, Did)),
    CheckPredecessor(Did),
}

#[derive(Clone, Debug)]
pub struct Chord {
    // ﬁrst node on circle that succeeds (n + 2 ^(k-1) ) mod 2^m , 1 <= k<= m
    // for index start with 0, it should be (n+2^k) mod 2^m
    pub finger: Vec<Option<Did>>,
    // The next node on the identiﬁer circle; ﬁnger[1].node
    pub successor: Did,
    // The previous node on the identiﬁer circle
    pub predecessor: Option<Did>,
    pub id: Did,
    pub fix_finger_index: u8,
}

impl Chord {
    // create a new Chord ring.
    pub fn new(id: Did) -> Self {
        Self {
            successor: id,
            predecessor: None,
            // for Eth idess, it's 160
            finger: vec![None; 160],
            id,
            fix_finger_index: 0,
        }
    }

    // join a Chord ring containing node id .
    pub fn join(&mut self, id: Did) -> Result<ChordAction> {
        for k in (0u32..160u32).rev() {
            let n: BigUint = self.id.into();
            // (n + 2^k) % 2^m >= n
            // pos >= id
            let pos = (n + BigUint::from(2u16).pow(k)) % BigUint::from(2u16).pow(160);
            if pos <= id.into() {
                match self.finger[k as usize] {
                    Some(v) => {
                        if pos >= v.into() {
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

        Ok(ChordAction::FindSuccessor((self.successor, self.id)))
    }

    // called periodically. veriﬁes n’s immediate
    // successor, and tells the successor about n.
    pub fn stablilize(&mut self) -> ChordAction {
        // x = successor:predecessor;
        // if (x in (n, successor)) { successor = x; successor:notify(n); }
        if let Some(x) = self.predecessor {
            if x > self.id && x < self.successor {
                self.successor = x;
                return ChordAction::Notify((x, self.id));
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
                if id > pre && id < self.id {
                    self.predecessor = Some(id)
                }
            }
            None => self.predecessor = Some(id),
        }
    }

    // called periodically. refreshes ﬁnger table entries.
    // next stores the index of the next ﬁnger to ﬁx.
    pub fn fix_fingers(&mut self) -> Result<ChordAction> {
        // next = next + 1;
        //if (next > m) next = 1;
        // finger[next] = ﬁnd_successor(n + 2^(next-1) );
        // for index start with 0
        // finger[next] = ﬁnd_successor(n + 2^(next) );
        self.fix_finger_index += 1;
        if self.fix_finger_index >= 160 {
            self.fix_finger_index = 0;
        }
        let did: BigUint = (BigUint::from(self.id)
            + BigUint::from(2u16).pow(self.fix_finger_index.into()))
            % BigUint::from(2u16).pow(160);
        match self.find_successor(Did::try_from(did)?) {
            Ok(res) => match res {
                ChordAction::Some(v) => {
                    self.finger[self.fix_finger_index as usize] = Some(v);
                    Ok(ChordAction::None)
                }
                ChordAction::FindSuccessor((a, b)) => Ok(ChordAction::FindSuccessorAndAddToFinger(
                    (self.fix_finger_index, a, b),
                )),
                _ => {
                    log::error!("Invalid Chord Action");
                    Err(anyhow!("Invalid Chord Action"))
                }
            },
            Err(e) => Err(anyhow!(e)),
        }
    }

    // called periodically. checks whether predecessor has failed.
    pub fn check_predecessor(&self) -> ChordAction {
        match self.predecessor {
            Some(p) => ChordAction::CheckPredecessor(p),
            None => ChordAction::None,
        }
    }

    pub fn closest_precding_node(&self, id: Did) -> Result<Did> {
        for i in (1..160).rev() {
            if let Some(v) = self.finger[i] {
                if v > self.id && v < id {
                    // check a recorded did x in (self.id, target_id)
                    return Ok(v);
                }
            }
        }
        Err(anyhow!("cannot find cloest precding node"))
    }

    // Fig.5 n.find_successor(id)
    pub fn find_successor(&self, id: Did) -> Result<ChordAction> {
        // if (id \in (n; successor]); return successor
        if self.id < id && id <= self.successor {
            Ok(ChordAction::Some(id))
        } else {
            // n = closest preceding node(id);
            // return n.ﬁnd_successor(id);
            match self.closest_precding_node(id) {
                Ok(n) => Ok(ChordAction::FindSuccessor((n, id))),
                Err(e) => Err(anyhow!(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_chord_join() {
        let a = Did::from_str("0xaaE807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0xbb9999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xccffee254729296a45a3885639AC7E10F9d54979").unwrap();

        let mut node_a = Chord::new(a);
        let mut node_b = Chord::new(b);
        let mut node_c = Chord::new(c);

        assert_eq!(node_a.successor, a);
        assert_eq!(node_b.successor, b);
        assert_eq!(node_c.successor, c);

        assert!(a < b && b < c);
        node_a.join(b).unwrap();
        // if self.id < id && id <= self.successor
        assert_eq!(node_a.successor, b, "{:?}", node_a.finger);
        assert_eq!(node_a.find_successor(b).unwrap(), ChordAction::Some(b));
        assert_eq!(
            node_a.find_successor(c).unwrap(),
            ChordAction::FindSuccessor((b, c))
        );
        node_a.join(c).unwrap();
        assert_eq!(node_a.successor, b, "{:?}", node_a.finger);
        assert!(!node_a.finger.contains(&Some(a)));
        assert!(node_a.finger.contains(&Some(b)));
        assert!(node_a.finger.contains(&Some(c)), "{:?}", node_a.finger);

        node_b.join(c).unwrap();
        let _ = node_b.join(a);
        assert!(!node_b.finger.contains(&Some(a)));
        assert!(!node_b.finger.contains(&Some(b)));
        assert!(node_b.finger.contains(&Some(c)));

        // because a < b < c
        // c not in (a, successor)
        // go find cloest_precding_node which id in (a, c)
        assert_eq!(
            node_a.find_successor(c).unwrap(),
            ChordAction::FindSuccessor((b, c))
        );

        node_c.join(a).unwrap();
        node_c.join(b).unwrap();
        assert_eq!(node_c.successor, a, "{:?}", node_c.successor);
        assert!(node_c.finger.contains(&Some(a)), "{:?}", node_c.finger);
        assert!(!node_c.finger.contains(&Some(b)))
    }
}
