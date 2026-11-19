use std::str::FromStr;

use rings_core::delegation::Account;
use rings_core::delegation::DelegateeKey;
use rings_core::delegation::Delegation;
use rings_core::dht::Did;
use rings_core::ecc::keccak256;
use rings_core::ecc::keys::SigningSecretKey;
use rings_core::ecc::prime_order::NonIdentityPoint;
use rings_core::ecc::prime_order::NonZeroScalar;
use rings_core::ecc::prime_order::SHARED_SECRET_BYTES;
use rings_core::ecc::signers;
use rings_core::ecc::PublicKey;
use rings_core::ecc::Secp256k1;
use rings_core::ecc::SecretKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::utils;
use serde::Deserialize;
use serde::Serialize;
use zeroize::Zeroizing;

/// Domain separator for TURN long-term credential signatures produced by a [`DelegateeKey`] or
/// direct [`Account`] / [`Did`] signer.
pub const TURN_AUTH_DOMAIN: &str = "rings-turn-auth-v1";

/// Domain separator for deriving the TURN `MESSAGE-INTEGRITY` password (`credential`) from a
/// secp256k1 Diffie-Hellman shared secret and the verified signature bytes.
const TURN_CREDENTIAL_KDF_DOMAIN: &[u8] = b"rings-turn-credential-v1\n";

/// Maximum byte length of a STUN `USERNAME` attribute under RFC 5389 §15.3 (`< 513` bytes).
pub const MAX_STUN_USERNAME_BYTES: usize = 512;

/// Pack the domain-separated transcript signed by a [`DelegateeKey`] for TURN authentication
/// against `server_pubkey`.
///
/// The transcript binds the signature to both the exact [`Delegation`] (via its content address
/// [`Delegation::digest`]) and the target TURN server's secp256k1 public key `server_pubkey`.
pub fn pack_turn_auth(delegation: &Delegation, server_pubkey: PublicKey<33>) -> Result<Vec<u8>> {
    let digest_hex = hex::encode(delegation.digest()?.into_bytes());
    let server_pk_hex = hex::encode(server_pubkey.0);
    Ok(format!("{TURN_AUTH_DOMAIN}\n{digest_hex}\n{server_pk_hex}").into_bytes())
}

/// Pack the domain-separated transcript signed directly by an [`Account`] authorizing
/// `session_pubkey` for TURN authentication against `server_pubkey`.
///
/// Direct account/DID signatures carry no delegation TTL and therefore do not expire when a
/// session [`DelegateeKey`] rotates or expires.
pub fn pack_turn_account_auth(
    account: &Account,
    session_pubkey: PublicKey<33>,
    server_pubkey: PublicKey<33>,
) -> Vec<u8> {
    let verifier = account.account_verifier();
    format!(
        "{TURN_AUTH_DOMAIN}\n{}\n{}\n{}\n{}",
        verifier.algorithm().as_str(),
        verifier.did(),
        hex::encode(session_pubkey.0),
        hex::encode(server_pubkey.0),
    )
    .into_bytes()
}

/// Pack the domain-separated transcript signed directly by a secp256k1 [`Did`] (`SecretKey`) for
/// TURN authentication against `server_pubkey`.
pub fn pack_turn_did_auth(did: Did, server_pubkey: PublicKey<33>) -> Vec<u8> {
    format!(
        "{TURN_AUTH_DOMAIN}\nsecp256k1\n{did}\n{}",
        hex::encode(server_pubkey.0),
    )
    .into_bytes()
}

/// Attach `?pubkey=<base58(server_pubkey)>` (or `&pubkey=...`) to a `turn://` or `turns://` URL.
pub fn format_turn_server_url(raw_url: &str, server_pubkey: PublicKey<33>) -> Result<String> {
    let _ = NonIdentityPoint::<Secp256k1>::try_from(server_pubkey)?;
    let pk_b58 = server_pubkey.to_base58_string()?;
    let trimmed = raw_url.trim();
    if trimmed.contains('?') {
        Ok(format!("{trimmed}&pubkey={pk_b58}"))
    } else {
        Ok(format!("{trimmed}?pubkey={pk_b58}"))
    }
}

