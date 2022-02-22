use num_bigint::BigUint;
use std::ops::Deref;
use web3::types::Address;

#[derive(Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
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
