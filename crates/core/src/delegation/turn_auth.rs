use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use super::Account;
use super::DelegateeKey;
use super::Delegation;
use super::DelegatorAuthorization;
use crate::dht::Did;
use crate::ecc::keys::SigningSecretKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::utils;

/// Domain separator for TURN long-term credential signatures produced by a [`DelegateeKey`] or
/// direct [`Account`] / [`Did`] signer.
pub const TURN_AUTH_DOMAIN: &str = "rings-turn-auth-v1";

/// Maximum byte length of a STUN `USERNAME` attribute under RFC 5389 §15.3 (`< 513` bytes).
pub const MAX_STUN_USERNAME_BYTES: usize = 512;

/// Pack the domain-separated transcript signed by a [`DelegateeKey`] for TURN authentication.
///
/// The transcript binds the signature to the exact [`Delegation`] via its content address
/// [`Delegation::digest`], which commits to `(delegatee_did, delegator, ts_ms, ttl_ms,
/// delegator_signature)`.
pub fn pack_turn_auth(delegation: &Delegation) -> Result<Vec<u8>> {
    let digest_hex = hex::encode(delegation.digest()?.into_bytes());
    Ok(format!("{TURN_AUTH_DOMAIN}\n{digest_hex}").into_bytes())
}

/// Pack the domain-separated transcript signed directly by an [`Account`] for TURN authentication.
///
/// Direct account/DID signatures carry no delegation TTL and therefore do not expire when a
/// session [`DelegateeKey`] rotates or expires.
pub fn pack_turn_account_auth(account: &Account) -> Vec<u8> {
    let verifier = account.account_verifier();
    format!(
        "{TURN_AUTH_DOMAIN}\n{}\n{}",
        verifier.algorithm().as_str(),
        verifier.did()
    )
    .into_bytes()
}

/// Pack the domain-separated transcript signed directly by a secp256k1 [`Did`] (`SecretKey`) for
/// TURN authentication.
pub fn pack_turn_did_auth(did: Did) -> Vec<u8> {
    pack_turn_account_auth(&Account::Secp256k1(did))
}

/// Verified Rings identity that authenticated a TURN request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnAuthIdentity {
    /// Authenticated via a time-bounded [`Delegation`] signed by its [`DelegateeKey`].
    Delegation(Delegation),
    /// Authenticated via a direct signature from an [`Account`] (such as `Account::Secp256k1(did)`).
    Account(Account),
}

impl TurnAuthIdentity {
    /// Return the account [`Did`] represented by this TURN authentication identity.
    pub fn did(&self) -> Did {
        match self {
            Self::Delegation(delegation) => delegation.delegator_did(),
            Self::Account(account) => account.did(),
        }
    }

    /// Borrow the underlying [`Delegation`] when authenticated via a [`DelegateeKey`].
    pub fn delegation(&self) -> Option<&Delegation> {
        match self {
            Self::Delegation(delegation) => Some(delegation),
            Self::Account(_) => None,
        }
    }

    /// Borrow the authorizing or signing [`Account`].
    pub fn account(&self) -> &Account {
        match self {
            Self::Delegation(delegation) => &delegation.delegator,
            Self::Account(account) => account,
        }
    }
}