/// Extract a TURN server's secp256k1 [`PublicKey<33>`] from a `turn://` or `turns://` URL's
/// `?pubkey=<base58>` query parameter (or `turn://<base58_pubkey>@host:port` userinfo).
pub fn extract_turn_server_pubkey(raw_url: &str) -> Option<PublicKey<33>> {
    let trimmed = raw_url.trim();
    let (scheme, rest) = trimmed.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("turn") && !scheme.eq_ignore_ascii_case("turns") {
        return None;
    }

    if let Some((_, query_and_frag)) = rest.split_once('?') {
        let query = query_and_frag
            .split_once('#')
            .map_or(query_and_frag, |(q, _)| q);
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                if key.eq_ignore_ascii_case("pubkey") {
                    if let Some(pk) = decode_secp256k1_pubkey_b58(value) {
                        return Some(pk);
                    }
                }
            }
        }
    }

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end)?;
    let (userinfo, _host_port) = authority.rsplit_once('@')?;
    if !userinfo.contains(':') && !userinfo.is_empty() {
        return decode_secp256k1_pubkey_b58(userinfo);
    }
    None
}

fn decode_secp256k1_pubkey_b58(encoded: &str) -> Option<PublicKey<33>> {
    let pk = if let Ok(pk) = PublicKey::<33>::try_from_b58t(encoded) {
        pk
    } else {
        let bytes = base58_monero::decode(encoded).ok()?;
        let arr: [u8; 33] = bytes.try_into().ok()?;
        PublicKey(arr)
    };
    NonIdentityPoint::<Secp256k1>::try_from(pk).ok()?;
    Some(pk)
}

fn strip_pubkey_query_param(suffix: &str) -> String {
    let Some((before_q, after_q)) = suffix.split_once('?') else {
        return suffix.to_string();
    };
    let (query, frag) = match after_q.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (after_q, None),
    };
    let remaining_params: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            !pair.is_empty()
                && pair
                    .split_once('=')
                    .is_none_or(|(k, _)| !k.eq_ignore_ascii_case("pubkey"))
        })
        .collect();
    let mut out = before_q.to_string();
    if !remaining_params.is_empty() {
        out.push('?');
        out.push_str(&remaining_params.join("&"));
    }
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

fn secp256k1_shared_secret(
    secret_key: &SecretKey,
    peer_pubkey: PublicKey<33>,
) -> Result<Zeroizing<[u8; SHARED_SECRET_BYTES]>> {
    let peer = NonIdentityPoint::<Secp256k1>::try_from(peer_pubkey)?;
    let scalar = NonZeroScalar::<Secp256k1>::from_secret_key(secret_key);
    Ok(peer.shared_secret(&scalar))
}

fn derive_turn_credential(shared_secret: &[u8; SHARED_SECRET_BYTES], sig: &[u8]) -> Result<String> {
    let mut preimage = Zeroizing::new(Vec::with_capacity(
        TURN_CREDENTIAL_KDF_DOMAIN.len() + SHARED_SECRET_BYTES + sig.len(),
    ));
    preimage.extend_from_slice(TURN_CREDENTIAL_KDF_DOMAIN);
    preimage.extend_from_slice(shared_secret);
    preimage.extend_from_slice(sig);
    let digest = keccak256(&preimage);
    base58_monero::encode(&digest).map_err(|_| Error::Encode)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct AccountSessionClaim {
    account: Account,
    session_pubkey: PublicKey<33>,
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
            Self::Delegation(delegation) => delegation.delegator(),
            Self::Account(account) => account,
        }
    }
}

