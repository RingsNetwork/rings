use num_bigint::BigUint;
use std::ops::Deref;
use std::str::FromStr;
use web3::types::Address;

#[derive(Copy, Clone, Eq, Ord, PartialEq, PartialOrd, Debug)]
pub struct Did(Address);

impl Deref for Did {
    type Target = Address;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Did> for BigUint {
    fn from(did: Did) -> BigUint {
        BigUint::from_bytes_be(did.as_bytes())
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
        Ok(Self(Address::from_str(&s)?.into()))
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
}
