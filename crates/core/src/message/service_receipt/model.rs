//! Executable finite-state model for the provisional ProbeV1 receipt flow.
//!
//! Model scope and refinement map:
//!
//! - [`Action::SendRequest`] refines `Stabilizer::probe_peer_liveness`.
//! - [`Action::DeliverRequest`] refines `HandleMsg<ProbeRequestV1>`.
//! - [`Action::DeliverOffer`] refines `HandleMsg<ProbeOfferV1>`.
//! - [`Action::DeliverAcknowledgement`] refines
//!   `HandleMsg<ProbeAcknowledgementV1>` and `admit_provisional_receipt`.
//! - [`Action::Tick`], [`Action::Duplicate`], and [`Action::Drop`] model the
//!   relevant finite time and network schedules.
//! - [`Action::HardCrashRestart`] preserves synchronously committed
//!   replay/evidence and clears volatile pending/network state.
//! - [`Action::Evict`] models bounded receipt-byte eviction while retaining the
//!   independently bounded freshness marker.
//! - Delivery actions assume the outer transport envelope has already passed
//!   the shared replay and live-verification boundary. The embedded request,
//!   completion, and two role attestations retain independent session windows.
//! - Admission outcomes cover commit, replay-capacity rejection, and durable
//!   persistence failure; failed outcomes commit neither evidence nor a marker.
//!
//! Cryptographic unforgeability is an assumption at this layer. The production
//! refinement test below discharges the temporal predicate over real session
//! and role signatures. Canonical byte/digest and role/transaction binding are
//! witnessed by the shared native/Wasm tests in `tests.rs`.
//! No delivery fairness is assumed: the happy path is reachable, not inevitable
//! under loss. Deleting or replacing the configured evidence store is outside
//! the process-crash model.

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::*;
use crate::ecc::SecretKey;
use crate::session::SessionSk;