/// Signature- and ECDH-backed TURN credentials derived from a [`DelegateeKey`] or a direct
/// [`Did`] / [`Account`] signing key paired with the target TURN server's [`PublicKey<33>`].
///
/// Standard WebRTC clients (browser `RTCPeerConnection` and `webrtc-rs`) authenticate to TURN
/// servers using the RFC 5389 / RFC 5766 long-term credential mechanism: the client sends
/// `USERNAME` in cleartext and proves knowledge of `credential` (the password) via
/// `MESSAGE-INTEGRITY = HMAC-SHA1(MD5(username : realm : credential), stun_packet)`.
///
/// To prevent passive UDP observers from extracting `credential` out of the cleartext `USERNAME`:
/// - `username` carries only the public identity claim and signature (`<claim>.<base58(signature)>`).
/// - `credential` (the TURN password) is derived from the secp256k1 Diffie-Hellman shared secret
///   between the client's signing/session key and the TURN server's `server_pubkey`:
///   `base58(keccak256("rings-turn-credential-v1\n" || ECDH(client_sk, server_pk) || signature))`.
/// - The TURN server recovers/extracts `client_pk` after verifying the signature in `username`,
///   computes the matching `ECDH(server_sk, client_pk)`, and derives the exact same `credential`
///   without storing any per-user password table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCredentials {
    /// STUN/TURN `USERNAME` attribute carrying `<claim>.<base58(signature)>`.
    pub username: String,
    /// STUN/TURN password (`credential`) derived from the client-server ECDH shared secret.
    pub credential: String,
}

