# Provisional DRanking Service Receipts

Rings currently implements one narrow DRanking input: a versioned, countersigned receipt for an
authenticated liveness probe. It is a collection and persistence vertical slice. It is not the
finalized epoch ledger, trust algebra, committee protocol, or routing policy described by the full
DRanking design.

## ProbeV1 flow

1. The beneficiary sends `ProbeRequestV1` with a random nonce and a current five-minute epoch.
2. The provider returns `ProbeOfferV1`. The offer embeds the exact signed request transaction, an
   exact provider-signed completion transaction, the canonical claim, and the provider's claim
   attestation.
3. The beneficiary verifies the transcript against its one outstanding request, signs the same
   claim under the beneficiary role domain, and returns `ProbeAcknowledgementV1`.
4. The provider verifies the complete receipt under live rules and admits it to its bounded local
   evidence store.

The canonical claim binds `network_id`, service kind, provider account, beneficiary account,
epoch, nonce, units, request digest, and completion digest. Provider and beneficiary signatures
use different domains. Roles are account DIDs recovered from delegated session proofs, so a
session-key rotation does not change the account role.

`V1` is part of the canonical marker and signing domains. It prevents a signature over this field
layout from being interpreted as another receipt protocol. The release is still a hard cutover:
there is no legacy probe/report decoder, negotiation, or dual format.

## Verification boundaries

Cryptographic verification is portable: a later reader can verify the two account roles, the
canonical claim, and that each delegated proof was valid at its signing time. Live verification is
receiver-local: it additionally requires live delegated proofs and accepts only the current or an
adjacent provisional epoch at observation time. Persisted evidence records that observation time,
but replaying an old receipt never recreates live-observation status.

The freshness key is `(network_id, beneficiary_account, epoch, nonce)`. The same receipt digest is
a duplicate; a different receipt for an occupied freshness key is a conflict. Both are rejected.

## Storage and non-claims

The node keeps provisional evidence in a component separate from local peer measurements. Native
nodes use a separate file-backed snapshot and browser nodes use a separate IndexedDB store. Global
and per-account-pair record and byte limits are hard bounds. Oldest evidence is evicted
deterministically; APIs expose only bounded digest-ordered pages and aggregate counters without DID
labels.

This evidence does not update `CreditRecord`, affect `order_peers_by_quality`, prove relay or
storage service, prevent self-dealing, or provide Sybil resistance. A later finalized DRanking
ledger must define a distinct receipt format and signing domain instead of treating provisional
receipts as finalized trust input.
