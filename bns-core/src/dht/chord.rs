use crate::did::Did;
use anyhow::anyhow;
use anyhow::Result;
use num_bigint::BigUint;

#[derive(Clone, Debug, PartialEq)]
pub enum FoundRes {
    Found(Did),
    ToFind(Did),
}

#[derive(Clone, Debug)]
pub struct Chord {
    // ﬁrst node on circle that succeeds (n + 2 ^(k-1) ) mod 2^m , 1 <= k<= m
    // for index start with 0, it should be (n+2^k) mod 2^m
    pub finger: Vec<Option<Did>>,
    // The next node on the identiﬁer circle; ﬁnger[1].node
    pub successor: Option<Did>,
    // The previous node on the identiﬁer circle
    pub predecessor: Option<Did>,
    pub id: Did,
}

impl Chord {
    pub fn new(id: Did) -> Self {
        Self {
            successor: None,
            predecessor: None,
            // for Eth idess, it's 160
            finger: vec![None; 160],
            id,
        }
    }

    pub fn join(&mut self, id: Did) {
        for k in 0u32..160u32 {
            let n: BigUint = self.id.into();
            // (n + 2^k) % 2^m >= n
            let delta = (n + BigUint::from(2u16).pow(k)) % BigUint::from(2u16).pow(160);
            if delta <= id.into() {
                match self.finger[k as usize] {
                    Some(v) => {
                        if delta > v.into() {
                            self.finger[k as usize] = Some(id);
                        }
                    }
                    None => {
                        self.finger[k as usize] = Some(id);
                    }
                }
            }
        }
        self.successor = self.finger[0];
    }

    pub fn cloest_precding_node(&self, id: Did) -> Result<Did> {
        for i in (1..160).rev() {
            if let Some(t) = self.finger[i] {
                if t > self.id && t < id {
                    return Ok(t);
                }
            }
        }
        Err(anyhow!("cannot find cloest precding node"))
    }

    // Fig.5 n.find_successor(id)
    pub fn find_successor(&self, id: Did) -> Result<FoundRes> {
        match self.successor {
            Some(successor) => {
                // if (id \in (n; successor]); return successor
                if self.id < id && id <= successor {
                    Ok(FoundRes::Found(id))
                } else {
                    // n = closest preceding node(id);
                    // return n.ﬁnd_successor(id);
                    match self.cloest_precding_node(id) {
                        Ok(n) => Ok(FoundRes::ToFind(n)),
                        Err(e) => Err(anyhow!(e)),
                    }
                }
            }
            None => Err(anyhow!("successor not found")),
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
        let mut node_c = Chord::new(c);

        assert!(a < b && b < c);
        node_a.join(b);
        assert_eq!(node_a.successor.unwrap(), b);
        assert_eq!(node_a.find_successor(b).unwrap(), FoundRes::Found(b));
        assert_eq!(node_a.find_successor(c).unwrap(), FoundRes::ToFind(b));
        node_a.join(c);
        assert_eq!(node_a.successor.unwrap(), b, "{:?}", node_a.finger);
        node_b.join(c);
        // because a < b < c
        // c not in (a, successor)
        // go find cloest_precding_node which id in (a, c)
        assert_eq!(node_a.find_successor(c).unwrap(), FoundRes::ToFind(b));
    }
}
