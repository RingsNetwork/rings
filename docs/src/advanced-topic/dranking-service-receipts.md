# Provisional DRanking Service Receipts

Rings currently implements one narrow DRanking input: a versioned, countersigned receipt for an
authenticated liveness probe. It is a collection and persistence vertical slice. It is not the
finalized epoch ledger, trust algebra, committee protocol, or routing policy described by the full
DRanking design.

## Probe flow

1. The beneficiary sends `ProbeRequest` with a random nonce and a current five-minute epoch.
2. The provider returns `ProbeOffer`. The offer embeds the exact signed request transaction, an
   exact provider-signed completion transaction, the canonical claim, and the provider's claim
   attestation.
3. The beneficiary verifies the transcript against its one outstanding request, signs the same
   claim under the beneficiary role domain, and returns `ProbeAcknowledgement`.
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
a duplicate; a different receipt for an occupied freshness key is a conflict. Receipt bytes may be
evicted, but their fixed-size replay markers remain independently bounded and durable while the key
can still be admitted. If the global or per-beneficiary marker bound is full, a new key fails closed
instead of evicting an active marker. A monotonic replay floor prevents clock regression from
reopening a pruned epoch.

## Executable model and refinement

Two native Stateright models define the complete state relation for this provisional slice:

- `crates/core/src/message/service_receipt/model.rs` models the request, offer, acknowledgement,
  pending request, three replay gates, four independent delegated-session lifetimes, proof
  lifetimes, epoch, retained evidence, and independent freshness occupancy. Its actions cover
  send/deliver, time advance, loss, duplication, reordering, hard-crash restart, and receipt
  eviction; its admission outcomes cover commit, replay-capacity rejection, and persistence
  failure.
- `crates/measure/src/evidence/model.rs` models admission, exact duplicates, freshness conflicts,
  pair-local and global pressure, deterministic eviction, invalid record rejection, pagination,
  replay-marker saturation, clock regression, and hard-crash restart under explicit record, byte,
  and marker bounds.

The checked safety properties are: admitted evidence was live at its observation instant; only a
complete transcript can be admitted; no admitted digest is admitted twice; receipt and replay-state
bounds always hold; eviction cannot remove duplicate protection for a still-admissible key; restart
preserves every committed marker; capacity and regressed-clock paths fail closed; and provisional
evidence cannot change credit or routing inputs. Reachability properties witness the live happy path
and every admission outcome. No delivery fairness is assumed, so the happy path is reachable rather
than inevitable under loss. Production-refinement checks replay the model states through real
session signatures and through `ProvisionalEvidenceStore`; the shared native/Wasm tests cover
canonical bytes, claim-field binding, transaction binding, and the live-session boundary.

The finite model bounds time, message multiplicity, candidates, and one crash restart. Cryptographic
unforgeability remains an explicit assumption: model checking does not replace verification of the
real signing and canonical-encoding implementations, which is why those are separate refinement
tests. Delivery actions take prior acceptance of each outer transport envelope as a precondition;
the model still checks the embedded request and completion independently from the two claim-role
attestations.

Evidence admission is synchronous with the separate evidence snapshot: storage failure rolls the
transition back, and success means the receipt plus replay marker survive a process crash. Deleting
or replacing that configured store explicitly starts a new collector history and is outside the
crash model. The durable storage write is the admission linearization point; evidence queries and
other admissions share its serialization lock. Cancellation of an incomplete call may
conservatively retain the marker, but cannot admit the key twice.

## Storage and non-claims

The node keeps provisional evidence in a component separate from local peer measurements. Native
nodes use a separate file-backed snapshot and browser nodes use a separate IndexedDB store. Global
and per-account-pair record and byte limits are hard bounds. Oldest evidence is evicted
deterministically without removing its active replay marker. Replay markers have separate hard
global and per-beneficiary bounds; saturation rejects new keys. APIs expose only bounded
digest-ordered pages and aggregate counters without DID labels.

Opening the browser evidence store is part of provider construction. If IndexedDB cannot be
opened, construction fails instead of substituting volatile memory, because a successful
memory-only admission would not refine the documented process-crash model.
Likewise, embedding constructors that do not supply a durable evidence backend keep probe
liveness available but fail closed rather than returning a successful receipt admission.

This evidence does not update `CreditRecord`, affect `order_peers_by_quality`, prove relay or
storage service, prevent self-dealing, or provide Sybil resistance. A later finalized DRanking
ledger must define a distinct receipt format and signing domain instead of treating provisional
receipts as finalized trust input.
