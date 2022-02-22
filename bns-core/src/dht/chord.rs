use crate::did::Did;
use num_bigint::BigUint;
use anyhow::Result;
use anyhow::anyhow;

pub enum FoundRes {
    Found(Did),
    ToFind(Did)
}

#[derive(Clone)]
pub struct Chord {
    // ﬁrst node on circle that succeeds (n + 2 ^(k-1) ) mod 2^m , 1 <= k<= m
    pub finger: Vec<Option<Did>>,
    // The next node on the identiﬁer circle; ﬁnger[1].node
    pub successor: Option<Did>,
    // The previous node on the identiﬁer circle
    pub predecessor: Option<Did>,
    pub id: Did
}

impl Chord {
    pub fn new(id: Did) -> Self {
        Self {
            successor: None,
            predecessor: None,
            // for Eth idess, it's 160
            finger: vec![None; 160],
            id
        }
    }

    pub fn join(&mut self, id: Did) {
        for k in 0u32..160u32 {
            let n: BigUint = self.id.into();
            if (n + BigUint::from(2u16).pow(k - 1)) % BigUint::from(2u16).pow(160) <= id.into() {
                self.finger[k as usize] = Some(id)
            }
        }
        self.successor = self.finger[0];
    }

    pub fn cloest_precding_node(&self, id: Did) -> Result<Did> {
        for i in (1 .. 160).rev() {
            if let Some(t) = self.finger[i] {
                if t > self.id && t < id {
                    return Ok(t);
                }
            }
        }
        Err(anyhow!("cannot find cloest precding node"))
    }

    pub fn find_successor(&self, id: Did) -> Result<FoundRes> {
        match self.successor {
            Some(successor) => {
                if self.id < id && id <= successor {
                    Ok(FoundRes::Found(id))
                } else {
                    match self.cloest_precding_node(id) {
                        Ok(n) => Ok(FoundRes::ToFind(n)),
                        Err(e) => Err(anyhow!(e))
                    }

                }
            },
            None => Err(anyhow!("successor not found"))
        }
    }
}