const MAX_TIME: u8 = 4;
const MODEL_PROOF_TTL: u8 = 2;
const MODEL_EPOCH_END: u8 = 3;
const INITIAL_CREDIT: u8 = 7;
const INITIAL_ROUTE: u8 = 11;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum WireKind {
    Request,
    Offer,
    Acknowledgement,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum AdmissionMode {
    Commit,
    ReplayCapacityExhausted,
    PersistenceFailure,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProofWindow {
    signed_at: u8,
    expires_at: u8,
}

impl ProofWindow {
    fn signed_at(now: u8) -> Self {
        Self {
            signed_at: now,
            expires_at: now.saturating_add(MODEL_PROOF_TTL).min(MAX_TIME),
        }
    }

    const fn is_live_at(self, now: u8) -> bool {
        self.signed_at <= now && now <= self.expires_at
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProbeState {
    now: u8,
    request_session_end: u8,
    completion_session_end: u8,
    provider_attestation_session_end: u8,
    beneficiary_attestation_session_end: u8,
    admission_mode: AdmissionMode,
    request_issued: bool,
    pending_request: bool,
    request_copies: u8,
    offer_copies: u8,
    acknowledgement_copies: u8,
    request_proof: Option<ProofWindow>,
    completion_proof: Option<ProofWindow>,
    provider_attestation: Option<ProofWindow>,
    beneficiary_attestation: Option<ProofWindow>,
    request_replayed: bool,
    offer_replayed: bool,
    acknowledgement_replayed: bool,
    evidence_observed_at: Option<u8>,
    freshness_occupied: bool,
    admission_count: u8,
    capacity_rejected: bool,
    persistence_failed: bool,
    crash_restarted: bool,
    credit: u8,
    route: u8,
}

impl ProbeState {
    const fn initial(
        request_session_end: u8,
        completion_session_end: u8,
        provider_attestation_session_end: u8,
        beneficiary_attestation_session_end: u8,
        admission_mode: AdmissionMode,
    ) -> Self {
        Self {
            now: 0,
            request_session_end,
            completion_session_end,
            provider_attestation_session_end,
            beneficiary_attestation_session_end,
            admission_mode,
            request_issued: false,
            pending_request: false,
            request_copies: 0,
            offer_copies: 0,
            acknowledgement_copies: 0,
            request_proof: None,
            completion_proof: None,
            provider_attestation: None,
            beneficiary_attestation: None,
            request_replayed: false,
            offer_replayed: false,
            acknowledgement_replayed: false,
            evidence_observed_at: None,
            freshness_occupied: false,
            admission_count: 0,
            capacity_rejected: false,
            persistence_failed: false,
            crash_restarted: false,
            credit: INITIAL_CREDIT,
            route: INITIAL_ROUTE,
        }
    }

    const fn session_is_live_at(session_end: u8, now: u8) -> bool {
        now <= session_end
    }

    const fn epoch_is_live_at(now: u8) -> bool {
        now <= MODEL_EPOCH_END
    }

    fn request_is_live_at(&self, now: u8) -> bool {
        self.request_proof
            .is_some_and(|proof| proof.is_live_at(now))
            && Self::session_is_live_at(self.request_session_end, now)
            && Self::epoch_is_live_at(now)
    }

    fn completion_is_live_at(&self, now: u8) -> bool {
        self.completion_proof
            .is_some_and(|proof| proof.is_live_at(now))
            && Self::session_is_live_at(self.completion_session_end, now)
    }

    fn provider_attestation_is_live_at(&self, now: u8) -> bool {
        self.provider_attestation
            .is_some_and(|proof| proof.is_live_at(now))
            && Self::session_is_live_at(self.provider_attestation_session_end, now)
    }

    fn beneficiary_attestation_is_live_at(&self, now: u8) -> bool {
        self.beneficiary_attestation
            .is_some_and(|proof| proof.is_live_at(now))
            && Self::session_is_live_at(self.beneficiary_attestation_session_end, now)
    }

    fn receipt_is_live_at(&self, now: u8) -> bool {
        self.provider_attestation_is_live_at(now)
            && self.beneficiary_attestation_is_live_at(now)
            && Self::epoch_is_live_at(now)
    }

    const fn copies(&self, kind: WireKind) -> u8 {
        match kind {
            WireKind::Request => self.request_copies,
            WireKind::Offer => self.offer_copies,
            WireKind::Acknowledgement => self.acknowledgement_copies,
        }
    }

    fn set_copies(&mut self, kind: WireKind, copies: u8) {
        match kind {
            WireKind::Request => self.request_copies = copies,
            WireKind::Offer => self.offer_copies = copies,
            WireKind::Acknowledgement => self.acknowledgement_copies = copies,
        }
    }

    fn decrement(&mut self, kind: WireKind) {
        self.set_copies(kind, self.copies(kind).saturating_sub(1));
    }

    fn send_request(&mut self) {
        self.request_issued = true;
        self.pending_request = true;
        self.request_copies = 1;
        self.request_proof = Some(ProofWindow::signed_at(self.now));
    }

    fn deliver_request(&mut self) {
        self.decrement(WireKind::Request);
        if self.request_replayed || !self.request_is_live_at(self.now) {
            return;
        }
        self.request_replayed = true;
        self.completion_proof = Some(ProofWindow::signed_at(self.now));
        self.provider_attestation = Some(ProofWindow::signed_at(self.now));
        self.offer_copies = 1;
    }

    fn deliver_offer(&mut self) {
        self.decrement(WireKind::Offer);
        if self.offer_replayed
            || !self.request_is_live_at(self.now)
            || !self.completion_is_live_at(self.now)
            || !self.provider_attestation_is_live_at(self.now)
        {
            return;
        }
        self.offer_replayed = true;
        if !self.pending_request || !Self::epoch_is_live_at(self.now) {
            return;
        }
        self.pending_request = false;
        self.beneficiary_attestation = Some(ProofWindow::signed_at(self.now));
        self.acknowledgement_copies = 1;
    }

    fn deliver_acknowledgement(&mut self) {
        self.decrement(WireKind::Acknowledgement);
        if self.acknowledgement_replayed || !self.beneficiary_attestation_is_live_at(self.now) {
            return;
        }
        self.acknowledgement_replayed = true;
        if self.freshness_occupied || !self.receipt_is_live_at(self.now) {
            return;
        }
        match self.admission_mode {
            AdmissionMode::ReplayCapacityExhausted => {
                self.capacity_rejected = true;
                return;
            }
            AdmissionMode::PersistenceFailure => {
                self.persistence_failed = true;
                return;
            }
            AdmissionMode::Commit => {}
        }
        self.evidence_observed_at = Some(self.now);
        self.freshness_occupied = true;
        self.admission_count = self.admission_count.saturating_add(1);
    }

    fn duplicate(&mut self, kind: WireKind) {
        self.set_copies(kind, self.copies(kind).saturating_add(1).min(2));
    }

    fn drop_one(&mut self, kind: WireKind) {
        self.decrement(kind);
    }

    fn restart(&mut self) {
        self.crash_restarted = true;
        self.pending_request = false;
        self.request_copies = 0;
        self.offer_copies = 0;
        self.acknowledgement_copies = 0;
    }

    fn evict(&mut self) {
        self.evidence_observed_at = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Action {
    SendRequest,
    DeliverRequest,
    DeliverOffer,
    DeliverAcknowledgement,
    Duplicate(WireKind),
    Drop(WireKind),
    Tick,
    HardCrashRestart,
    Evict,
}

#[derive(Clone)]
struct ProbeModel;

impl Model for ProbeModel {
    type State = ProbeState;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        let mut states = Vec::new();
        for request_end in 0..=MAX_TIME {
            for completion_end in 0..=MAX_TIME {
                for provider_attestation_end in 0..=MAX_TIME {
                    for beneficiary_attestation_end in 0..=MAX_TIME {
                        for admission_mode in [
                            AdmissionMode::Commit,
                            AdmissionMode::ReplayCapacityExhausted,
                            AdmissionMode::PersistenceFailure,
                        ] {
                            states.push(ProbeState::initial(
                                request_end,
                                completion_end,
                                provider_attestation_end,
                                beneficiary_attestation_end,
                                admission_mode,
                            ));
                        }
                    }
                }
            }
        }
        states
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if !state.request_issued {
            actions.push(Action::SendRequest);
        }
        for (kind, deliver) in [
            (WireKind::Request, Action::DeliverRequest),
            (WireKind::Offer, Action::DeliverOffer),
            (WireKind::Acknowledgement, Action::DeliverAcknowledgement),
        ] {
            if state.copies(kind) > 0 {
                actions.push(deliver);
                actions.push(Action::Drop(kind));
            }
            if state.copies(kind) == 1 {
                actions.push(Action::Duplicate(kind));
            }
        }
        if state.now < MAX_TIME {
            actions.push(Action::Tick);
        }
        if !state.crash_restarted {
            actions.push(Action::HardCrashRestart);
        }
        if state.evidence_observed_at.is_some() {
            actions.push(Action::Evict);
        }
    }

    fn next_state(&self, previous: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = previous.clone();
        match action {
            Action::SendRequest => state.send_request(),
            Action::DeliverRequest => state.deliver_request(),
            Action::DeliverOffer => state.deliver_offer(),
            Action::DeliverAcknowledgement => state.deliver_acknowledgement(),
            Action::Duplicate(kind) => state.duplicate(kind),
            Action::Drop(kind) => state.drop_one(kind),
            Action::Tick => state.now = state.now.saturating_add(1),
            Action::HardCrashRestart => state.restart(),
            Action::Evict => state.evict(),
        }
        (state != *previous).then_some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always("admitted evidence was live at observation", |_, state| {
                state
                    .evidence_observed_at
                    .is_none_or(|observed_at| state.receipt_is_live_at(observed_at))
            }),
            Property::<Self>::always("only a complete transcript is admitted", |_, state| {
                state.evidence_observed_at.is_none()
                    || (state.request_replayed
                        && state.offer_replayed
                        && state.acknowledgement_replayed
                        && state.request_proof.is_some()
                        && state.completion_proof.is_some()
                        && state.provider_attestation.is_some()
                        && state.beneficiary_attestation.is_some())
            }),
            Property::<Self>::always(
                "retained evidence always has a replay marker",
                |_, state| state.evidence_observed_at.is_none() || state.freshness_occupied,
            ),
            Property::<Self>::always("one freshness key is admitted at most once", |_, state| {
                state.admission_count <= 1
            }),
            Property::<Self>::always("failed admission commits no evidence state", |_, state| {
                !(state.capacity_rejected || state.persistence_failed)
                    || (state.evidence_observed_at.is_none()
                        && !state.freshness_occupied
                        && state.admission_count == 0)
            }),
            Property::<Self>::always(
                "receipt evidence does not change routing inputs",
                |_, state| state.credit == INITIAL_CREDIT && state.route == INITIAL_ROUTE,
            ),
            Property::<Self>::sometimes("one live transcript is admitted", |_, state| {
                state.evidence_observed_at.is_some()
            }),
            Property::<Self>::sometimes(
                "a hard crash preserves a committed replay marker",
                |_, state| state.crash_restarted && state.freshness_occupied,
            ),
            Property::<Self>::sometimes("replay capacity rejection is reachable", |_, state| {
                state.capacity_rejected
            }),
            Property::<Self>::sometimes("persistence failure is reachable", |_, state| {
                state.persistence_failed
            }),
        ]
    }
}

fn signed_receipt_with_session_ends(
    provider_ttl_ms: u64,
    beneficiary_ttl_ms: u64,
) -> crate::error::Result<ProvisionalServiceReceiptV1> {
    const CREATED_AT_MS: u128 = 1_700_000_000_000;
    const NETWORK_ID: u32 = 7;
    let provider = SessionSk::from_test_keys(
        &SecretKey::random(),
        SecretKey::random(),
        CREATED_AT_MS,
        provider_ttl_ms,
    )?;
    let beneficiary = SessionSk::from_test_keys(
        &SecretKey::random(),
        SecretKey::random(),
        CREATED_AT_MS,
        beneficiary_ttl_ms,
    )?;
    let claim = ProvisionalServiceClaimV1::probe(
        NETWORK_ID,
        provider.account_did(),
        beneficiary.account_did(),
        ProvisionalEpochV1::from_unix_seconds(u64::try_from(CREATED_AT_MS / 1_000).unwrap_or(0)),
        [1; 32],
        [2; 32],
        [3; 32],
    );
    let bytes = claim.canonical_bytes().map_err(crate::error::Error::from)?;
    ProvisionalServiceReceiptV1::new(
        claim,
        MessageSigner::new(&provider, NETWORK_ID).sign_at(
            PROVIDER_DOMAIN,
            &bytes,
            CREATED_AT_MS,
        )?,
        MessageSigner::new(&beneficiary, NETWORK_ID).sign_at(
            BENEFICIARY_DOMAIN,
            &bytes,
            CREATED_AT_MS,
        )?,
    )
    .map_err(crate::error::Error::from)
}

#[test]
fn model_checks_all_bounded_time_and_network_schedules() {
    ProbeModel.checker().spawn_bfs().join().assert_properties();
}

#[test]
fn production_live_admission_refines_the_model_session_predicate() -> crate::error::Result<()> {
    const CREATED_AT_MS: u128 = 1_700_000_000_000;
    const NETWORK_ID: u32 = 7;
    for provider_end in 0..=MAX_TIME {
        for beneficiary_end in 0..=MAX_TIME {
            let receipt = signed_receipt_with_session_ends(
                u64::from(provider_end),
                u64::from(beneficiary_end),
            )?;
            for observed_offset in 0..=MAX_TIME {
                let expected = ProbeState::session_is_live_at(provider_end, observed_offset)
                    && ProbeState::session_is_live_at(beneficiary_end, observed_offset);
                assert_eq!(
                    receipt
                        .verify_live_at(
                            NETWORK_ID,
                            CREATED_AT_MS + u128::from(observed_offset),
                        )
                        .is_ok(),
                    expected,
                    "live-admission refinement failed: provider_end={provider_end}, beneficiary_end={beneficiary_end}, observed_offset={observed_offset}"
                );
            }
        }
    }
    Ok(())
}
