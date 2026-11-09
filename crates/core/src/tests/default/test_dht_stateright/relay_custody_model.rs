//! Stage 7 — finite-state model checking for relay custody across storage-less runs,
//! single-use next-holder correlation tokens, chain replication, and graceful leave handoff
//! (`#1181`, `#1182`, `#1057`).
//!
//! Ring topology (`N = 4`, successor capacity `s = 1`, relay replicas `r = 2`):
//!
//! ```text
//!   h₀ (100, Holds) ──▶ x (200, HoldsNone) ──▶ h₁ (300, Holds) ──▶ h₂ (400, Holds) ──▶ h₀
//! ```
//!
//! Because `s = 1` and `x` declares [`RelayCustody::HoldsNone`], `h₀`'s immediate successor
//! list `[x]` contains no relay-capable peer. For a relay inbox carrier at key
//! `k = inbox_key(h₀) = h₀ + 1` (for destination `d = h₀ ∈ (h₂, h₀]`), the true `r = 2`
//! replica chain while `h₀` is online is `{h₀, h₁}`, and shifts to `{h₁, h₂}` when `h₀`
//! gracefully leaves.
//!
//! State transitions exercised against real [`PeerRing`] / `relay_holder` / `relay_replicas`
//! operations:
//! 1. **Storage-less run discovery (`#1182`)**: `h₀`'s immediate successor is `x` (`HoldsNone`),
//!    so `outstanding_next_relay_holder_query` issues `(x + 1, token)`.
//! 2. **Single-use token correlation (`#1181`, `Law (Asked)`)**: adversarial/stale
//!    `take_next_relay_holder` responses with a wrong `request_id`, unasked key, inconsistent
//!    `origin`, or storage-less candidate `x` are rejected without mutating `next_holder` or
//!    retiring the outstanding `token`.
//! 3. **Valid next-holder resolution (`#1181`, `#1182`)**: a valid `take_next_relay_holder`
//!    response resolves `next_holder = Some(h₁)`, retiring `token` once and making
//!    `remote_relay_chain_neighbours` return `(None, Some(h₁))` across `x`.
//! 4. **Chain replication (`#777`)**: `h₀` forwards the carrier at `k` across `x` to `h₁`
//!    (`ReplicationStep::Forward { to: h₁, bytes: true }`), `h₁` admits it from its previous
//!    member `h₀` (`ReplicationStep::Held`), and `h₀` records `ReplicaAcknowledgement::Held`.
//! 5. **Graceful departure of `h₀` (`#1057`)**:
//!    - `h₀` computes handoff under `relay_segment_for_leave` (`local_holds = false`), shifting
//!      membership of `k` to `{h₁, h₂}`;
//!    - `h₂` checks `is_relay_member_for_handoff(k, h₀)` both *before* and *after* receiving
//!      `Leave` from `h₀` (proving reordering invariance between `SyncEntriesWithSuccessor`
//!      and `Leave`);
//!    - `h₀` deletes its local carrier only after `SyncedEntryAck` from `h₂` confirms the
//!      persisted value (`S2'`).

use std::collections::BTreeSet;

use crate::delegation::DelegateeKey;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::relay_holder::owner_answer;
use crate::dht::relay_holder::relay_carrier_offer;
use crate::dht::relay_holder::relay_members;
use crate::dht::relay_holder::RelayCustody;
use crate::dht::relay_holder::RelayMembers;
use crate::dht::relay_holder::RelayReplication;
use crate::dht::topology::TopologyRemoval;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::ReplicaAcknowledgement;
use crate::dht::ReplicationStep;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::message::RelayHoldDigest;
use crate::storage::MemStorage;
use crate::tests::held_inbox_for;

const H0_POS: u32 = 100;
const X_POS: u32 = 200;
const H1_POS: u32 = 300;
const H2_POS: u32 = 400;

