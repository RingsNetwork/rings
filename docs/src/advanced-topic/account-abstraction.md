# Delegated Signing

## Roles

Rings separates the identity authorizing a signing key from the key that signs individual messages:

- The **delegator** is an external account identified by an account DID or verification key.
- The **delegatee** is the holder of a generated signing key, identified by the DID derived from its public key.
- A **delegation** is the signed proof that binds the delegatee DID to the delegator and a validity period.

The delegator signs the delegation proof. The delegatee key then signs messages while that delegation is valid. The delegator's private key is not given to the Rings node.

A delegation is not a network identifier. The relay transport's `RelaySessionId` identifies a TCP connection or UDP flow scoped to a peer, namespace, and initiator; it does not identify the delegator or delegatee.

## External delegators

A delegator can be represented by a secp256k1, secp256r1, EIP-191, BIP-137, Ed25519, or BLS12-381 account verifier. The account algorithm and encoded entity are provided when constructing a delegation builder.

## Rust API

`DelegationBuilder` creates a delegatee key and the corresponding signed `Delegation`. The caller signs `unsigned_proof()` with the delegator and supplies that signature with `set_delegator_signature()`:

```rust
use rings_core::delegation::DelegationBuilder;

let builder = DelegationBuilder::new(delegator_entity, delegator_type);
let proof = builder.unsigned_proof();
let delegator_signature = delegator_signer.sign(proof.as_bytes())?;
let delegatee_key = builder
    .set_delegator_signature(delegator_signature)
    .build()?;

let delegation = delegatee_key.delegation();
let message_signature = delegatee_key.sign(message)?;
assert!(delegation.verify(message, message_signature).is_ok());
```

`Delegation`, `DelegationBuilder`, `DelegationDigest`, and `DelegateeKey` are exported to WebAssembly as well.
