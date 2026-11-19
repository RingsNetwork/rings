use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use rings_core::delegation::DelegationDigest;
use rings_core::dht::Did;
use rings_core::ecc::keccak256;
use rings_core::ecc::PublicKey;
use rings_core::ecc::SecretKey;
use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;
use turn::auth::generate_auth_key;
use turn::auth::AuthHandler;

use crate::credentials::TurnAuthIdentity;
use crate::credentials::TurnCredentials;
use crate::credentials::VerifiedTurnAuth;
use crate::error::Result;
use crate::error::TurnError;

/// Default maximum number of verified TURN authentication entries retained in memory so
/// subsequent TURN requests (`Refresh`, `CreatePermission`, `ChannelBind`) skip redundant
/// public-key recovery while still enforcing delegation expiry and delegatee signatures.
pub const DEFAULT_VERIFIED_DELEGATION_CACHE_CAPACITY: usize = 4096;

/// Admission policy controlling which verified Rings [`Did`] identities (either via a
/// [`Delegation`](rings_core::delegation::Delegation) or a direct [`Account`](rings_core::delegation::Account) / [`Did`] signature)
/// may allocate TURN relays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TurnAdmissionPolicy {
    /// Admit any cryptographically valid, currently live [`Delegation`](rings_core::delegation::Delegation)
    /// or direct [`Account`](rings_core::delegation::Account) / [`Did`] signature.
    #[default]
    AllowAnyDelegation,
    /// Admit only requests signed or authorized by a [`Did`] in the allowlist.
    Allowlist(BTreeSet<Did>),
}

impl TurnAdmissionPolicy {
    /// Construct an allowlist policy from an iterator of allowed [`Did`]s.
    pub fn allowlist(dids: impl IntoIterator<Item = Did>) -> Self {
        Self::Allowlist(dids.into_iter().collect())
    }

    /// Return whether `did` is permitted by this policy.
    pub fn is_admitted(&self, did: Did) -> bool {
        match self {
            Self::AllowAnyDelegation => true,
            Self::Allowlist(allowed) => allowed.contains(&did),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CacheEntry {
    Delegation(DelegationDigest),
    DirectAccount([u8; 32]),
}

#[derive(Debug)]
struct VerifiedAuthCache {
    capacity: usize,
    order: VecDeque<CacheEntry>,
    set: BTreeSet<CacheEntry>,
}

impl VerifiedAuthCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            set: BTreeSet::new(),
        }
    }

    fn contains(&self, entry: &CacheEntry) -> bool {
        self.set.contains(entry)
    }

    fn insert(&mut self, entry: CacheEntry) {
        if self.capacity == 0 || self.set.contains(&entry) {
            return;
        }
        if self.set.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        self.set.insert(entry);
        self.order.push_back(entry);
    }
}

/// Stateless signature- and ECDH-verifying [`AuthHandler`] backed by Rings
/// [`DelegateeKey`](rings_core::delegation::DelegateeKey) and direct [`Did`] /
/// [`Account`](rings_core::delegation::Account) credentials.
///
/// For every incoming TURN request (`Allocate`, `Refresh`, `CreatePermission`, `ChannelBind`),
/// [`DelegationAuthHandler`] extracts the [`TurnAuthIdentity`] and signature from `username`,
/// verifies signatures against `server_key.pubkey()`, computes the Diffie-Hellman shared secret
/// using `server_key` to derive `credential`, checks [`TurnAdmissionPolicy`], and derives the
/// RFC 5389 long-term key `MD5(username : realm : credential)`.
pub struct DelegationAuthHandler {
    policy: TurnAdmissionPolicy,
    server_key: SecretKey,
    clock: SharedClock,
    cache: Mutex<VerifiedAuthCache>,
    expected_allocation_owners: Mutex<HashMap<SocketAddr, String>>,
}

impl Default for DelegationAuthHandler {
    fn default() -> Self {
        Self::new(TurnAdmissionPolicy::default())
    }
}

impl DelegationAuthHandler {
    /// Create a new [`DelegationAuthHandler`] with `policy`, a random server [`SecretKey`], and
    /// the system clock.
    pub fn new(policy: TurnAdmissionPolicy) -> Self {
        Self::with_clock(policy, Arc::new(SystemClock))
    }

    /// Create a new [`DelegationAuthHandler`] with `policy`, a random server [`SecretKey`], and
    /// an explicit [`SharedClock`].
    pub fn with_clock(policy: TurnAdmissionPolicy, clock: SharedClock) -> Self {
        Self::with_server_key_and_clock(policy, SecretKey::random(), clock)
    }