/// Signature-backed TURN credentials derived from either a [`DelegateeKey`] or a direct
/// [`Did`] / [`Account`] signing key.
///
/// Standard WebRTC clients (browser `RTCPeerConnection` and `webrtc-rs`) authenticate to TURN
/// servers using the RFC 5389 / RFC 5766 long-term credential mechanism: the client sends
/// `USERNAME` in cleartext and proves knowledge of `credential` (the password) via
/// `MESSAGE-INTEGRITY = HMAC-SHA1(MD5(username : realm : credential), stun_packet)`.
///
/// Because `credential` is never transmitted in cleartext over STUN, [`TurnCredentials`] encodes:
/// - **Delegated mode** ([`TurnCredentials::from_delegatee_key`]):
///   `username = <base58(codec(delegation))>.<base58(delegatee_sig)>`, `credential = <base58(delegatee_sig)>`
/// - **Direct DID mode** ([`TurnCredentials::from_secret_key`], [`TurnCredentials::from_did_signature`]):
///   `username = <0x_did>.<base58(did_sig)>`, `credential = <base58(did_sig)>`
/// - **Direct Account mode** ([`TurnCredentials::from_signing_key`], [`TurnCredentials::from_account_signature`]):
///   `username = <base58(codec(account))>.<base58(account_sig)>`, `credential = <base58(account_sig)>`
///
/// A TURN server's `AuthHandler` decodes `username`, verifies the signature, and derives the exact
/// same `MD5(username : realm : credential)` key without storing any user/password table or shared
/// secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCredentials {
    /// STUN/TURN `USERNAME` attribute carrying `<claim>.<base58(signature)>`.
    pub username: String,
    /// STUN/TURN password (`credential`), equal to `<base58(signature)>`.
    pub credential: String,
}

/// Verified TURN authentication extracted from a STUN `USERNAME` attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedTurnAuth {
    /// The verified [`TurnAuthIdentity`] (`Delegation` or direct `Account` / `Did`) carried in
    /// the `USERNAME` attribute.
    pub identity: TurnAuthIdentity,
    /// The Base58-encoded signature used as the TURN password (`credential`).
    pub credential: String,
}

impl VerifiedTurnAuth {
    /// Return the authenticated [`Did`] (either the delegator DID or the direct signing DID).
    pub fn did(&self) -> Did {
        self.identity.did()
    }

    /// Alias for [`Self::did`]: the account [`Did`] that authorized or directly signed the request.
    pub fn delegator_did(&self) -> Did {
        self.identity.did()
    }

    /// Borrow the verified [`Delegation`] if the client authenticated with a [`DelegateeKey`].
    pub fn delegation(&self) -> Option<&Delegation> {
        self.identity.delegation()
    }

    /// Borrow the verified [`Account`] that authorized or directly signed the request.
    pub fn account(&self) -> &Account {
        self.identity.account()
    }
}

impl TurnCredentials {
    /// Sign TURN credentials using a session `delegatee_key`.
    pub fn from_delegatee_key(delegatee_key: &DelegateeKey) -> Result<Self> {
        let delegation = delegatee_key.delegation();
        let transcript = pack_turn_auth(&delegation)?;
        let sig = delegatee_key.sign(&transcript)?;
        let delegation_bytes =
            rings_codec::serialize(&delegation).map_err(Error::CodecSerialize)?;
        let delegation_b58 = base58_monero::encode(&delegation_bytes).map_err(|_| Error::Encode)?;
        Self::from_claim_and_sig(delegation_b58, &sig)
    }

    /// Sign non-expiring TURN credentials directly with a secp256k1 [`SecretKey`] (`Did`).
    pub fn from_secret_key(secret_key: &SecretKey) -> Result<Self> {
        let did = Did::from(secret_key.address());
        let transcript = pack_turn_did_auth(did);
        let sig = secret_key.sign_raw(&transcript)?;
        Self::from_did_signature(did, sig)
    }

    /// Build non-expiring TURN credentials from a secp256k1 [`Did`] and its signature over
    /// [`pack_turn_did_auth`].
    pub fn from_did_signature(did: Did, signature: impl AsRef<[u8]>) -> Result<Self> {
        let account = Account::Secp256k1(did);
        let transcript = pack_turn_account_auth(&account);
        if !account
            .account_verifier()
            .verify(&transcript, signature.as_ref())
        {
            return Err(Error::VerifySignatureFailed);
        }
        Self::from_claim_and_sig(did.to_string(), signature.as_ref())
    }

    /// Sign non-expiring TURN credentials directly with any [`SigningSecretKey`] (`Secp256k1`,
    /// `Eip191`, `Bip137`, `Secp256r1`, `Ed25519`, or `Bls12381`).
    pub fn from_signing_key(signing_key: &SigningSecretKey) -> Result<Self> {
        if let SigningSecretKey::Secp256k1(secret_key) = signing_key {
            return Self::from_secret_key(secret_key);
        }
        let account = Account::from(signing_key.public_key()?);
        let transcript = pack_turn_account_auth(&account);
        let sig = signing_key.sign_raw(&transcript)?;
        Self::from_account_signature(account, sig)
    }

