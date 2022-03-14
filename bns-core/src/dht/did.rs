/// Did is a finate Ring R(P) where P = 2^160
use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::{Eq, PartialEq};
use std::ops::Deref;
use std::ops::Neg;
use std::ops::{Add, Sub};
use std::str::FromStr;
use web3::types::Address;
use web3::types::H160;

#[derive(Copy, Clone, Eq, Ord, PartialEq, PartialOrd, Debug, Serialize, Deserialize)]
pub struct Did(Address);

impl Deref for Did {
    type Target = Address;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Did> for Address {
    fn from(a: Did) -> Self {
        a.0.to_owned()
    }
}

impl From<Did> for BigUint {
    fn from(did: Did) -> BigUint {
        BigUint::from_bytes_be(did.as_bytes())
    }
}

impl From<BigUint> for Did {
    fn from(a: BigUint) -> Self {
        let ff = a % (BigUint::from(2u16).pow(160));
        let mut va: Vec<u8> = ff.to_bytes_be();
        let mut res = vec![0u8; 20 - va.len()];
        res.append(&mut va);
        assert_eq!(res.len(), 20, "{:?}", res);
        Self(H160::from_slice(&res))
    }
}

impl From<Address> for Did {
    fn from(addr: Address) -> Self {
        Self(addr)
    }
}

impl FromStr for Did {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(Self(Address::from_str(s)?))
    }
}

// impl Finate Ring For Did

impl Neg for Did {
    type Output = Self;
    fn neg(self) -> Self {
        let ret = BigUint::from(2u16).pow(160) - BigUint::from(self);
        ret.into()
    }
}

impl Add for Did {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        ((BigUint::from(self) + BigUint::from(rhs)) % (BigUint::from(2u16).pow(160))).into()
    }
}

impl Sub for Did {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        self + (-rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_did() {
        let a = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        let b = Did::from_str("0x999999cf1046e68e36E1aA2E0E07105eDDD1f08E").unwrap();
        let c = Did::from_str("0xc0ffee254729296a45a3885639AC7E10F9d54979").unwrap();
        assert!(c > b && b > a);
    }

    #[test]
    fn test_finate_ring_neg() {
        let zero = Did::from_str("0x0000000000000000000000000000000000000000").unwrap();
        let a = Did::from_str("0x11E807fcc88dD319270493fB2e822e388Fe36ab0").unwrap();
        assert_eq!(-a + a, zero);
        assert_eq!(-(-a), a);
    }
}