    /// Create a new [`DelegationAuthHandler`] with an explicit `server_key` and [`SharedClock`].
    pub fn with_server_key_and_clock(
        policy: TurnAdmissionPolicy,
        server_key: SecretKey,
        clock: SharedClock,
    ) -> Self {
        Self {
            policy,
            server_key,
            clock,
            cache: Mutex::new(VerifiedAuthCache::new(
                DEFAULT_VERIFIED_DELEGATION_CACHE_CAPACITY,
            )),
            expected_allocation_owners: Mutex::new(HashMap::new()),
        }
    }

    /// Return the configured [`TurnAdmissionPolicy`].
    pub fn policy(&self) -> &TurnAdmissionPolicy {
        &self.policy
    }

    /// Borrow the server's secp256k1 [`SecretKey`].
    pub const fn server_key(&self) -> &SecretKey {
        &self.server_key
    }

    /// Return the server's compressed secp256k1 [`PublicKey<33>`].
    pub fn server_pubkey(&self) -> PublicKey<33> {
        self.server_key.pubkey()
    }

    pub(crate) fn set_expected_allocation_owner(
        &self,
        src_addr: SocketAddr,
        owner_username: Option<String>,
    ) {
        let mut map = self
            .expected_allocation_owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(owner) = owner_username {
            map.insert(src_addr, owner);
        } else {
            map.remove(&src_addr);
        }
    }

    fn expected_allocation_owner(&self, src_addr: SocketAddr) -> Option<String> {
        self.expected_allocation_owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&src_addr)
            .cloned()
    }

    /// Verify a TURN `username` and `realm` at the handler's current clock instant and compute
    /// the 16-byte RFC 5389 `MESSAGE-INTEGRITY` key.
    pub fn verify_and_derive_key(
        &self,
        username: &str,
        realm: &str,
    ) -> Result<(VerifiedTurnAuth, Vec<u8>)> {
        self.verify_and_derive_key_for_allocation(username, realm, false)
    }

    fn verify_and_derive_key_for_allocation(
        &self,
        username: &str,
        realm: &str,
        established_allocation: bool,
    ) -> Result<(VerifiedTurnAuth, Vec<u8>)> {
        let now_ms = self.clock.now_ms();
        let (identity, _, _, _) = TurnCredentials::decode_username_parts(username)?;
        let verification_ts_ms = match (&identity, established_allocation) {
            (TurnAuthIdentity::Delegation(delegation), true) => delegation.created_at_ms(),
            _ => now_ms,
        };
        let cache_entry = match &identity {
            TurnAuthIdentity::Delegation(delegation) => {
                CacheEntry::Delegation(delegation.digest()?)
            }
            TurnAuthIdentity::Account(_) => {
                CacheEntry::DirectAccount(keccak256(username.as_bytes()))
            }
        };

        let is_cached = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&cache_entry);

        let verified = if is_cached {
            TurnCredentials::verify_established_at(username, &self.server_key, verification_ts_ms)?
        } else {
            let verified =
                TurnCredentials::verify_at(username, &self.server_key, verification_ts_ms)?;
            self.cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(cache_entry);
            verified
        };

        let did = verified.did();
        if !self.policy.is_admitted(did) {
            return Err(TurnError::DelegatorNotAdmitted(did));
        }

        let key = generate_auth_key(username, realm, &verified.credential);
        Ok((verified, key))
    }
}

impl AuthHandler for DelegationAuthHandler {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        src_addr: SocketAddr,
    ) -> std::result::Result<Vec<u8>, turn::Error> {
        let established_allocation = match self.expected_allocation_owner(src_addr) {
            Some(expected_owner) if expected_owner == username => true,
            Some(_) => {
                tracing::debug!(
                    %src_addr,
                    %realm,
                    "rejected TURN request: username does not match established allocation owner"
                );
                return Err(turn::Error::ErrNoSuchUser);
            }
            None => false,
        };
        match self.verify_and_derive_key_for_allocation(username, realm, established_allocation) {
            Ok((verified, key)) => {
                match &verified.identity {
                    TurnAuthIdentity::Delegation(delegation) => {
                        tracing::debug!(
                            delegator = %delegation.delegator_did(),
                            delegatee = %delegation.delegatee_did(),
                            %src_addr,
                            %realm,
                            established_allocation,
                            "admitted TURN request via Delegation signature"
                        );
                    }
                    TurnAuthIdentity::Account(account) => {
                        tracing::debug!(
                            did = %account.did(),
                            %src_addr,
                            %realm,
                            "admitted TURN request via direct DID/Account signature"
                        );
                    }
                }
                Ok(key)
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    %src_addr,
                    %realm,
                    "rejected TURN request authentication"
                );
                Err(turn::Error::ErrNoSuchUser)
            }
        }
    }
}
