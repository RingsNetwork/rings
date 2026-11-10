//! Finite exhaustive Stateright models for Mail, DNS, and Registration lease freshness
//! (`descriptor.rs`, `registration.rs`, `mail/gateway.rs`, `dns_relay/descriptor.rs`) and the
//! one-wait `RegistrationReadiness` latch (`registration/readiness.rs`) under packet loss,
//! restart, and bounded clock skew (`±TS_OFFSET_TOLERANCE_MS`).

use std::collections::BTreeSet;
use std::future::ready;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::oneshot;
use futures::FutureExt;
use rings_core::consts::TS_OFFSET_TOLERANCE_MS;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::message::MessageSigner;
use rings_core::swarm::storage_api::StoreUnconfirmed;
use rings_dns_relay::ProxyProtocol;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;
use stateright::Property;

use super::early_publish_armed;
use super::registry_refresh_bound;
use super::republish_due;
use super::validate_schedule;
use super::RegistrationReadiness;
use super::TimingKeys;
use crate::descriptor::latest_valid_by_did;
use crate::descriptor::registry_lease;
use crate::descriptor::SignedDescriptor;
use crate::dns_relay::ServedPort;
use crate::dns_relay::ServiceDescriptor;
use crate::dns_relay::ServiceDescriptorBody;
use crate::mail::descriptor::MailDescriptor;
use crate::mail::descriptor::MailDescriptorBody;
use crate::mail::gateway::confirmed;
use crate::mail::gateway::MailGatewayError;

// Scaled model milliseconds aligned with production `registry_lease()` (`DEFAULT_TTL_MS = 600_000`)
// and `TS_OFFSET_TOLERANCE_MS` (`3_000`):
// - `Λ = registry_lease() = 600_000 ms` (10 ticks of 60 s)
// - `T = fresh_for_ms = 180_000 ms` (3 ticks)
// - `I = heartbeat_interval = 60_000 ms` (1 tick; `2I = 120_000 < Λ`)
// - `σ = TS_OFFSET_TOLERANCE_MS = 3_000 ms`
const TICK_MS: u128 = 60_000;
const HEARTBEAT_MS: u128 = 60_000;
const FRESH_FOR_MS: u64 = 180_000;
const ORIGIN_MS: u128 = 1_000_000;

const _: () = assert!(TS_OFFSET_TOLERANCE_MS == 3_000 && TICK_MS * 10 == 600_000);