    /// Build non-expiring TURN credentials from an [`Account`] and its signature over
    /// [`pack_turn_account_auth`].
    pub fn from_account_signature(account: Account, signature: impl AsRef<[u8]>) -> Result<Self> {
        if let Account::Secp256k1(did) = account {
            return Self::from_did_signature(did, signature);
        }
        let transcript = pack_turn_account_auth(&account);
        if !account
            .account_verifier()
            .verify(&transcript, signature.as_ref())
        {
            return Err(Error::VerifySignatureFailed);
        }
        let account_bytes = rings_codec::serialize(&account).map_err(Error::CodecSerialize)?;
        let account_b58 = base58_monero::encode(&account_bytes).map_err(|_| Error::Encode)?;
        Self::from_claim_and_sig(account_b58, signature.as_ref())
    }

    fn from_claim_and_sig(claim: String, sig: &[u8]) -> Result<Self> {
        let sig_b58 = base58_monero::encode(sig).map_err(|_| Error::Encode)?;
        let username = format!("{claim}.{sig_b58}");
        if username.len() > MAX_STUN_USERNAME_BYTES {
            return Err(Error::Encode);
        }
        Ok(Self {
            username,
            credential: sig_b58,
        })
    }

    /// Format this credential pair into a `turn://` or `turns://` server URL when the URL carries
    /// no explicit username/password.
    ///
    /// `stun://` URLs and `turn://` URLs that already specify credentials (`turn://user:pass@host`)
    /// are returned unchanged.
    pub fn apply_to_ice_server_url(&self, raw_url: &str) -> String {
        let trimmed = raw_url.trim();
        let Some((scheme, rest)) = trimmed.split_once("://") else {
            return trimmed.to_string();
        };
        if !scheme.eq_ignore_ascii_case("turn") && !scheme.eq_ignore_ascii_case("turns") {
            return trimmed.to_string();
        }
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = rest.get(..authority_end).unwrap_or(rest);
        let path_suffix = rest.get(authority_end..).unwrap_or("");
        if let Some((userinfo, host_port)) = authority.rsplit_once('@') {
            if !userinfo.trim_matches(':').is_empty() {
                return trimmed.to_string();
            }
            if host_port.is_empty() {
                return trimmed.to_string();
            }
            return format!(
                "{scheme}://{}:{}@{host_port}{path_suffix}",
                self.username, self.credential
            );
        }
        if authority.is_empty() {
            return trimmed.to_string();
        }
        format!(
            "{scheme}://{}:{}@{authority}{path_suffix}",
            self.username, self.credential
        )
    }