/// Verified TURN authentication extracted from a STUN `USERNAME` attribute and server key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedTurnAuth {
    /// The verified [`TurnAuthIdentity`] (`Delegation` or direct `Account` / `Did`) carried in
    /// the `USERNAME` attribute.
    pub identity: TurnAuthIdentity,
    /// The verified client/session secp256k1 public key used for Diffie-Hellman key agreement.
    pub client_pubkey: PublicKey<33>,
    /// The ECDH-derived TURN password (`credential`).
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
    /// Sign TURN credentials using a session `delegatee_key` for `server_pubkey`.
    pub fn from_delegatee_key(
        delegatee_key: &DelegateeKey,
        server_pubkey: PublicKey<33>,
    ) -> Result<Self> {
        let peer = NonIdentityPoint::<Secp256k1>::try_from(server_pubkey)?;
        let delegation = delegatee_key.delegation();
        let transcript = pack_turn_auth(&delegation, server_pubkey)?;
        let sig = delegatee_key.sign(&transcript)?;
        let shared = delegatee_key.diffie_hellman(&peer);
        let delegation_bytes =
            rings_codec::serialize(&delegation).map_err(Error::CodecSerialize)?;
        let delegation_b58 = base58_monero::encode(&delegation_bytes).map_err(|_| Error::Encode)?;
        Self::from_claim_sig_and_shared(delegation_b58, &sig, &shared)
    }

    /// Sign non-expiring TURN credentials directly with a secp256k1 [`SecretKey`] (`Did`) for
    /// `server_pubkey`.
    pub fn from_secret_key(secret_key: &SecretKey, server_pubkey: PublicKey<33>) -> Result<Self> {
        let did = Did::from(secret_key.address());
        let transcript = pack_turn_did_auth(did, server_pubkey);
        let sig = secret_key.sign_raw(&transcript)?;
        let shared = secp256k1_shared_secret(secret_key, server_pubkey)?;
        Self::from_claim_sig_and_shared(did.to_string(), &sig, &shared)
    }

    /// Build non-expiring TURN credentials from a secp256k1 [`Did`], a client `session_key`,
    /// `server_pubkey`, and the signature over
    /// `pack_turn_account_auth(&Account::Secp256k1(did), session_key.pubkey(), server_pubkey)`.
    pub fn from_did_signature(
        did: Did,
        session_key: &SecretKey,
        server_pubkey: PublicKey<33>,
        signature: impl AsRef<[u8]>,
    ) -> Result<Self> {
        Self::from_account_signature(
            Account::Secp256k1(did),
            session_key,
            server_pubkey,
            signature,
        )
    }

    /// Sign non-expiring TURN credentials directly with any [`SigningSecretKey`] (`Secp256k1`,
    /// `Eip191`, `Bip137`, `Secp256r1`, `Ed25519`, or `Bls12381`) for `server_pubkey`.
    pub fn from_signing_key(
        signing_key: &SigningSecretKey,
        server_pubkey: PublicKey<33>,
    ) -> Result<Self> {
        let session_key;
        let session_ref = match signing_key {
            SigningSecretKey::Secp256k1(secret_key) => {
                return Self::from_secret_key(secret_key, server_pubkey);
            }
            SigningSecretKey::Eip191(secret_key) | SigningSecretKey::Bip137(secret_key) => {
                secret_key
            }
            SigningSecretKey::Secp256r1(_)
            | SigningSecretKey::Ed25519(_)
            | SigningSecretKey::Bls12381(_) => {
                session_key = SecretKey::random();
                &session_key
            }
        };
        let account = Account::from(signing_key.public_key()?);
        let transcript = pack_turn_account_auth(&account, session_ref.pubkey(), server_pubkey);
        let sig = signing_key.sign_raw(&transcript)?;
        Self::from_account_signature(account, session_ref, server_pubkey, sig)
    }

    /// Build non-expiring TURN credentials from an [`Account`], a client `session_key`,
    /// `server_pubkey`, and the account's signature over [`pack_turn_account_auth`].
    pub fn from_account_signature(
        account: Account,
        session_key: &SecretKey,
        server_pubkey: PublicKey<33>,
        signature: impl AsRef<[u8]>,
    ) -> Result<Self> {
        let session_pubkey = session_key.pubkey();
        let shared = secp256k1_shared_secret(session_key, server_pubkey)?;
        let transcript = pack_turn_account_auth(&account, session_pubkey, server_pubkey);
        if !account
            .account_verifier()
            .verify(&transcript, signature.as_ref())
        {
            return Err(Error::VerifySignatureFailed);
        }
        let claim = AccountSessionClaim {
            account,
            session_pubkey,
        };
        let claim_bytes = rings_codec::serialize(&claim).map_err(Error::CodecSerialize)?;
        let claim_b58 = base58_monero::encode(&claim_bytes).map_err(|_| Error::Encode)?;
        Self::from_claim_sig_and_shared(claim_b58, signature.as_ref(), &shared)
    }

    fn from_claim_sig_and_shared(
        claim: String,
        sig: &[u8],
        shared_secret: &[u8; SHARED_SECRET_BYTES],
    ) -> Result<Self> {
        let sig_b58 = base58_monero::encode(sig).map_err(|_| Error::Encode)?;
        let username = format!("{claim}.{sig_b58}");
        if username.len() > MAX_STUN_USERNAME_BYTES {
            return Err(Error::Encode);
        }
        let credential = derive_turn_credential(shared_secret, sig)?;
        Ok(Self {
            username,
            credential,
        })
    }

    /// Format this credential pair into a `turn://` or `turns://` server URL when the URL carries
    /// no explicit `username:password`.
    ///
    /// Any `?pubkey=...` query parameter used for server public-key discovery is stripped from the
    /// resulting ICE server URL so standard WebRTC URL parsers accept it. `stun://` URLs and
    /// `turn://` URLs that already specify `username:password@host` are returned unchanged.
    pub fn apply_to_ice_server_url(&self, raw_url: &str) -> String {
        let trimmed = raw_url.trim();
        let Some((scheme, rest)) = trimmed.split_once("://") else {
            return trimmed.to_string();
        };
        if !scheme.eq_ignore_ascii_case("turn") && !scheme.eq_ignore_ascii_case("turns") {
            return trimmed.to_string();
        }
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = rest.get(..authority_end).unwrap_or(rest);
        let raw_suffix = rest.get(authority_end..).unwrap_or("");
        let clean_suffix = strip_pubkey_query_param(raw_suffix);
        if let Some((userinfo, host_port)) = authority.rsplit_once('@') {
            let has_explicit_user_pass = userinfo
                .split_once(':')
                .is_some_and(|(u, p)| !u.is_empty() && !p.is_empty());
            if has_explicit_user_pass {
                return trimmed.to_string();
            }
            if !userinfo.trim_matches(':').is_empty()
                && decode_secp256k1_pubkey_b58(userinfo).is_none()
            {
                return trimmed.to_string();
            }
            if host_port.is_empty() {
                return trimmed.to_string();
            }
            return format!(
                "{scheme}://{}:{}@{host_port}{clean_suffix}",
                self.username, self.credential
            );
        }
        if authority.is_empty() {
            return trimmed.to_string();
        }
        format!(
            "{scheme}://{}:{}@{authority}{clean_suffix}",
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

    /// Decode and verify a STUN `USERNAME` attribute against `server_key` as of the current
    /// system clock.
    pub fn verify(username: &str, server_key: &SecretKey) -> Result<VerifiedTurnAuth> {
        Self::verify_at(username, server_key, utils::get_epoch_ms())
    }

    /// Decode and verify a STUN `USERNAME` attribute against `server_key` at `at_ms`
    /// (Unix epoch milliseconds).
    ///
    /// - For [`TurnAuthIdentity::Delegation`], verifies both the delegator's signature on the
    ///   [`Delegation`] (plus its validity window at `at_ms`) and the delegatee's signature, then
    ///   derives `credential` via ECDH with `server_key`.
    /// - For [`TurnAuthIdentity::Account`], verifies the direct account/DID signature and derives
    ///   `credential` via ECDH with `server_key`.
    pub fn verify_at(
        username: &str,
        server_key: &SecretKey,
        at_ms: u128,
    ) -> Result<VerifiedTurnAuth> {
        Self::verify_with_mode(username, server_key, at_ms, false)
    }

    /// Decode and verify a STUN `USERNAME` attribute against `server_key` at `at_ms` when the
    /// underlying identity authorization has already been verified and cached.
    pub fn verify_established_at(
        username: &str,
        server_key: &SecretKey,
        at_ms: u128,
    ) -> Result<VerifiedTurnAuth> {
        Self::verify_with_mode(username, server_key, at_ms, true)
    }

    /// Decode the `(TurnAuthIdentity, optional_session_pubkey, sig_bytes, sig_b58)` parts from
    /// `username` without running signature verification yet.
    pub fn decode_username_parts(
        username: &str,
    ) -> Result<(TurnAuthIdentity, Option<PublicKey<33>>, Vec<u8>, &str)> {
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
                None,
                sig_bytes,
                sig_b58,
            ));
        }

        if let Ok(claim_bytes) = base58_monero::decode(claim_str) {
            if let Ok(delegation) = rings_codec::deserialize::<Delegation>(&claim_bytes) {
                return Ok((
                    TurnAuthIdentity::Delegation(delegation),
                    None,
                    sig_bytes,
                    sig_b58,
                ));
            }
            if let Ok(claim) = rings_codec::deserialize::<AccountSessionClaim>(&claim_bytes) {
                return Ok((
                    TurnAuthIdentity::Account(claim.account),
                    Some(claim.session_pubkey),
                    sig_bytes,
                    sig_b58,
                ));
            }
        }

        if claim_str.len() == 40 && claim_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            let did = Did::from_str(claim_str)?;
            return Ok((
                TurnAuthIdentity::Account(Account::Secp256k1(did)),
                None,
                sig_bytes,
                sig_b58,
            ));
        }

        Err(Error::Decode)
    }

    fn verify_with_mode(
        username: &str,
        server_key: &SecretKey,
        at_ms: u128,
        established: bool,
    ) -> Result<VerifiedTurnAuth> {
        let server_pubkey = server_key.pubkey();
        let (identity, session_pubkey, sig_bytes, _sig_b58) =
            Self::decode_username_parts(username)?;
        let client_pubkey = match (&identity, session_pubkey) {
            (TurnAuthIdentity::Delegation(delegation), _) => {
                let transcript = pack_turn_auth(delegation, server_pubkey)?;
                if established {
                    delegation.recover_established_signer_at(&transcript, &sig_bytes, at_ms)?
                } else {
                    delegation.recover_signer_at(&transcript, &sig_bytes, at_ms)?
                }
            }
            (TurnAuthIdentity::Account(Account::Secp256k1(did)), None) => {
                let transcript = pack_turn_did_auth(*did, server_pubkey);
                signers::secp256k1::recover(&transcript, &sig_bytes)
                    .ok()
                    .filter(|pk| Did::from(pk.address()) == *did)
                    .ok_or(Error::VerifySignatureFailed)?
            }
            (TurnAuthIdentity::Account(account), Some(session_pk)) => {
                let _ = NonIdentityPoint::<Secp256k1>::try_from(session_pk)?;
                if !established {
                    let transcript = pack_turn_account_auth(account, session_pk, server_pubkey);
                    if !account.account_verifier().verify(&transcript, &sig_bytes) {
                        return Err(Error::VerifySignatureFailed);
                    }
                }
                session_pk
            }
            (TurnAuthIdentity::Account(_), None) => return Err(Error::Decode),
        };
        let shared = secp256k1_shared_secret(server_key, client_pubkey)?;
        let credential = derive_turn_credential(&shared, &sig_bytes)?;
        Ok(VerifiedTurnAuth {
            identity,
            client_pubkey,
            credential,
        })
    }
}

