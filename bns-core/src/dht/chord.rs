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
    pub finger: Vec<Did>,
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
            finger: vec![id; 160],
            id,
            fix_finger_index: 0,
        }
    }

    // join a Chord ring containing node id .
    pub fn join(&mut self, id: Did) -> ChordAction {
        for k in 0u32..160u32 {
            let n: BigUint = self.id.into();
            // (n + 2^k) % 2^m >= n
            let delta = (n + BigUint::from(2u16).pow(k)) % BigUint::from(2u16).pow(160);
            if delta <= id.into() && delta > self.finger[k as usize].into() {
                self.finger[k as usize] = id;
            }
        }
        self.successor = self.finger[0];
        ChordAction::FindSuccessor((self.successor, id))
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
                    self.finger[self.fix_finger_index as usize] = v;
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

    pub fn cloest_precding_node(&self, id: Did) -> Result<Did> {
        for i in (1..160).rev() {
            if self.finger[i] > self.id && self.finger[i] < id {
                // check a recorded did x in (self.id, target_id)
                return Ok(self.finger[i]);
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
            match self.cloest_precding_node(id) {
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
    fn test_chord() {
        let a = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0x999999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xc0ffee254729296a45a3885639AC7E10F9d54979").unwrap();

        let mut node_a = Chord::new(a);
        let mut node_b = Chord::new(b);
        let _node_c = Chord::new(c);

        assert!(a < b && b < c);
        node_a.join(b);
        assert_eq!(node_a.successor, b);
        assert_eq!(node_a.find_successor(b).unwrap(), ChordAction::Some(b));
        assert_eq!(
            node_a.find_successor(c).unwrap(),
            ChordAction::FindSuccessor((b, c))
        );
        node_a.join(c);
        assert_eq!(node_a.successor, b, "{:?}", node_a.finger);
        node_b.join(c);
        // because a < b < c
        // c not in (a, successor)
        // go find cloest_precding_node which id in (a, c)
        assert_eq!(
            node_a.find_successor(c).unwrap(),
            ChordAction::FindSuccessor((b, c))
        );
    }
}
