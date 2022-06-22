use std::fmt::Write;

use serde::Deserialize;
use serde::Serialize;

use crate::ecc::signers;
use crate::ecc::PublicKey;
use crate::err::Error;
use crate::err::Result;
use crate::session::Session;
use crate::session::Signer;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct MessageVerification {
    pub session: Session,
    pub ttl_ms: usize,
    pub ts_ms: u128,
    pub sig: Vec<u8>,
}

impl MessageVerification {
    pub fn verify<T>(&self, data: &T) -> bool
    where T: Serialize {
        if !self.session.verify() {
            return false;
        }

        if let (Ok(addr), Ok(msg)) = (self.session.address(), self.msg(data)) {
            match self.session.auth.signer {
                Signer::DEFAULT => signers::default::verify(&msg, &addr, &self.sig),
                Signer::EIP712 => signers::eip712::verify(&msg, &addr, &self.sig),
            }
        } else {
            false
        }
    }

    pub fn session_pubkey<T>(&self, data: &T) -> Result<PublicKey>
    where T: Serialize {
        let msg = self.msg(data)?;
        match self.session.auth.signer {
            Signer::DEFAULT => signers::default::recover(&msg, &self.sig),
            Signer::EIP712 => signers::eip712::recover(&msg, &self.sig),
        }
    }

    pub fn pack_msg<T>(data: &T, ts_ms: u128, ttl_ms: usize) -> Result<String>
    where T: Serialize {
        let mut msg = serde_json::to_string(data).map_err(|_| Error::SerializeToString)?;
        write!(msg, "\n{}\n{}", ts_ms, ttl_ms).map_err(|_| Error::SerializeToString)?;
        Ok(msg)
    }

    fn msg<T>(&self, data: &T) -> Result<String>
    where T: Serialize {
        Self::pack_msg(data, self.ts_ms, self.ttl_ms)
    }
}
