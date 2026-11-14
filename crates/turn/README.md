# rings-turn

Stateless TURN server for Rings Network using **"signature as password"** authentication instead of traditional username/password databases.

## How It Works

Standard WebRTC clients (both browser `RTCPeerConnection` and native `webrtc-rs`) authenticate to TURN servers using the RFC 5389 / RFC 5766 long-term credential mechanism:

1. The client sends `USERNAME` in cleartext and proves knowledge of `credential` (the TURN password) via `MESSAGE-INTEGRITY = HMAC-SHA1(MD5(username : realm : credential), stun_packet)`.
2. `rings-core` supports two signature modes via `TurnCredentials`:
   - **Direct DID / Account signature (non-expiring)**:
     - Secp256k1 `SecretKey` / `Did`:
       - `username`: `<0x_did>.<base58(did_sig)>`
       - `credential` (password): `<base58(did_sig)>`
       - Signed transcript: `"rings-turn-auth-v1\nsecp256k1\n<0x_did>"` (`pack_turn_did_auth`)
     - Any `Account` (`EIP191`, `BIP137`, `Secp256r1`, `Ed25519`, `Bls12381`):
       - `username`: `<base58(codec(account))>.<base58(account_sig)>`
       - `credential` (password): `<base58(account_sig)>`
       - Signed transcript: `"rings-turn-auth-v1\n<algorithm>\n<did>"` (`pack_turn_account_auth`)
   - **Session `DelegateeKey` signature (bounded by `Delegation` TTL)**:
     - `username`: `<base58(codec(delegation))>.<base58(delegatee_sig)>`
     - `credential` (password): `<base58(delegatee_sig)>`
     - Signed transcript: `"rings-turn-auth-v1\n<hex(delegation.digest())>"` (`pack_turn_auth`)
3. On the server side, `DelegationAuthHandler` implements `turn::auth::AuthHandler`:
   - Decodes `<claim>` and `<sig>` from `USERNAME`.
   - Verifies the direct `Did` / `Account` signature or the `Delegation` (delegator authorization, validity window `[ts_ms - tolerance, ts_ms + ttl_ms]`, and delegatee signature).
   - Checks `TurnAdmissionPolicy` (`AllowAnyDelegation` or `Allowlist(BTreeSet<Did>)`).
   - Computes and returns `generate_auth_key(username, realm, sig_b58)` so the underlying `turn` server validates `MESSAGE-INTEGRITY` without any stored user/password table.
