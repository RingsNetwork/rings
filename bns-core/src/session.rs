use crate::ecc::verify;
use crate::ecc::SecretKey;
use crate::utils;
use crate::err::{Result, Error};
use std::sync::Arc;
use std::sync::RwLock;
use web3::types::Address;
use serde::Deserialize;
use serde::Serialize;

const DEFAULT_TTL_MS: usize = 24 * 3600 * 1000;

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
enum Ttl {
    Some(usize),
    Never
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct AuthorizedInfo {
    pub authorizer: Address,
    pub addr: Address,
    pub ttl_ms: Ttl,
    pub ts_ms: u128,
}


#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct Session {
    pub sig: Vec<u8>,
    pub auth: AuthorizedInfo
}

pub struct SessionInfo {
    pub session: Session,
    pub session_key: SecretKey
}

pub struct SessionManager {
    pub session_info: Arc<RwLock<SessionInfo>>,
}


impl Session {
    pub fn gen_unsign_info(authorizer: Address, ttl: Option<Ttl>) -> Result<(AuthorizedInfo, SecretKey)> {
        let key = SecretKey::random();
        let info = AuthorizedInfo {
            authorizer,
            addr: key.address(),
            ttl_ms: ttl.unwrap_or(Ttl::Some(DEFAULT_TTL_MS)),
            ts_ms: utils::get_epoch_ms()
        };
        Ok((info, key))
    }

    pub fn new(sig: &Vec<u8>, auth_info: &AuthorizedInfo) -> Self {
        Self {
            sig: sig.clone(),
            auth: auth_info.clone()
        }
    }

    pub fn is_expired(&self) -> bool {
        if let Ttl::Some(ttl_ms) = self.auth.ttl_ms {
            let now = utils::get_epoch_ms();
            now > self.auth.ts_ms + ttl_ms as u128
        } else {
            false
        }
    }

    pub fn verify(&self) -> bool {
        if self.is_expired() {
            return false;
        }
        if let Ok(auth_str) = serde_json::to_string(&self.auth).map_err(|_| Error::SerializeToString) {
            verify(&auth_str, &self.auth.authorizer, self.sig.clone())
        } else {
            false
        }
    }

    pub fn address(&self) -> Result<Address> {
        if !self.verify() {
            Err(Error::VerifySignatureFailed)
        } else {
            Ok(self.auth.addr)
        }
    }
}


impl SessionInfo {
    pub fn sign(&self, msg: &str) -> Vec<u8> {
        self.session_key.sign(&msg).to_vec()
    }

    pub fn authorizer(&self) -> Address {
        self.session.auth.authorizer
    }
}