fn sign_ice_servers_with(
    ice_servers: &str,
    mut make_creds: impl FnMut(PublicKey<33>) -> Result<TurnCredentials>,
) -> Result<String> {
    let trimmed = ice_servers.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let mut out = Vec::new();
    for entry in trimmed.split(';') {
        if let Some(server_pubkey) = extract_turn_server_pubkey(entry) {
            let creds = make_creds(server_pubkey)?;
            out.push(creds.apply_to_ice_server_url(entry));
        } else {
            out.push(entry.trim().to_string());
        }
    }
    Ok(out.join(";"))
}

/// Extension trait for signing [`TurnCredentials`] and populating `turn://` / `turns://`
/// ICE server URLs with [`DelegateeKey`], [`SecretKey`], or [`SigningSecretKey`].
pub trait TurnCredentialSigner {
    /// Generate signature- and ECDH-backed [`TurnCredentials`] for `server_pubkey`.
    fn turn_credentials(&self, server_pubkey: PublicKey<33>) -> Result<TurnCredentials>;

    /// Populate `turn://` and `turns://` URLs carrying `?pubkey=<base58(server_pubkey)>` in a
    /// `;`-separated ICE server string with [`TurnCredentials`] signed by `self`.
    fn sign_ice_servers(&self, ice_servers: &str) -> Result<String> {
        sign_ice_servers_with(ice_servers, |server_pubkey| {
            self.turn_credentials(server_pubkey)
        })
    }
}