    /// Apply these TURN credentials to every bare `turn://` or `turns://` entry in a
    /// `;`-separated ICE server configuration string.
    pub fn apply_to_ice_servers(&self, ice_servers: &str) -> String {
        let trimmed = ice_servers.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        trimmed
            .split(';')
            .map(|entry| self.apply_to_ice_server_url(entry))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Decode and verify a STUN `USERNAME` attribute as of the current system clock.
    pub fn verify(username: &str) -> Result<VerifiedTurnAuth> {
        Self::verify_at(username, utils::get_epoch_ms())
    }

    /// Decode and verify a STUN `USERNAME` attribute at `at_ms` (Unix epoch milliseconds).
    ///
    /// - For [`TurnAuthIdentity::Delegation`], verifies both the delegator's signature on the
    ///   [`Delegation`] (plus its validity window at `at_ms`) and the delegatee's signature.
    /// - For [`TurnAuthIdentity::Account`], verifies the direct account/DID signature over
    ///   [`pack_turn_account_auth`].
    pub fn verify_at(username: &str, at_ms: u128) -> Result<VerifiedTurnAuth> {
        Self::verify_under(username, at_ms, DelegatorAuthorization::Verify)
    }

    /// Decode and verify a STUN `USERNAME` attribute at `at_ms` when the underlying identity
    /// authorization has already been verified and cached.
    ///
    /// For [`TurnAuthIdentity::Delegation`], both the delegation's validity window
    /// `[ts_ms - TS_OFFSET_TOLERANCE_MS, ts_ms + ttl_ms]` at `at_ms` and the delegatee's signature
    /// are still checked on every call.
    pub fn verify_established_at(username: &str, at_ms: u128) -> Result<VerifiedTurnAuth> {
        Self::verify_under(username, at_ms, DelegatorAuthorization::Established)
    }

    /// Decode the `(TurnAuthIdentity, sig_bytes, sig_b58)` parts from `username` without running
    /// signature verification yet.
    pub fn decode_username_parts(username: &str) -> Result<(TurnAuthIdentity, Vec<u8>, &str)> {
        if username.len() > MAX_STUN_USERNAME_BYTES {
            return Err(Error::Decode);
        }
        let (claim_str, sig_b58) = username.split_once('.').ok_or(Error::Decode)?;
        if claim_str.is_empty() || sig_b58.is_empty() || sig_b58.contains('.') {
            return Err(Error::Decode);
        }
        let sig_bytes = base58_monero::decode(sig_b58).map_err(|_| Error::Decode)?;

        if claim_str.starts_with("0x") || claim_str.starts_with("0X") {
            let did = Did::from_str(claim_str)?;
            return Ok((
                TurnAuthIdentity::Account(Account::Secp256k1(did)),
                sig_bytes,
                sig_b58,
            ));
        }

        if let Ok(claim_bytes) = base58_monero::decode(claim_str) {
            if let Ok(delegation) = rings_codec::deserialize::<Delegation>(&claim_bytes) {
                return Ok((TurnAuthIdentity::Delegation(delegation), sig_bytes, sig_b58));
            }
            if let Ok(account) = rings_codec::deserialize::<Account>(&claim_bytes) {
                return Ok((TurnAuthIdentity::Account(account), sig_bytes, sig_b58));
            }
        }

        if claim_str.len() == 40 && claim_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            let did = Did::from_str(claim_str)?;
            return Ok((
                TurnAuthIdentity::Account(Account::Secp256k1(did)),
                sig_bytes,
                sig_b58,
            ));
        }

        Err(Error::Decode)
    }

    pub(crate) fn verify_under(
        username: &str,
        at_ms: u128,
        authorization: DelegatorAuthorization,
    ) -> Result<VerifiedTurnAuth> {
        let (identity, sig_bytes, sig_b58) = Self::decode_username_parts(username)?;
        match &identity {
            TurnAuthIdentity::Delegation(delegation) => {
                let transcript = pack_turn_auth(delegation)?;
                delegation.verify_under(&transcript, &sig_bytes, at_ms, authorization)?;
            }
            TurnAuthIdentity::Account(account) => {
                if matches!(authorization, DelegatorAuthorization::Verify) {
                    let transcript = pack_turn_account_auth(account);
                    if !account.account_verifier().verify(&transcript, &sig_bytes) {
                        return Err(Error::VerifySignatureFailed);
                    }
                }
            }
        }
        Ok(VerifiedTurnAuth {
            identity,
            credential: sig_b58.to_string(),
        })
    }
}

impl DelegateeKey {
    /// Generate signature-backed [`TurnCredentials`] from this [`DelegateeKey`].
    pub fn turn_credentials(&self) -> Result<TurnCredentials> {
        TurnCredentials::from_delegatee_key(self)
    }

    /// Populate bare `turn://` and `turns://` URLs in a `;`-separated ICE server string with
    /// [`TurnCredentials`] signed by this [`DelegateeKey`].
    pub fn sign_ice_servers(&self, ice_servers: &str) -> Result<String> {
        Ok(self.turn_credentials()?.apply_to_ice_servers(ice_servers))
    }
}