// ===========================================================================
// Part 1: Lease freshness, renewal, first-edge republish, clock skew & Mail 250/451
// ===========================================================================

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LeaseMutant {
    /// Skips the signed lease end `ℓ` check, allowing forged/replayed heartbeats to keep a stopped
    /// registrant listed forever.
    IgnoreSignedLeaseEnd,
    /// Reports SMTP `250` before an owner confirms storing the mail (`StoreQuorum::Any` bypass).
    AcceptUnconfirmedMailAs250,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Skew {
    BehindSigma,
    Aligned,
    AheadSigma,
}

impl Skew {
    const fn apply(self, writer_now_ms: u128) -> u128 {
        match self {
            Self::BehindSigma => writer_now_ms.saturating_sub(TS_OFFSET_TOLERANCE_MS),
            Self::Aligned => writer_now_ms,
            Self::AheadSigma => writer_now_ms.saturating_add(TS_OFFSET_TOLERANCE_MS),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum MailReportOutcome {
    FirstOwnerStored,
    AllOwnersRefused,
    TimedOut,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct LeaseState {
    now_ticks: u8,
    edges: u8,
    armed_early_publish: bool,
    early_publishes_since_heartbeat: u8,
    stopped_at_tick: Option<u8>,
    /// Active lease `(expires_at_ms, fresh_for_ms)` held by publisher.
    held_lease: Option<(u128, u64)>,
    /// Stored descriptors at the DHT owners: `(expires_at_ms, fresh_for_ms, heartbeat_at_ms)`.
    stored: Vec<(u128, u64, u128)>,
    last_mail_smtp_code: Option<u16>,
    mail_250_without_live_or_owner_violated: bool,
    ghost_bound_violated: bool,
    early_publish_bound_violated: bool,
    losses_left: u8,
    restarts_left: u8,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LeaseAction {
    /// Scheduled heartbeat (`delivered = false` models packet loss).
    Heartbeat {
        delivered: bool,
    },
    AdmitEdge,
    TriggerEarlyRepublish,
    Tick,
    StopPublisher,
    RestartPublisher,
    AttemptMail {
        skew: Skew,
        report: MailReportOutcome,
    },
}

#[derive(Clone, Default)]
struct LeaseFreshnessModel {
    mutant: Option<LeaseMutant>,
}

impl LeaseFreshnessModel {
    fn writer_ms(ticks: u8) -> u128 {
        ORIGIN_MS + u128::from(ticks) * TICK_MS
    }

    /// Exact pure mirror of [`SignedDescriptor::descriptor_is_live_at`] (verified against real
    /// signed [`MailDescriptor`] and [`ServiceDescriptor`] below).
    fn is_descriptor_live(
        (expires_at_ms, fresh_for_ms, heartbeat_at_ms): (u128, u64, u128),
        reader_now_ms: u128,
        mutant: Option<LeaseMutant>,
    ) -> bool {
        let lease_ms = registry_lease().as_millis();
        let terms_admissible = expires_at_ms
            <= reader_now_ms
                .saturating_add(lease_ms)
                .saturating_add(TS_OFFSET_TOLERANCE_MS)
            && u128::from(fresh_for_ms) <= lease_ms;
        let heard_until = heartbeat_at_ms
            .saturating_add(u128::from(fresh_for_ms))
            .saturating_add(TS_OFFSET_TOLERANCE_MS);
        let live_until = if mutant == Some(LeaseMutant::IgnoreSignedLeaseEnd) {
            heard_until
        } else {
            expires_at_ms.min(heard_until)
        };
        terms_admissible && reader_now_ms <= live_until
    }

    fn any_live_at(state: &LeaseState, reader_now_ms: u128, mutant: Option<LeaseMutant>) -> bool {
        state
            .stored
            .iter()
            .copied()
            .any(|desc| Self::is_descriptor_live(desc, reader_now_ms, mutant))
    }

    fn publish(state: &mut LeaseState, is_heartbeat: bool, delivered: bool) {
        let now_ms = Self::writer_ms(state.now_ticks);
        let renewal_margin_ms = 2 * HEARTBEAT_MS;
        let lease_ms = registry_lease().as_millis();
        let kept = state
            .held_lease
            .filter(|&(expires_at_ms, _)| now_ms.saturating_add(renewal_margin_ms) < expires_at_ms);
        let (expires_at_ms, fresh_for_ms) = kept.unwrap_or_else(|| {
            let nominal = now_ms.saturating_add(lease_ms);
            let next_exp = state
                .held_lease
                .map_or(nominal, |(prev_exp, _)| nominal.max(prev_exp + 1))
                .min(nominal + TS_OFFSET_TOLERANCE_MS);
            let terms = (next_exp, FRESH_FOR_MS);
            state.held_lease = Some(terms);
            terms
        });

        if is_heartbeat {
            state.armed_early_publish = early_publish_armed(true, usize::from(state.edges));
            state.early_publishes_since_heartbeat = 0;
        } else {
            state.armed_early_publish = early_publish_armed(false, usize::from(state.edges));
            state.early_publishes_since_heartbeat =
                state.early_publishes_since_heartbeat.saturating_add(1);
            if state.early_publishes_since_heartbeat > 1 {
                state.early_publish_bound_violated = true;
            }
        }

        if delivered && state.edges > 0 {
            if let Some(existing) = state
                .stored
                .iter_mut()
                .find(|(exp, fresh, _)| *exp == expires_at_ms && *fresh == fresh_for_ms)
            {
                existing.2 = existing.2.max(now_ms);
            } else {
                state.stored.push((expires_at_ms, fresh_for_ms, now_ms));
            }
        }
    }
}

impl Model for LeaseFreshnessModel {
    type Action = LeaseAction;
    type State = LeaseState;

    fn init_states(&self) -> Vec<Self::State> {
        vec![LeaseState {
            now_ticks: 0,
            edges: 0,
            armed_early_publish: false,
            early_publishes_since_heartbeat: 0,
            stopped_at_tick: None,
            held_lease: None,
            stored: Vec::new(),
            last_mail_smtp_code: None,
            mail_250_without_live_or_owner_violated: false,
            ghost_bound_violated: false,
            early_publish_bound_violated: false,
            losses_left: 1,
            restarts_left: 1,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.now_ticks >= 12 || state.last_mail_smtp_code.is_some() {
            return;
        }
        if state.stopped_at_tick.is_none() {
            actions.push(LeaseAction::Heartbeat { delivered: true });
            if state.losses_left > 0 {
                actions.push(LeaseAction::Heartbeat { delivered: false });
            }
            if state.edges == 0 {
                actions.push(LeaseAction::AdmitEdge);
            }
            if republish_due(state.armed_early_publish, usize::from(state.edges)) {
                actions.push(LeaseAction::TriggerEarlyRepublish);
            }
            if !state.stored.is_empty() {
                actions.push(LeaseAction::StopPublisher);
            }
            if state.restarts_left > 0 && state.held_lease.is_some() {
                actions.push(LeaseAction::RestartPublisher);
            }
        }
        actions.push(LeaseAction::Tick);
        if state.last_mail_smtp_code.is_none() {
            for skew in [Skew::BehindSigma, Skew::Aligned, Skew::AheadSigma] {
                for report in [
                    MailReportOutcome::FirstOwnerStored,
                    MailReportOutcome::AllOwnersRefused,
                    MailReportOutcome::TimedOut,
                ] {
                    actions.push(LeaseAction::AttemptMail { skew, report });
                }
            }
        }
    }

    fn next_state(&self, prev: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = prev.clone();
        match action {
            LeaseAction::Heartbeat { delivered } => {
                if !delivered {
                    state.losses_left = state.losses_left.saturating_sub(1);
                }
                Self::publish(&mut state, true, delivered);
            }
            LeaseAction::AdmitEdge => {
                state.edges = 1;
            }
            LeaseAction::TriggerEarlyRepublish => {
                Self::publish(&mut state, false, true);
            }
            LeaseAction::Tick => {
                state.now_ticks = state.now_ticks.saturating_add(1);
                if self.mutant == Some(LeaseMutant::IgnoreSignedLeaseEnd)
                    && state.stopped_at_tick.is_some()
                {
                    // Simulate an adversarial syncer/writer re-appending old bytes with a fresh
                    // unsigned dot `now_ms` after the registrant stopped!
                    let forged_dot = Self::writer_ms(state.now_ticks);
                    for item in &mut state.stored {
                        item.2 = forged_dot;
                    }
                }
                if let Some(stop_tick) = state.stopped_at_tick {
                    let stop_ms = Self::writer_ms(stop_tick);
                    let lease_ms = registry_lease().as_millis();
                    for skew in [Skew::BehindSigma, Skew::Aligned, Skew::AheadSigma] {
                        let reader_ms = skew.apply(Self::writer_ms(state.now_ticks));
                        // Prop. 10 ghost bound: past `t_s + Λ + σ`, no reader may list the stopped
                        // registrant even if its bytes are re-appended with a forged dot!
                        if reader_ms > stop_ms + lease_ms + TS_OFFSET_TOLERANCE_MS
                            && Self::any_live_at(&state, reader_ms, self.mutant)
                        {
                            state.ghost_bound_violated = true;
                        }
                    }
                }
            }
            LeaseAction::StopPublisher => {
                state.stopped_at_tick = Some(state.now_ticks);
            }
            LeaseAction::RestartPublisher => {
                state.restarts_left = state.restarts_left.saturating_sub(1);
                // Volatile held-lease state and armed trigger clear on process restart.
                state.held_lease = None;
                state.armed_early_publish = false;
                state.early_publishes_since_heartbeat = 0;
            }
            LeaseAction::AttemptMail { skew, report } => {
                let reader_ms = skew.apply(Self::writer_ms(state.now_ticks));
                let has_live_recipient = Self::any_live_at(&state, reader_ms, self.mutant);
                let smtp_code = if !has_live_recipient {
                    451
                } else if self.mutant == Some(LeaseMutant::AcceptUnconfirmedMailAs250) {
                    250
                } else {
                    // Execute the production `mail::gateway::confirmed` combinator!
                    let did = Did::from(1_u32);
                    let verdict = match report {
                        MailReportOutcome::FirstOwnerStored => {
                            confirmed(did, ready(Ok(())), futures::future::pending())
                                .now_or_never()
                                .expect("ready stored report resolves immediately")
                        }
                        MailReportOutcome::AllOwnersRefused => confirmed(
                            did,
                            ready(Err(StoreUnconfirmed::Refused { placement: did })),
                            futures::future::pending(),
                        )
                        .now_or_never()
                        .expect("ready refusal resolves immediately"),
                        MailReportOutcome::TimedOut => {
                            confirmed(did, futures::future::pending(), ready(()))
                                .now_or_never()
                                .expect("ready timeout resolves immediately")
                        }
                    };
                    match verdict {
                        Ok(()) => 250,
                        Err(
                            MailGatewayError::Unstored { .. } | MailGatewayError::Unreported { .. },
                        ) => 451,
                        Err(_) => 451,
                    }
                };
                if smtp_code == 250
                    && (!has_live_recipient || report != MailReportOutcome::FirstOwnerStored)
                {
                    state.mail_250_without_live_or_owner_violated = true;
                }
                state.last_mail_smtp_code = Some(smtp_code);
            }
        }
        (state != *prev).then_some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "stopped registrant never outlives signed lease ghost bound Lambda + sigma",
                |_, state| !state.ghost_bound_violated,
            ),
            Property::<Self>::always(
                "first-edge trigger publishes at most once between heartbeats",
                |_, state| !state.early_publish_bound_violated,
            ),
            Property::<Self>::always(
                "mail gateway returns 250 only with live recipient and confirmed owner store",
                |_, state| !state.mail_250_without_live_or_owner_violated,
            ),
            Property::<Self>::sometimes(
                "early publish after zero-edge bootstrap reaches owners and serves 250 under skew",
                |_, state| {
                    state.early_publishes_since_heartbeat == 1
                        && state.last_mail_smtp_code == Some(250)
                },
            ),
            Property::<Self>::sometimes(
                "restart and single lost heartbeat still preserve reachability within T + sigma",
                |model, state| {
                    state.restarts_left == 0
                        && state.losses_left == 0
                        && Self::any_live_at(state, Self::writer_ms(state.now_ticks), model.mutant)
                },
            ),
        ]
    }
}

// ===========================================================================
// Part 2: `RegistrationReadiness` latch Stateright model (`readiness.rs`)
// ===========================================================================

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ReadinessAction {
    PublishZeroEdges(u8),
    PublishWithEdges(u8),
    ResolvePlacementStored { task: u8, placement: u8 },
    ResolvePlacementRefused { task: u8, placement: u8 },
    RestartProcess,
}

#[derive(Clone, Default)]
struct ReadinessLatchModel;

#[derive(Debug, Default)]
struct ReadinessSummary {
    monotonic_within_process_ok: bool,
    soundness_ok: bool,
    one_wait_supersession_ok: bool,
    all_iff_every_task_ok: bool,
    saw_superseded_lost_wait: bool,
    saw_all_ready: bool,
}

fn replay_readiness(history: &[ReadinessAction]) -> ReadinessSummary {
    let mut readiness = Arc::new(RegistrationReadiness::new(2));
    // Per-task active `(wait_future, [Option<Sender>; 2], [confirmed_ok; 2])`.
    type PendingPublish = (
        std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
        [Option<oneshot::Sender<std::result::Result<(), StoreUnconfirmed>>>; 2],
        [bool; 2],
    );
    let mut pending: [Option<PendingPublish>; 2] = [None, None];
    let mut ever_confirmed_all: [bool; 2] = [false, false];
    let mut was_ready: [bool; 2] = [false, false];

    let mut summary = ReadinessSummary {
        monotonic_within_process_ok: true,
        soundness_ok: true,
        one_wait_supersession_ok: true,
        all_iff_every_task_ok: true,
        ..ReadinessSummary::default()
    };

    for &step in history {
        match step {
            ReadinessAction::PublishZeroEdges(task) => {
                // Production law (#855): a publish that began with zero admitted edges never
                // calls `RegistrationReadiness::wait` and never sets readiness.
                let t = usize::from(task);
                if readiness.is_ready(t) && !ever_confirmed_all[t] {
                    summary.soundness_ok = false;
                }
            }
            ReadinessAction::PublishWithEdges(task) => {
                let t = usize::from(task);
                let (tx0, rx0) = oneshot::channel();
                let (tx1, rx1) = oneshot::channel();
                let map_rx = |rx: oneshot::Receiver<std::result::Result<(), StoreUnconfirmed>>| async move {
                    rx.await.unwrap_or(Err(StoreUnconfirmed::Abandoned))
                };
                let mut new_wait = Box::pin(readiness.wait(t, vec![map_rx(rx0), map_rx(rx1)]));
                // Law (one wait): starting `new_wait` MUST immediately supersede and resolve any
                // older stalled `wait` future for task `t` without raising the latch!
                if let Some((mut old_wait, _old_txs, _)) = pending[t].take() {
                    if (&mut old_wait).now_or_never().is_none() {
                        summary.one_wait_supersession_ok = false;
                    } else {
                        summary.saw_superseded_lost_wait = true;
                    }
                }
                let _ = (&mut new_wait).now_or_never();
                pending[t] = Some((new_wait, [Some(tx0), Some(tx1)], [false, false]));
            }
            ReadinessAction::ResolvePlacementStored { task, placement } => {
                let t = usize::from(task);
                let p = usize::from(placement);
                if let Some((wait_fut, txs, ok_flags)) = &mut pending[t] {
                    if let Some(tx) = txs[p].take() {
                        let _ = tx.send(Ok(()));
                        ok_flags[p] = true;
                        if ok_flags == &[true, true] {
                            ever_confirmed_all[t] = true;
                        }
                    }
                    if wait_fut.as_mut().now_or_never().is_some() {
                        pending[t] = None;
                    }
                }
            }
            ReadinessAction::ResolvePlacementRefused { task, placement } => {
                let t = usize::from(task);
                let p = usize::from(placement);
                if let Some((wait_fut, txs, _)) = &mut pending[t] {
                    if let Some(tx) = txs[p].take() {
                        let _ = tx.send(Err(StoreUnconfirmed::Refused {
                            placement: Did::from(u32::from(placement) + 1),
                        }));
                    }
                    if wait_fut.as_mut().now_or_never().is_some() {
                        pending[t] = None;
                    }
                }
            }
            ReadinessAction::RestartProcess => {
                readiness = Arc::new(RegistrationReadiness::new(2));
                pending = [None, None];
                ever_confirmed_all = [false, false];
                was_ready = [false, false];
            }
        }

        for t in 0..2 {
            let now_ready = readiness.is_ready(t);
            if was_ready[t] && !now_ready {
                summary.monotonic_within_process_ok = false;
            }
            if now_ready != ever_confirmed_all[t] {
                summary.soundness_ok = false;
            }
            was_ready[t] = now_ready;
        }
        let all_resolved = readiness.all().now_or_never().is_some();
        let both_ready = readiness.is_ready(0) && readiness.is_ready(1);
        if all_resolved != both_ready {
            summary.all_iff_every_task_ok = false;
        }
        if all_resolved {
            summary.saw_all_ready = true;
        }
    }

    summary
}

impl Model for ReadinessLatchModel {
    type Action = ReadinessAction;
    type State = Vec<ReadinessAction>;

    fn init_states(&self) -> Vec<Self::State> {
        vec![Vec::new(), vec![
            ReadinessAction::PublishWithEdges(0),
            ReadinessAction::ResolvePlacementStored {
                task: 0,
                placement: 0,
            },
            ReadinessAction::ResolvePlacementStored {
                task: 0,
                placement: 1,
            },
        ]]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        let seeded_ready = state.len() >= 3
            && state[..3]
                == [
                    ReadinessAction::PublishWithEdges(0),
                    ReadinessAction::ResolvePlacementStored {
                        task: 0,
                        placement: 0,
                    },
                    ReadinessAction::ResolvePlacementStored {
                        task: 0,
                        placement: 1,
                    },
                ];
        let limit = if seeded_ready { 6 } else { 4 };
        if state.len() >= limit {
            return;
        }
        actions.extend([
            ReadinessAction::PublishZeroEdges(0),
            ReadinessAction::PublishWithEdges(0),
            ReadinessAction::PublishWithEdges(1),
            ReadinessAction::ResolvePlacementStored {
                task: 0,
                placement: 0,
            },
            ReadinessAction::ResolvePlacementStored {
                task: 1,
                placement: 0,
            },
            ReadinessAction::ResolvePlacementStored {
                task: 1,
                placement: 1,
            },
            ReadinessAction::ResolvePlacementRefused {
                task: 0,
                placement: 1,
            },
        ]);
        if !state.contains(&ReadinessAction::RestartProcess) {
            actions.push(ReadinessAction::RestartProcess);
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = last.clone();
        next.push(action);
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "readiness latch is sound, monotone per process, and enforces the one-wait bound",
                |_, history| {
                    let s = replay_readiness(history);
                    s.monotonic_within_process_ok
                        && s.soundness_ok
                        && s.one_wait_supersession_ok
                        && s.all_iff_every_task_ok
                },
            ),
            Property::<Self>::sometimes(
                "superseding a lost report wait terminates the older wait immediately",
                |_, history| replay_readiness(history).saw_superseded_lost_wait,
            ),
            Property::<Self>::sometimes(
                "all() resolves once every registration task confirms every remote placement",
                |_, history| replay_readiness(history).saw_all_ready,
            ),
        ]
    }
}

fn refuted_lease(model: LeaseFreshnessModel) -> BTreeSet<&'static str> {
    let checker = model.checker().spawn_bfs().join();
    checker
        .model()
        .properties()
        .into_iter()
        .filter(|property| {
            let discovered = checker.discovery(property.name).is_some();
            match property.expectation {
                Expectation::Sometimes => !discovered,
                _ => discovered,
            }
        })
        .map(|property| property.name)
        .collect()
}

/// Exhaustive Stateright model check of Mail, DNS, and Registration lease freshness, schedule
/// validation, and `RegistrationReadiness` under packet loss, restart, and clock skew.
#[test]
fn test_service_lease_freshness_and_readiness_stateright_model() {
    // Schedule validation guard (`I < T <= Lambda` and `I < Lambda / 2`).
    let keys = TimingKeys {
        heartbeat_interval: "hb",
        ttl: "ttl",
        enabled_by: None,
    };
    assert!(validate_schedule(
        &keys,
        Duration::from_millis(HEARTBEAT_MS as u64),
        Duration::from_millis(FRESH_FOR_MS),
    )
    .is_ok());
    assert!(validate_schedule(
        &keys,
        Duration::from_millis(FRESH_FOR_MS),
        Duration::from_millis(FRESH_FOR_MS),
    )
    .is_err());
    assert!(validate_schedule(&keys, registry_refresh_bound(), registry_lease()).is_err());

    LeaseFreshnessModel::default()
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();

    ReadinessLatchModel
        .checker()
        .spawn_bfs()
        .join()
        .assert_properties();
}

/// Mutating the signed lease end bound or accepting unconfirmed mail writes is refuted by the
/// lease/mail Stateright model.
#[test]
fn test_each_service_lease_and_mail_mutant_is_refuted() {
    for (mutant, law) in [
        (
            LeaseMutant::IgnoreSignedLeaseEnd,
            "stopped registrant never outlives signed lease ghost bound Lambda + sigma",
        ),
        (
            LeaseMutant::AcceptUnconfirmedMailAs250,
            "mail gateway returns 250 only with live recipient and confirmed owner store",
        ),
    ] {
        let model = LeaseFreshnessModel {
            mutant: Some(mutant),
        };
        assert!(
            refuted_lease(model).contains(law),
            "{mutant:?} must be refuted by `{law}`"
        );
    }
}

/// Cryptographic differential refinement check: real signed [`MailDescriptor`] (`live_recipient`)
/// and [`ServiceDescriptor`] (`descriptor_is_live_at`, `latest_valid_by_did`) agree across all
/// boundary timestamps, skew offsets, inadmissible lease terms, and superseding renewals with
/// [`LeaseFreshnessModel::is_descriptor_live`].
#[test]
fn test_signed_mail_and_dns_descriptors_refine_lease_freshness_predicates() {
    const NET_ID: u32 = 1;
    let base_ms = rings_core::utils::get_epoch_ms();
    let secret = SecretKey::random();
    let delegatee = DelegateeKey::new_with_seckey_at(
        &secret,
        rings_core::consts::DEFAULT_DELEGATION_TTL_MS,
        base_ms.saturating_sub(10 * TICK_MS),
    )
    .expect("test delegation");
    let did = delegatee.delegator_did();
    let pubkey = delegatee
        .delegation()
        .delegator_verification_pubkey()
        .expect("verification pubkey");
    let signer = MessageSigner::new(&delegatee, NET_ID);

    let lease_ms = registry_lease().as_millis();
    for &(expires_at_ms, fresh_for_ms, heartbeat_at_ms) in &[
        (base_ms + lease_ms, FRESH_FOR_MS, base_ms),
        (base_ms + 2 * TICK_MS, FRESH_FOR_MS, base_ms),
        // Inadmissible freshness window (`T > Lambda`).
        (base_ms + lease_ms, (lease_ms as u64) + 1, base_ms),
        // Inadmissible lease end (`l > reader + Lambda + sigma` at `base_ms`).
        (
            base_ms + lease_ms + TS_OFFSET_TOLERANCE_MS + 1,
            FRESH_FOR_MS,
            base_ms,
        ),
    ] {
        let mail = MailDescriptor::new_signed(
            MailDescriptorBody {
                did,
                public_key: pubkey.clone(),
                network_id: NET_ID,
                fresh_for_ms,
                expires_at_ms,
            },
            signer,
        )
        .expect("sign mail descriptor")
        .heard_at(heartbeat_at_ms);

        let dns = ServiceDescriptor::new_signed(
            ServiceDescriptorBody {
                did,
                public_key: pubkey.clone(),
                network_id: NET_ID,
                ports: vec![ServedPort {
                    port: 443,
                    proxy_protocol: ProxyProtocol::Off,
                }],
                txt: Vec::new(),
                fresh_for_ms,
                expires_at_ms,
            },
            signer,
        )
        .expect("sign dns service descriptor")
        .heard_at(heartbeat_at_ms);

        for offset_ticks in 0..=12_u128 {
            for skew in [Skew::BehindSigma, Skew::Aligned, Skew::AheadSigma] {
                let reader_now = skew.apply(base_ms + offset_ticks * TICK_MS);
                let expected = LeaseFreshnessModel::is_descriptor_live(
                    (expires_at_ms, fresh_for_ms, heartbeat_at_ms),
                    reader_now,
                    None,
                );
                assert_eq!(
                    dns.descriptor_is_live_at(reader_now, NET_ID),
                    expected,
                    "ServiceDescriptor liveness drifted at reader_now={reader_now}"
                );
                assert_eq!(
                    MailDescriptor::live_recipient([mail.clone()], did, reader_now, NET_ID,)
                        .is_some(),
                    expected,
                    "MailDescriptor::live_recipient drifted at reader_now={reader_now}"
                );
            }
        }
    }

    // Verify signed lease supersession: a renewed lease (`expires_at_ms` larger) outranks a
    // replayed older lease even when the older lease carries a later unsigned `heartbeat_at_ms`.
    let older_replayed = ServiceDescriptor::new_signed(
        ServiceDescriptorBody {
            did,
            public_key: pubkey.clone(),
            network_id: NET_ID,
            ports: Vec::new(),
            txt: Vec::new(),
            fresh_for_ms: FRESH_FOR_MS,
            expires_at_ms: base_ms + 3 * TICK_MS,
        },
        signer,
    )
    .expect("older")
    .heard_at(base_ms + 2 * TICK_MS);

    let newer_lease = ServiceDescriptor::new_signed(
        ServiceDescriptorBody {
            did,
            public_key: pubkey,
            network_id: NET_ID,
            ports: vec![ServedPort {
                port: 80,
                proxy_protocol: ProxyProtocol::Off,
            }],
            txt: Vec::new(),
            fresh_for_ms: FRESH_FOR_MS,
            expires_at_ms: base_ms + 6 * TICK_MS,
        },
        signer,
    )
    .expect("newer")
    .heard_at(base_ms + TICK_MS);

    assert!(newer_lease.supersedes(&older_replayed));
    let selected = latest_valid_by_did(
        [older_replayed, newer_lease.clone()],
        base_ms + 2 * TICK_MS,
        NET_ID,
        false,
    );
    assert_eq!(selected, vec![newer_lease]);
}