impl TurnCredentialSigner for DelegateeKey {
    fn turn_credentials(&self, server_pubkey: PublicKey<33>) -> Result<TurnCredentials> {
        TurnCredentials::from_delegatee_key(self, server_pubkey)
    }
}

impl TurnCredentialSigner for SecretKey {
    fn turn_credentials(&self, server_pubkey: PublicKey<33>) -> Result<TurnCredentials> {
        TurnCredentials::from_secret_key(self, server_pubkey)
    }
}

impl TurnCredentialSigner for SigningSecretKey {
    fn turn_credentials(&self, server_pubkey: PublicKey<33>) -> Result<TurnCredentials> {
        TurnCredentials::from_signing_key(self, server_pubkey)
    }
}

#[cfg(test)]
mod tests {
    use rings_core::delegation::DelegationBuilder;

    use super::*;

    #[test]
    fn turn_credentials_round_trip_and_keep_password_out_of_cleartext_username() {
        let server_key = SecretKey::random();
        let server_pubkey = server_key.pubkey();
        let account_key = SecretKey::random();
        let ts_ms = 1_700_000_000_000;
        let ttl_ms = 60_000;
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();

        let creds = delegatee_key.turn_credentials(server_pubkey).unwrap();
        assert!(creds.username.len() <= MAX_STUN_USERNAME_BYTES);
        let (_, sig_suffix) = creds.username.split_once('.').unwrap();
        assert_ne!(
            creds.credential, sig_suffix,
            "TURN password must not equal the cleartext username signature suffix"
        );
        assert!(
            !creds.username.contains(&creds.credential),
            "cleartext TURN username must never contain the TURN password"
        );

        let verified =
            TurnCredentials::verify_at(&creds.username, &server_key, ts_ms + 1_000).unwrap();
        assert_eq!(verified.credential, creds.credential);
        assert_eq!(verified.delegation(), Some(&delegatee_key.delegation()));
        assert_eq!(verified.did(), delegatee_key.delegator_did());

        // Different server key cannot verify or derive the credential.
        let wrong_server_key = SecretKey::random();
        assert!(
            TurnCredentials::verify_at(&creds.username, &wrong_server_key, ts_ms + 1_000).is_err()
        );
    }