impl SecretKey {
    /// Generate non-expiring signature-backed [`TurnCredentials`] signed directly by this
    /// secp256k1 [`SecretKey`] (`Did`).
    pub fn turn_credentials(&self) -> Result<TurnCredentials> {
        TurnCredentials::from_secret_key(self)
    }

    /// Populate bare `turn://` and `turns://` URLs in a `;`-separated ICE server string with
    /// non-expiring [`TurnCredentials`] signed directly by this [`SecretKey`].
    pub fn sign_ice_servers(&self, ice_servers: &str) -> Result<String> {
        Ok(self.turn_credentials()?.apply_to_ice_servers(ice_servers))
    }
}

impl SigningSecretKey {
    /// Generate non-expiring signature-backed [`TurnCredentials`] signed directly by this
    /// [`SigningSecretKey`].
    pub fn turn_credentials(&self) -> Result<TurnCredentials> {
        TurnCredentials::from_signing_key(self)
    }

    /// Populate bare `turn://` and `turns://` URLs in a `;`-separated ICE server string with
    /// non-expiring [`TurnCredentials`] signed directly by this [`SigningSecretKey`].
    pub fn sign_ice_servers(&self, ice_servers: &str) -> Result<String> {
        Ok(self.turn_credentials()?.apply_to_ice_servers(ice_servers))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rings_transport::ice_server::IceServer;

    use super::*;

    #[test]
    fn turn_credentials_round_trip_and_verify_with_delegatee_key() {
        let account_key = SecretKey::random();
        let ts_ms = 1_700_000_000_000;
        let ttl_ms = 60_000;
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();

        let creds = delegatee_key.turn_credentials().unwrap();
        assert!(creds.username.len() <= MAX_STUN_USERNAME_BYTES);
        assert!(creds.username.ends_with(&format!(".{}", creds.credential)));

        let verified = TurnCredentials::verify_at(&creds.username, ts_ms + 1_000).unwrap();
        assert_eq!(verified.credential, creds.credential);
        assert_eq!(verified.delegation(), Some(&delegatee_key.delegation()));
        assert_eq!(verified.did(), delegatee_key.delegator_did());
    }

    #[test]
    fn turn_credentials_round_trip_with_direct_did_secret_key_and_never_expire() {
        let secret_key = SecretKey::random();
        let did = Did::from(secret_key.address());
        let creds = secret_key.turn_credentials().unwrap();

        assert!(creds.username.starts_with(&format!("{did}.")));
        assert!(creds.username.len() <= MAX_STUN_USERNAME_BYTES);

        // Verifies at any timestamp (including u128::MAX) because direct DID signatures have no TTL.
        for at_ms in [0, 1_700_000_000_000, u128::MAX] {
            let verified = TurnCredentials::verify_at(&creds.username, at_ms).unwrap();
            assert_eq!(verified.did(), did);
            assert_eq!(verified.delegation(), None);
            assert_eq!(verified.account(), &Account::Secp256k1(did));
            assert_eq!(verified.credential, creds.credential);
        }

        // Forged DID or forged signature is rejected.
        let other_key = SecretKey::random();
        let other_creds = other_key.turn_credentials().unwrap();
        let forged_username = format!("{did}.{}", other_creds.credential);
        assert!(matches!(
            TurnCredentials::verify(&forged_username),
            Err(Error::VerifySignatureFailed)
        ));
    }

    #[test]
    fn turn_credentials_support_all_direct_signing_secret_key_algorithms() {
        let secp = SecretKey::random();
        let keys = [
            SigningSecretKey::Secp256k1(secp.clone()),
            SigningSecretKey::Eip191(secp.clone()),
            SigningSecretKey::Bip137(secp.clone()),
            SigningSecretKey::Secp256r1(secp),
            SigningSecretKey::random_ed25519(),
            SigningSecretKey::random_bls12381().unwrap(),
        ];

        for key in keys {
            let expected_did = key.public_key().unwrap().did();
            let creds = key.turn_credentials().unwrap();
            assert!(
                creds.username.len() <= MAX_STUN_USERNAME_BYTES,
                "algorithm {} username length {} exceeds limit",
                key.algorithm(),
                creds.username.len()
            );
            let verified = TurnCredentials::verify_at(&creds.username, u128::MAX).unwrap();
            assert_eq!(verified.did(), expected_did, "{}", key.algorithm());
            assert_eq!(verified.credential, creds.credential);
        }
    }

    #[test]
    fn widest_delegation_fits_within_stun_username_limit() {
        let widest_key = Delegation::widest_key_for_test();
        let creds = TurnCredentials::from_delegatee_key(&widest_key).unwrap();
        assert!(
            creds.username.len() <= MAX_STUN_USERNAME_BYTES,
            "widest username length {} exceeds STUN limit {}",
            creds.username.len(),
            MAX_STUN_USERNAME_BYTES
        );
    }

    #[test]
    fn turn_credentials_reject_expired_and_not_yet_valid_delegations() {
        let account_key = SecretKey::random();
        let ts_ms = 1_700_000_000_000;
        let ttl_ms = 60_000;
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();
        let creds = delegatee_key.turn_credentials().unwrap();

        assert!(matches!(
            TurnCredentials::verify_at(&creds.username, ts_ms + u128::from(ttl_ms) + 1),
            Err(Error::DelegationExpired)
        ));
        assert!(matches!(
            TurnCredentials::verify_at(
                &creds.username,
                ts_ms - crate::consts::TS_OFFSET_TOLERANCE_MS - 1
            ),
            Err(Error::DelegationNotYetValid)
        ));
    }

    #[test]
    fn turn_credentials_reject_forged_signature_from_other_delegatee() {
        let account_key = SecretKey::random();
        let other_key = SecretKey::random();
        let ts_ms = 1_700_000_000_000;
        let ttl_ms = 60_000;
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();
        let other_delegatee = DelegateeKey::new_with_seckey_at(&other_key, ttl_ms, ts_ms).unwrap();

        let creds = delegatee_key.turn_credentials().unwrap();
        let other_creds = other_delegatee.turn_credentials().unwrap();
        let (delegation_b58, _) = creds.username.split_once('.').unwrap();
        let forged_username = format!("{delegation_b58}.{}", other_creds.credential);

        assert!(matches!(
            TurnCredentials::verify_at(&forged_username, ts_ms + 1_000),
            Err(Error::VerifySignatureFailed)
        ));
    }

    #[test]
    fn sign_ice_servers_populates_bare_turn_urls_and_preserves_stun_and_explicit_auth() {
        let account_key = SecretKey::random();
        let delegatee_key =
            DelegateeKey::new_with_seckey_at(&account_key, 60_000, 1_700_000_000_000).unwrap();
        let creds = delegatee_key.turn_credentials().unwrap();

        let raw = "stun://stun.l.google.com:19302;turn://turn.rings.rs:3478/relay;turn://custom:secret@other.turn.rs:3478";
        let signed = delegatee_key.sign_ice_servers(raw).unwrap();
        let parsed = IceServer::vec_from_str(&signed).unwrap();
        assert_eq!(parsed.len(), 3);

        assert_eq!(parsed[0].urls, vec!["stun:stun.l.google.com:19302"]);
        assert_eq!(parsed[0].username, "");
        assert_eq!(parsed[0].credential, "");

        assert_eq!(parsed[1].urls, vec!["turn:turn.rings.rs:3478/relay"]);
        assert_eq!(parsed[1].username, creds.username);
        assert_eq!(parsed[1].credential, creds.credential);

        assert_eq!(parsed[2].urls, vec!["turn:other.turn.rs:3478"]);
        assert_eq!(parsed[2].username, "custom");
        assert_eq!(parsed[2].credential, "secret");

        let single =
            IceServer::from_str(&creds.apply_to_ice_server_url("turn://127.0.0.1:3478")).unwrap();
        assert_eq!(single.urls, vec!["turn:127.0.0.1:3478"]);
        assert_eq!(single.username, creds.username);
        assert_eq!(single.credential, creds.credential);
    }
}
