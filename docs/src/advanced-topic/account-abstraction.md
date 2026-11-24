# Account Abstraction & Delegated Signing

## Roles

Rings separates the identity authorizing a signing key from the key that signs individual messages:

- The **delegator** is an external account identified by an account DID or verification key.
- The **delegatee** is the holder of a generated secp256k1 session signing key, identified by the DID derived from its public key.
- A **delegation** is the signed proof that binds the delegatee DID to the delegator and a validity period.

The delegator signs the delegation proof. The delegatee key then signs messages while that delegation is valid. The delegator's private key is not given to the Rings node (unless an operator explicitly configures `delegator_key` in `config.yaml` for automatic in-process renewal).

A delegation is not a network identifier. The relay transport's `RelaySessionId` identifies a TCP connection or UDP flow scoped to a peer, namespace, and initiator; it does not identify the delegator or delegatee.

## External delegators & signature schemes

Six account verification algorithms are supported across native, browser, and FFI hosts:

| `account_type` | Curve / standard | Signature size | Typical source |
|---|---|---|---|
| `secp256k1` | ECDSA secp256k1 | 65 bytes (`r \|\| s \|\| v`) | Native secp256k1 keypair / Bitcoin-style |
| `eip191` | Ethereum personal_sign | 65 bytes (`r \|\| s \|\| v`) | MetaMask / `eth_account` |
| `bip137` | Bitcoin signed message | 65 bytes | Bitcoin wallet signers |
| `secp256r1` | NIST P-256 | 64 bytes (`r \|\| s`) | WebCrypto P-256 |
| `ed25519` | Ed25519 | 64 bytes | Phantom / Solana wallet |
| `bls12-381` | BLS12-381 | 96 bytes | BLS threshold / validator keys |

## Validity window, signing horizon & rotation

- **Validity window**: a delegation created at `ts_ms` with `ttl_ms` (default 30 days = `2_592_000_000` ms) is valid across `[ts_ms - 3000 ms, ts_ms + ttl_ms]`; premature timestamps outside the 3-second clock skew tolerance fail with `DelegationNotYetValid`.
- **Signing horizon (`25 s` before expiry)**: outbound frames are refused once `now_ms >= expires_at_ms - 25_000` (`DelegationHorizonReached`), preventing frames from expiring in flight.
- **Load-time key verification**: loading a `DelegateeKey` verifies that its secret scalar matches the authorized `delegatee_did` (`DelegateeKeyMismatch`).
- **In-process rotation**: `Processor::renew_delegatee_key` atomically swaps in a refreshed `DelegateeKey` while keeping the displaced key in memory until its original expiry `e` so in-flight mail and onion hops still decrypt cleanly.

## Rust API

`DelegationBuilder` creates a delegatee key and the corresponding signed `Delegation`. The caller signs `try_unsigned_proof()` (or `unsigned_proof()`), optionally sets `set_ttl(ttl_ms)` and `set_ts(ts_ms)`, supplies the signature with `set_delegator_signature()`, and finalizes via `build()` or `build_at(now_ms)`:

```rust
use rings_core::delegation::DelegationBuilder;

let builder = DelegationBuilder::new(delegator_entity, delegator_type)
    .set_ttl(30 * 24 * 60 * 60 * 1000);
let proof = builder.try_unsigned_proof()?;
let delegator_signature = delegator_signer.sign(proof.as_bytes())?;
let delegatee_key = builder
    .set_delegator_signature(delegator_signature)
    .build()?;

let delegation = delegatee_key.delegation();
let message_signature = delegatee_key.sign(message)?;
assert!(delegation.verify(message, message_signature).is_ok());
```

`Delegation`, `DelegationBuilder`, `DelegationDigest`, and `DelegateeKey` are exported to WebAssembly as well.