const ADV_WRONG_TOKEN: u8 = 0b0001;
const ADV_UNASKED_KEY: u8 = 0b0010;
const ADV_INCONSISTENT_ORIGIN: u8 = 0b0100;
const ADV_STORAGE_LESS_X: u8 = 0b1000;
const ADV_ALL_MASK: u8 =
    ADV_WRONG_TOKEN | ADV_UNASKED_KEY | ADV_INCONSISTENT_ORIGIN | ADV_STORAGE_LESS_X;

const MODEL_NOW_MS: u128 = 1_700_000_000_000;

fn h0_did() -> Did {
    Did::from(H0_POS)
}

fn x_did() -> Did {
    Did::from(X_POS)
}

fn h1_did() -> Did {
    Did::from(H1_POS)
}

fn h2_did() -> Did {
    Did::from(H2_POS)
}

/// Destination `d = h₀ ∈ (h₂, h₀]`; its relay inbox key `k = d + 1 ∈ (h₀, x]` has `h₀` as its
/// segment owner, `{h₀, h₁}` as its `r = 2` members while `h₀` holds relay mail, and `{h₁, h₂}`
/// when `h₀` leaves.
fn carrier_key() -> Did {
    inbox_key(h0_did())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum NextHolderQueryPhase {
    Idle,
    Asked,
    ResolvedH1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum H1ReplicationPhase {
    NotReplicated,
    ForwardInFlight,
    HeldAtH1,
    AckedAtH0,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum HandoffSyncPhase {
    Idle,
    SyncInFlightToH2,
    AcceptedAtH2AckInFlight,
    AckedAndCleanedAtH0,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum RelayCustodyStep {
    DiscoverStorageLessRun,
    AdversarialWrongToken,
    AdversarialUnaskedKey,
    AdversarialInconsistentOrigin,
    AdversarialStorageLessHolderX,
    ResolveNextHolderToH1,
    SendChainReplicationToH1,
    DeliverChainReplicationAtH1,
    DeliverChainReplicaAckAtH0,
    BeginGracefulLeaveAtH0,
    DeliverLeaveToH2,
    DeliverHandoffSyncToH2,
    DeliverHandoffAckAndDeleteAtH0,
}

const RELAY_CUSTODY_STEPS: [RelayCustodyStep; 13] = [
    RelayCustodyStep::DiscoverStorageLessRun,
    RelayCustodyStep::AdversarialWrongToken,
    RelayCustodyStep::AdversarialUnaskedKey,
    RelayCustodyStep::AdversarialInconsistentOrigin,
    RelayCustodyStep::AdversarialStorageLessHolderX,
    RelayCustodyStep::ResolveNextHolderToH1,
    RelayCustodyStep::SendChainReplicationToH1,
    RelayCustodyStep::DeliverChainReplicationAtH1,
    RelayCustodyStep::DeliverChainReplicaAckAtH0,
    RelayCustodyStep::BeginGracefulLeaveAtH0,
    RelayCustodyStep::DeliverLeaveToH2,
    RelayCustodyStep::DeliverHandoffSyncToH2,
    RelayCustodyStep::DeliverHandoffAckAndDeleteAtH0,
];

const FAIR_PROGRESSION_STEPS: [RelayCustodyStep; 9] = [
    RelayCustodyStep::DiscoverStorageLessRun,
    RelayCustodyStep::ResolveNextHolderToH1,
    RelayCustodyStep::SendChainReplicationToH1,
    RelayCustodyStep::DeliverChainReplicationAtH1,
    RelayCustodyStep::DeliverChainReplicaAckAtH0,
    RelayCustodyStep::BeginGracefulLeaveAtH0,
    RelayCustodyStep::DeliverLeaveToH2,
    RelayCustodyStep::DeliverHandoffSyncToH2,
    RelayCustodyStep::DeliverHandoffAckAndDeleteAtH0,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct RelayCustodyModelState {
    next_holder_query: NextHolderQueryPhase,
    resolved_next_holder: Option<Did>,
    rejected_adversarial_mask: u8,
    h0_holds_carrier: bool,
    h1_replication: H1ReplicationPhase,
    h0_leaving: bool,
    leave_delivered_to_h2: bool,
    handoff_sync: HandoffSyncPhase,
}

struct RelayRings {
    h0: PeerRing,
    x: PeerRing,
    h1: PeerRing,
    h2: PeerRing,
}

impl RelayRings {
    fn new() -> Self {
        let replication = RelayReplication {
            replicas: 2,
            age_gate_ms: 0,
        };
        let (h0_id, x_id, h1_id, h2_id) = (h0_did(), x_did(), h1_did(), h2_did());

        let h0 = PeerRing::new_with_storage(h0_id, 1, Box::new(MemStorage::new()))
            .with_relay_replication(replication)
            .with_relay_storage(Box::new(MemStorage::new()));
        let x = PeerRing::new_with_storage(x_id, 1, Box::new(MemStorage::new()))
            .with_relay_replication(replication);
        let h1 = PeerRing::new_with_storage(h1_id, 1, Box::new(MemStorage::new()))
            .with_relay_replication(replication)
            .with_relay_storage(Box::new(MemStorage::new()));
        let h2 = PeerRing::new_with_storage(h2_id, 1, Box::new(MemStorage::new()))
            .with_relay_replication(replication)
            .with_relay_storage(Box::new(MemStorage::new()));

        // Wire 1-successor topology and predecessors round `h0 -> x -> h1 -> h2 -> h0`.
        h0.admit_connected(x_id, None).unwrap();
        h0.notify(h2_id).unwrap();
        x.admit_connected(h1_id, None).unwrap();
        x.notify(h0_id).unwrap();
        h1.admit_connected(h2_id, None).unwrap();
        h1.notify(x_id).unwrap();
        h2.admit_connected(h0_id, None).unwrap();
        h2.notify(h1_id).unwrap();

        // Record relay custody declarations: only `x` is storage-less (`HoldsNone`).
        for ring in [&h0, &x, &h1, &h2] {
            for (peer, custody) in [
                (h0_id, RelayCustody::Holds),
                (x_id, RelayCustody::HoldsNone),
                (h1_id, RelayCustody::Holds),
                (h2_id, RelayCustody::Holds),
            ] {
                if peer != ring.did {
                    ring.observe_relay_custody(peer, custody).unwrap();
                }
            }
        }
        assert_eq!(h0.relay_custody(), RelayCustody::Holds);
        assert_eq!(x.relay_custody(), RelayCustody::HoldsNone);
        assert_eq!(h1.relay_custody(), RelayCustody::Holds);
        assert_eq!(h2.relay_custody(), RelayCustody::Holds);

        // Settle previous-holder chains (`r(L + 1) = 4` hops around the ring).
        let rings = [&h0, &x, &h1, &h2];
        for _ in 0..2 {
            for (idx, sender) in rings.iter().enumerate() {
                if let Some((head, previous)) = sender.previous_relay_holder_to_pass().unwrap() {
                    let receiver = rings[(idx + 1) % rings.len()];
                    assert_eq!(receiver.did, head);
                    assert!(receiver
                        .accept_previous_relay_holder(sender.did, previous)
                        .unwrap());
                }
            }
        }

        Self { h0, x, h1, h2 }
    }
}

/// Real 4-node [`PeerRing`] fixture `h₀ -> x -> h₁ -> h₂ -> h₀` with `s = 1, r = 2`.
struct RelayRingFixture {
    rings: RelayRings,
    hold_digest: RelayHoldDigest,
    hold_op: EntryOperation,
    carrier_entry: Entry,
}

impl RelayRingFixture {
    fn new() -> Self {
        let rings = RelayRings::new();
        let h0_id = h0_did();
        let signer = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
        let inbox = held_inbox_for(h0_id, &signer)
            .unwrap()
            .try_into_storage_entry()
            .unwrap();
        let hold_op = EntryOperation::Extend(inbox)
            .stamped(MODEL_NOW_MS, h0_id)
            .unwrap();
        let hold_digest = RelayHoldDigest::of(&hold_op).unwrap();
        let carrier_entry = hold_op.entry().clone().try_into_storage_entry().unwrap();

        Self {
            rings,
            hold_digest,
            hold_op,
            carrier_entry,
        }
    }
}

impl RelayCustodyModelState {
    fn initial() -> Self {
        Self {
            next_holder_query: NextHolderQueryPhase::Idle,
            resolved_next_holder: None,
            rejected_adversarial_mask: 0,
            h0_holds_carrier: true,
            h1_replication: H1ReplicationPhase::NotReplicated,
            h0_leaving: false,
            leave_delivered_to_h2: false,
            handoff_sync: HandoffSyncPhase::Idle,
        }
    }

    /// Set of live relay-capable holders currently carrying `k`.
    fn live_holders(&self) -> BTreeSet<Did> {
        let mut holders = BTreeSet::new();
        if self.h0_holds_carrier {
            holders.insert(h0_did());
        }
        if matches!(
            self.h1_replication,
            H1ReplicationPhase::HeldAtH1 | H1ReplicationPhase::AckedAtH0
        ) {
            holders.insert(h1_did());
        }
        if matches!(
            self.handoff_sync,
            HandoffSyncPhase::AcceptedAtH2AckInFlight | HandoffSyncPhase::AckedAndCleanedAtH0
        ) {
            holders.insert(h2_did());
        }
        holders
    }

    /// Terminal quiescent state after `h₀`'s graceful leave handoff completes.
    fn is_terminal_quiescent(&self) -> bool {
        self.next_holder_query == NextHolderQueryPhase::ResolvedH1
            && self.resolved_next_holder == Some(h1_did())
            && !self.h0_holds_carrier
            && self.h1_replication == H1ReplicationPhase::AckedAtH0
            && self.h0_leaving
            && self.leave_delivered_to_h2
            && self.handoff_sync == HandoffSyncPhase::AckedAndCleanedAtH0
            && self.live_holders() == BTreeSet::from([h1_did(), h2_did()])
    }

    fn step(self, step: RelayCustodyStep, fixture: &RelayRingFixture) -> Option<Self> {
        let (h0_id, x_id, h1_id, h2_id) = (h0_did(), x_did(), h1_did(), h2_did());
        let expected_query_key = x_id + Did::from(1u32);
        let k = carrier_key();

        match step {
            RelayCustodyStep::DiscoverStorageLessRun => {
                if self.next_holder_query != NextHolderQueryPhase::Idle {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let (query_key, token) = h0
                    .outstanding_next_relay_holder_query()
                    .unwrap()
                    .expect("storage-less successor x must trigger next-holder query (#1182)");
                assert_eq!(query_key, expected_query_key);
                assert!(h0
                    .asks_next_relay_holder(expected_query_key, token)
                    .unwrap());
                assert_eq!(h0.remote_relay_chain_neighbours().unwrap(), (None, None));
                assert_eq!(
                    fixture.rings.x.remote_relay_chain_neighbours().unwrap(),
                    (None, None)
                );
                Some(Self {
                    next_holder_query: NextHolderQueryPhase::Asked,
                    ..self
                })
            }
            RelayCustodyStep::AdversarialWrongToken => {
                if self.rejected_adversarial_mask & ADV_WRONG_TOKEN != 0 {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let wrong_token = uuid::Uuid::from_u128(0xDEAD_BEEF);
                match self.next_holder_query {
                    NextHolderQueryPhase::Idle => {
                        assert_eq!(
                            h0.take_next_relay_holder(
                                expected_query_key,
                                wrong_token,
                                x_id,
                                Some(h1_id),
                                None,
                            )
                            .unwrap(),
                            None
                        );
                        assert!(!h0.has_resolved_next_relay_holder().unwrap());
                    }
                    NextHolderQueryPhase::Asked => {
                        let (_, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                        assert_eq!(
                            h0.take_next_relay_holder(
                                expected_query_key,
                                wrong_token,
                                x_id,
                                Some(h1_id),
                                None,
                            )
                            .unwrap(),
                            None
                        );
                        assert!(h0
                            .asks_next_relay_holder(expected_query_key, token)
                            .unwrap());
                        assert!(!h0.has_resolved_next_relay_holder().unwrap());
                    }
                    NextHolderQueryPhase::ResolvedH1 => {
                        let (_, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                        assert_eq!(
                            h0.take_next_relay_holder(
                                expected_query_key,
                                token,
                                x_id,
                                Some(h1_id),
                                None,
                            )
                            .unwrap(),
                            None
                        );
                        assert_eq!(
                            h0.take_next_relay_holder(
                                expected_query_key,
                                wrong_token,
                                x_id,
                                Some(h2_id),
                                None,
                            )
                            .unwrap(),
                            None
                        );
                        assert_eq!(
                            h0.remote_relay_chain_neighbours().unwrap(),
                            (None, Some(h1_id))
                        );
                    }
                }
                Some(Self {
                    rejected_adversarial_mask: self.rejected_adversarial_mask | ADV_WRONG_TOKEN,
                    ..self
                })
            }
            RelayCustodyStep::AdversarialUnaskedKey => {
                if self.rejected_adversarial_mask & ADV_UNASKED_KEY != 0 {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let (_, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                let unasked_key = x_id + Did::from(25u32);
                assert_eq!(
                    h0.take_next_relay_holder(unasked_key, token, x_id, Some(h1_id), None)
                        .unwrap(),
                    None
                );
                assert!(h0
                    .asks_next_relay_holder(expected_query_key, token)
                    .unwrap());
                assert!(!h0.has_resolved_next_relay_holder().unwrap());
                Some(Self {
                    rejected_adversarial_mask: self.rejected_adversarial_mask | ADV_UNASKED_KEY,
                    ..self
                })
            }
            RelayCustodyStep::AdversarialInconsistentOrigin => {
                if self.rejected_adversarial_mask & ADV_INCONSISTENT_ORIGIN != 0 {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let (_, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                // `expected_query_key = x + 1` lies outside `(x + 50, h1]`.
                let inconsistent_origin = x_id + Did::from(50u32);
                assert_eq!(
                    h0.take_next_relay_holder(
                        expected_query_key,
                        token,
                        inconsistent_origin,
                        Some(h1_id),
                        None,
                    )
                    .unwrap(),
                    None
                );
                // Token must remain active after inconsistent-origin rejection (#1181).
                assert!(h0
                    .asks_next_relay_holder(expected_query_key, token)
                    .unwrap());
                assert!(!h0.has_resolved_next_relay_holder().unwrap());
                Some(Self {
                    rejected_adversarial_mask: self.rejected_adversarial_mask
                        | ADV_INCONSISTENT_ORIGIN,
                    ..self
                })
            }
            RelayCustodyStep::AdversarialStorageLessHolderX => {
                if self.rejected_adversarial_mask & ADV_STORAGE_LESS_X != 0 {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let (_, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                // Even if `x` claims itself as holder for `x + 1`, `h0` knows `x` is `HoldsNone`
                // and refuses to resolve `next_holder` to `x`.
                let _ = h0
                    .take_next_relay_holder(expected_query_key, token, x_id, Some(x_id), None)
                    .unwrap();
                assert!(!h0.has_resolved_next_relay_holder().unwrap());
                assert_eq!(h0.remote_relay_chain_neighbours().unwrap(), (None, None));
                Some(Self {
                    rejected_adversarial_mask: self.rejected_adversarial_mask | ADV_STORAGE_LESS_X,
                    ..self
                })
            }
            RelayCustodyStep::ResolveNextHolderToH1 => {
                if self.next_holder_query != NextHolderQueryPhase::Asked {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let (query_key, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                assert_eq!(query_key, expected_query_key);
                let x_answer = owner_answer(
                    &fixture.rings.x.relay_segment().unwrap(),
                    x_id,
                    expected_query_key,
                );
                assert_eq!(x_answer, Some(h1_id));
                assert_eq!(
                    h0.take_next_relay_holder(expected_query_key, token, x_id, x_answer, None)
                        .unwrap(),
                    None
                );
                assert!(h0.has_resolved_next_relay_holder().unwrap());
                assert_eq!(h0.outstanding_next_relay_holder_query().unwrap(), None);
                assert_eq!(
                    h0.remote_relay_chain_neighbours().unwrap(),
                    (None, Some(h1_id))
                );
                // Replaying the consumed single-use token must fail (`Law (Asked)`).
                assert!(!h0
                    .asks_next_relay_holder(expected_query_key, token)
                    .unwrap());
                assert_eq!(
                    h0.take_next_relay_holder(expected_query_key, token, x_id, Some(h2_id), None,)
                        .unwrap(),
                    None
                );
                assert_eq!(
                    h0.remote_relay_chain_neighbours().unwrap(),
                    (None, Some(h1_id))
                );
                Some(Self {
                    next_holder_query: NextHolderQueryPhase::ResolvedH1,
                    resolved_next_holder: Some(h1_id),
                    ..self
                })
            }
            RelayCustodyStep::SendChainReplicationToH1 => {
                if self.next_holder_query != NextHolderQueryPhase::ResolvedH1
                    || self.h1_replication != H1ReplicationPhase::NotReplicated
                {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let (query_key, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                h0.take_next_relay_holder(query_key, token, x_id, Some(h1_id), None)
                    .unwrap();
                let step_out = h0
                    .replicate_relay_hold(fixture.hold_digest, fixture.hold_op.clone(), h0_id)
                    .unwrap();
                assert_eq!(step_out, ReplicationStep::Forward {
                    to: h1_id,
                    bytes: true,
                });
                Some(Self {
                    h1_replication: H1ReplicationPhase::ForwardInFlight,
                    ..self
                })
            }
            RelayCustodyStep::DeliverChainReplicationAtH1 => {
                if self.h1_replication != H1ReplicationPhase::ForwardInFlight {
                    return None;
                }
                assert!(fixture.rings.h1.is_previous_relay_member(k, h0_id).unwrap());
                assert!(fixture.rings.h1.admits_relay_copy(k, h0_id).unwrap());
                let step_out = fixture
                    .rings
                    .h1
                    .replicate_relay_hold(fixture.hold_digest, fixture.hold_op.clone(), h0_id)
                    .unwrap();
                assert_eq!(step_out, ReplicationStep::Held);
                Some(Self {
                    h1_replication: H1ReplicationPhase::HeldAtH1,
                    ..self
                })
            }
            RelayCustodyStep::DeliverChainReplicaAckAtH0 => {
                if self.h1_replication != H1ReplicationPhase::HeldAtH1 {
                    return None;
                }
                let h0 = RelayRings::new().h0;
                let (query_key, token) = h0.outstanding_next_relay_holder_query().unwrap().unwrap();
                h0.take_next_relay_holder(query_key, token, x_id, Some(h1_id), None)
                    .unwrap();
                let _ = h0
                    .replicate_relay_hold(fixture.hold_digest, fixture.hold_op.clone(), h0_id)
                    .unwrap();
                assert_eq!(
                    h0.acknowledge_relay_replica(fixture.hold_digest, h1_id)
                        .unwrap(),
                    ReplicaAcknowledgement::Held { upstream: h0_id }
                );
                Some(Self {
                    h1_replication: H1ReplicationPhase::AckedAtH0,
                    ..self
                })
            }
            RelayCustodyStep::BeginGracefulLeaveAtH0 => {
                if self.h1_replication != H1ReplicationPhase::AckedAtH0 || self.h0_leaving {
                    return None;
                }
                let leave_segment = fixture.rings.h0.relay_segment_for_leave().unwrap();
                let offer = relay_carrier_offer(&leave_segment, k, h0_id, None, 2);
                assert_eq!(offer.purpose, StorageSyncPurpose::OwnershipHandoff);
                assert!(offer.purpose.permits_source_cleanup());
                // Under `segment_without(h0)` (`local_holds = false`), membership of `k` shifts
                // from `{h0, h1}` to `{h1, h2}` (#1057).
                assert_eq!(
                    relay_members(
                        &fixture.rings.h1.relay_segment_without(h0_id).unwrap(),
                        k,
                        2
                    ),
                    RelayMembers::Members(vec![h1_id, h2_id])
                );
                assert_eq!(
                    relay_members(
                        &fixture.rings.h2.relay_segment_without(h0_id).unwrap(),
                        k,
                        2
                    ),
                    RelayMembers::Members(vec![h1_id, h2_id])
                );
                Some(Self {
                    h0_leaving: true,
                    handoff_sync: HandoffSyncPhase::SyncInFlightToH2,
                    ..self
                })
            }
            RelayCustodyStep::DeliverLeaveToH2 => {
                if !self.h0_leaving || self.leave_delivered_to_h2 {
                    return None;
                }
                let h2 = RelayRings::new().h2;
                let removal = h2.handle_leave(h0_id, &[x_id], |_| true).unwrap();
                assert_eq!(removal, Some(TopologyRemoval::Referenced));
                assert_eq!(h2.successors().list().unwrap(), vec![x_id]);
                assert!(h2.is_relay_member_for_handoff(k, h0_id).unwrap());
                Some(Self {
                    leave_delivered_to_h2: true,
                    ..self
                })
            }
            RelayCustodyStep::DeliverHandoffSyncToH2 => {
                if self.handoff_sync != HandoffSyncPhase::SyncInFlightToH2 {
                    return None;
                }
                let h2 = RelayRings::new().h2;
                if self.leave_delivered_to_h2 {
                    // `Leave` arrived before `SyncEntriesWithSuccessor`: `h2` already removed `h0`
                    // and promoted `x`, and `is_relay_member_for_handoff(k, h0)` still admits `k`.
                    h2.handle_leave(h0_id, &[x_id], |_| true).unwrap();
                    assert!(h2.is_relay_member_for_handoff(k, h0_id).unwrap());
                } else {
                    // `SyncEntriesWithSuccessor` arrived before `Leave`: `h2` is not yet a member
                    // under its ordinary segment, but `is_relay_member_for_handoff(k, h0)` treats
                    // `h0` as holding no relay mail and admits the handoff (#1057).
                    assert!(!h2.is_relay_member(k).unwrap());
                    assert!(h2.is_relay_member_for_handoff(k, h0_id).unwrap());
                    assert!(h2.relocates_relay_from(h0_id).unwrap());
                }
                Some(Self {
                    handoff_sync: HandoffSyncPhase::AcceptedAtH2AckInFlight,
                    ..self
                })
            }
            RelayCustodyStep::DeliverHandoffAckAndDeleteAtH0 => {
                if self.handoff_sync != HandoffSyncPhase::AcceptedAtH2AckInFlight
                    || !self.h0_holds_carrier
                {
                    return None;
                }
                let ack = SyncedEntryAck::new(k, fixture.carrier_entry.clone());
                assert!(ack.confirms_local_value(&fixture.carrier_entry, MODEL_NOW_MS));
                Some(Self {
                    h0_holds_carrier: false,
                    handoff_sync: HandoffSyncPhase::AckedAndCleanedAtH0,
                    ..self
                })
            }
        }
    }

    fn drive_to_terminal(self, fixture: &RelayRingFixture) -> Self {
        let mut current = self;
        for _ in 0..16 {
            if current.is_terminal_quiescent() {
                return current;
            }
            let mut progressed = false;
            for step in FAIR_PROGRESSION_STEPS {
                if let Some(next) = current.step(step, fixture) {
                    current = next;
                    progressed = true;
                    break;
                }
            }
            if !progressed {
                break;
            }
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage 7 — exhaustive BFS over the 4-node ring `h₀ -> x -> h₁ -> h₂` (`x` storage-less,
    /// `s = 1`, `r = 2`), verifying:
    /// - **Safety (`Law (Asked)`, `#1181`, `#1182`)**: `next_holder` is never set to `x` or by an
    ///   invalid token/key/origin, and resolves exclusively to `Some(h₁)`;
    /// - **Safety (`Custody Conservation`, `#1057`)**: in every reachable state, the number of
    ///   live holders carrying `k` is `>= 1`, never drops to 0 during or after `h₀`'s departure,
    ///   and `h₀` deletes its local copy only after both `h₁` and `h₂` hold `k` (`S2'`);
    /// - **Liveness (`Quiescent Convergence`)**: from every reachable state, fair progression
    ///   reaches the terminal state where `{h₁, h₂}` both hold `k` and `h₀` has completed handoff.
    #[test]
    fn test_relay_custody_model_preserves_asked_law_custody_conservation_and_liveness() {
        let fixture = RelayRingFixture::new();
        let mut seen = BTreeSet::new();
        let mut frontier = vec![RelayCustodyModelState::initial()];
        let mut saw_handoff_before_leave = false;
        let mut saw_leave_before_handoff = false;

        while let Some(state) = frontier.pop() {
            if !seen.insert(state) {
                continue;
            }

            // Safety 1 (`Law (Asked)`): `resolved_next_holder` is never `x` and only ever `None`
            // (before resolution) or `Some(h1)` (after resolution).
            assert_ne!(
                state.resolved_next_holder,
                Some(x_did()),
                "storage-less peer x must never become next_holder: {state:?}"
            );
            match state.next_holder_query {
                NextHolderQueryPhase::Idle | NextHolderQueryPhase::Asked => {
                    assert_eq!(state.resolved_next_holder, None);
                }
                NextHolderQueryPhase::ResolvedH1 => {
                    assert_eq!(state.resolved_next_holder, Some(h1_did()));
                }
            }

            // Safety 2 (`Custody Conservation`): at least one live holder always carries `k`,
            // and during/after `h0`'s departure at least two holders carry `k`.
            let holders = state.live_holders();
            assert!(
                !holders.is_empty(),
                "custody conservation violated (0 holders): {state:?}"
            );
            assert!(
                !holders.contains(&x_did()),
                "storage-less peer x must never hold relay carrier: {state:?}"
            );
            if state.h0_leaving {
                assert!(
                    holders.len() >= 2,
                    "departure must preserve at least r = 2 holders across handoff: {state:?}, holders={holders:?}"
                );
            }

            // Track that BFS covers both orderings between `SyncEntriesWithSuccessor` and `Leave`.
            if state.handoff_sync == HandoffSyncPhase::AcceptedAtH2AckInFlight
                && !state.leave_delivered_to_h2
            {
                saw_handoff_before_leave = true;
            }
            if state.leave_delivered_to_h2
                && state.handoff_sync == HandoffSyncPhase::SyncInFlightToH2
            {
                saw_leave_before_handoff = true;
            }

            for step in RELAY_CUSTODY_STEPS {
                if let Some(next) = state.step(step, &fixture) {
                    // S2': deleting `h0`'s local copy is allowed only when `{h1, h2}` both hold `k`.
                    if state.h0_holds_carrier && !next.h0_holds_carrier {
                        assert_eq!(
                            next.live_holders(),
                            BTreeSet::from([h1_did(), h2_did()]),
                            "h0 deleted carrier before {{h1, h2}} both held it: {state:?} -> {next:?}"
                        );
                    }
                    if !seen.contains(&next) {
                        frontier.push(next);
                    }
                }
            }

            // Liveness (`Quiescent Convergence`): every reachable state can reach the unique
            // terminal quiescent state under fair progression.
            let terminal = state.drive_to_terminal(&fixture);
            assert!(
                terminal.is_terminal_quiescent(),
                "reachable state failed to converge to quiescent terminal state: start={state:?}, end={terminal:?}"
            );
        }

        assert!(
            saw_handoff_before_leave,
            "BFS must cover SyncEntriesWithSuccessor arriving at h2 before Leave"
        );
        assert!(
            saw_leave_before_handoff,
            "BFS must cover Leave arriving at h2 before SyncEntriesWithSuccessor"
        );
        assert!(
            seen.iter()
                .any(|s| s.is_terminal_quiescent() && s.rejected_adversarial_mask == ADV_ALL_MASK),
            "BFS must reach terminal quiescent state after rejecting all adversarial inputs"
        );
    }
}