    #[test]
    fn turn_credentials_round_trip_with_direct_did_secret_key_and_never_expire() {
        let server_key = SecretKey::random();
        let server_pubkey = server_key.pubkey();
        let secret_key = SecretKey::random();
        let did = Did::from(secret_key.address());
        let creds = secret_key.turn_credentials(server_pubkey).unwrap();

        assert!(creds.username.starts_with(&format!("{did}.")));
        assert!(creds.username.len() <= MAX_STUN_USERNAME_BYTES);
        assert!(!creds.username.contains(&creds.credential));

        // Verifies at any timestamp (including u128::MAX) because direct DID signatures have no TTL.
        for at_ms in [0, 1_700_000_000_000, u128::MAX] {
            let verified = TurnCredentials::verify_at(&creds.username, &server_key, at_ms).unwrap();
            assert_eq!(verified.did(), did);
            assert_eq!(verified.delegation(), None);
            assert_eq!(verified.account(), &Account::Secp256k1(did));
            assert_eq!(verified.credential, creds.credential);
        }

        // Forged DID or forged signature is rejected.
        let other_key = SecretKey::random();
        let other_creds = other_key.turn_credentials(server_pubkey).unwrap();
        let (_, other_sig) = other_creds.username.split_once('.').unwrap();
        let forged_username = format!("{did}.{other_sig}");
        assert!(matches!(
            TurnCredentials::verify(&forged_username, &server_key),
            Err(Error::VerifySignatureFailed)
        ));
    }

