use std::str::FromStr;
use std::sync::Arc;

use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::RpcMeta;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::dht::Stabilization;
use crate::prelude::rings_core::dht::TStabilize;
use crate::prelude::rings_core::ecc::PublicKey;
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::session::AuthorizedInfo;
use crate::prelude::rings_core::session::SessionManager;
use crate::prelude::rings_core::session::Signer;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::rings_core::swarm::SwarmBuilder;
use crate::prelude::rings_core::types::message::MessageListener;
use crate::prelude::web3::contract::tokens::Tokenizable;
use crate::processor::Processor;

/// AddressType enum contains `DEFAULT` and `ED25519`.
pub enum AddressType {
    DEFAULT,
    ED25519,
}

impl ToString for AddressType {
    fn to_string(&self) -> String {
        match self {
            Self::DEFAULT => "default".to_owned(),
            Self::ED25519 => "ED25519".to_owned(),
        }
    }
}

/// A UnsignedInfo use for wasm.
#[derive(Clone)]
pub struct UnsignedInfo {
    /// Did identify
    key_addr: Did,
    /// auth information
    auth: AuthorizedInfo,
    /// random secrekey generate by service
    random_key: SecretKey,
}

impl UnsignedInfo {
    /// Create a new `UnsignedInfo` instance with SignerMode::EIP191
    pub fn new(key_addr: String) -> Result<Self> {
        Self::new_with_signer(key_addr, Signer::EIP191)
    }

    /// Create a new `UnsignedInfo` instance
    ///   * key_addr: wallet address
    ///   * signer: `SignerMode`
    pub fn new_with_signer(key_addr: String, signer: Signer) -> Result<Self> {
        let key_addr = Did::from_str(key_addr.as_str()).map_err(|_| Error::InvalidDid)?;
        let (auth, random_key) = SessionManager::gen_unsign_info(key_addr, None, Some(signer));

        Ok(UnsignedInfo {
            auth,
            random_key,
            key_addr,
        })
    }

    /// Create a new `UnsignedInfo` instance
    ///   * pubkey: solana wallet pubkey
    pub fn new_with_address(address: String, addr_type: AddressType) -> Result<Self> {
        let (key_addr, auth, random_key) = match addr_type {
            AddressType::DEFAULT => {
                let key_addr = Did::from_str(address.as_str()).map_err(|_| Error::InvalidDid)?;
                let (auth, random_key) =
                    SessionManager::gen_unsign_info(key_addr, None, Some(Signer::EIP191));
                (key_addr, auth, random_key)
            }
            AddressType::ED25519 => {
                let pubkey =
                    PublicKey::try_from_b58t(&address).map_err(|_| Error::InvalidAddress)?;
                let (auth, random_key) =
                    SessionManager::gen_unsign_info_with_ed25519_pubkey(None, pubkey)
                        .map_err(|_| Error::InvalidAddress)?;
                (pubkey.address().into(), auth, random_key)
            }
        };
        Ok(UnsignedInfo {
            auth,
            random_key,
            key_addr,
        })
    }

    pub fn auth(&self) -> Result<String> {
        let s = self.auth.to_string().map_err(|_| Error::InvalidAuthData)?;
        Ok(s)
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct Client {
    processor: Arc<Processor>,
    rpc_meta: RpcMeta,
}

impl Client {
    /// Creat a new client instance.
    pub async fn new_client(
        unsigned_info: &UnsignedInfo,
        signed_data: &[u8],
        stuns: String,
    ) -> Result<Self> {
        Self::new_client_with_storage(unsigned_info, signed_data, stuns, "rings-node".to_owned())
            .await
    }

    pub fn address(&self) -> Result<String> {
        Ok(self.processor.did().into_token().to_string())
    }

    pub async fn new_client_with_storage(
        unsigned_info: &UnsignedInfo,
        signed_data: &[u8],
        stuns: String,
        storage_name: String,
    ) -> Result<Self> {
        let unsigned_info = unsigned_info.clone();
        let signed_data = signed_data.to_vec();
        let storage = PersistenceStorage::new_with_cap_and_name(50000, storage_name.as_str())
            .await
            .map_err(Error::Storage)?;

        let random_key = unsigned_info.random_key;
        let session_manager = SessionManager::new(&signed_data, &unsigned_info.auth, &random_key);

        let swarm = Arc::new(
            SwarmBuilder::new(&stuns, storage)
                .session_manager(unsigned_info.key_addr, session_manager)
                .build()
                .map_err(Error::Swarm)?,
        );

        let stabilization = Arc::new(Stabilization::new(swarm.clone(), 20));
        let processor = Arc::new(Processor::from((swarm, stabilization)));
        let rpc_meta = (processor.clone(), false).into();
        Ok(Client {
            processor,
            rpc_meta,
        })
    }

    /// start background listener without custom callback
    pub async fn start(&self) -> Result<()> {
        let p = self.processor.clone();

        let h = Arc::new(p.swarm.create_message_handler(None, None));
        let s = Arc::clone(&p.stabilization);
        futures::join!(
            async {
                h.listen().await;
            },
            async {
                s.wait().await;
            }
        );
        Ok(())
    }
}