    #[test]
    fn turn_credentials_support_all_direct_signing_secret_key_algorithms() {
        let server_key = SecretKey::random();
        let server_pubkey = server_key.pubkey();
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
            let creds = key.turn_credentials(server_pubkey).unwrap();
            assert!(creds.username.len() <= MAX_STUN_USERNAME_BYTES);
            assert!(!creds.username.contains(&creds.credential));
            let verified =
                TurnCredentials::verify_at(&creds.username, &server_key, u128::MAX).unwrap();
            assert_eq!(verified.did(), expected_did);
            assert_eq!(verified.credential, creds.credential);
        }
    }

    #[test]
    fn widest_delegation_fits_within_stun_username_limit() {
        let server_pubkey = SecretKey::random().pubkey();
        let bls_key = SigningSecretKey::random_bls12381().unwrap();
        let bls_pk = match bls_key.public_key().unwrap() {
            rings_core::ecc::VerificationPublicKey::Bls12381(pk) => pk,
            _ => unreachable!(),
        };
        let entity = base58_monero::encode_check(&bls_pk.0).unwrap();
        let builder = DelegationBuilder::new(entity, "bls12381".to_string())
            .set_ttl(u64::MAX)
            .set_ts(u128::MAX);
        let sig = bls_key
            .sign_raw(builder.unsigned_proof().as_bytes())
            .unwrap();
        let widest_key = builder
            .set_delegator_signature(sig)
            .build_at(u128::MAX)
            .unwrap();
        let creds = TurnCredentials::from_delegatee_key(&widest_key, server_pubkey).unwrap();
        assert!(creds.username.len() <= MAX_STUN_USERNAME_BYTES);
    }

    #[test]
    fn turn_credentials_reject_expired_and_not_yet_valid_delegations() {
        let server_key = SecretKey::random();
        let server_pubkey = server_key.pubkey();
        let account_key = SecretKey::random();
        let ts_ms = 1_700_000_000_000;
        let ttl_ms = 60_000;
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();
        let creds = delegatee_key.turn_credentials(server_pubkey).unwrap();

        assert!(matches!(
            TurnCredentials::verify_at(
                &creds.username,
                &server_key,
                ts_ms + u128::from(ttl_ms) + 1
            ),
            Err(Error::DelegationExpired)
        ));
        assert!(matches!(
            TurnCredentials::verify_at(
                &creds.username,
                &server_key,
                ts_ms - rings_core::consts::TS_OFFSET_TOLERANCE_MS - 1
            ),
            Err(Error::DelegationNotYetValid)
        ));
    }

    #[test]
    fn turn_credentials_reject_forged_signature_from_other_delegatee() {
        let server_key = SecretKey::random();
        let server_pubkey = server_key.pubkey();
        let account_key = SecretKey::random();
        let ts_ms = 1_700_000_000_000;
        let ttl_ms = 60_000;
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();
        let other_delegatee =
            DelegateeKey::new_with_seckey_at(&account_key, ttl_ms, ts_ms).unwrap();

        let creds = delegatee_key.turn_credentials(server_pubkey).unwrap();
        let other_creds = other_delegatee.turn_credentials(server_pubkey).unwrap();

        let (claim_part, _) = creds.username.split_once('.').unwrap();
        let (_, other_sig_part) = other_creds.username.split_once('.').unwrap();
        let forged_username = format!("{claim_part}.{other_sig_part}");

        assert!(matches!(
            TurnCredentials::verify_at(&forged_username, &server_key, ts_ms + 1_000),
            Err(Error::VerifySignatureFailed)
        ));
    }

    #[test]
    fn sign_ice_servers_populates_turn_urls_with_server_pubkey_and_preserves_stun_and_explicit_auth(
    ) {
        let server_key = SecretKey::random();
        let server_pubkey = server_key.pubkey();
        let account_key = SecretKey::random();
        let delegatee_key =
            DelegateeKey::new_with_seckey_at(&account_key, 60_000, 1_700_000_000_000).unwrap();
        let creds = delegatee_key.turn_credentials(server_pubkey).unwrap();

        let turn_with_pk =
            format_turn_server_url("turn://198.51.100.10:3478?transport=udp", server_pubkey)
                .unwrap();
        let raw = format!("stun://stun.l.google.com:19302;{turn_with_pk};turn://bob:secret@198.51.100.11:3478");
        let signed = delegatee_key.sign_ice_servers(&raw).unwrap();
        let entries: Vec<&str> = signed.split(';').collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], "stun://stun.l.google.com:19302");
        assert_eq!(
            entries[1],
            format!(
                "turn://{}:{}@198.51.100.10:3478?transport=udp",
                creds.username, creds.credential
            )
        );
        assert_eq!(entries[2], "turn://bob:secret@198.51.100.11:3478");
    }
}
