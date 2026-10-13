//! The link stage of one inbound connection: frames whose delegation slots are references, the
//! uncharged drop and `Unknown` disclaimer behind a miss, the leases confirmed by `Known`, and
//! the bounds on those control frames. Every wait is on the application inbox's delivery event
//! or on an activity probe.

use std::sync::Arc;
use std::sync::Mutex;

use super::super::default_transport;
use super::super::failed_receive_count;
use super::super::pending_peer;
use super::super::pending_peer_with;
use super::super::pending_peer_with_key;
use super::super::transport_with_measure;
use super::super::ApplicationRecorder;
use super::super::PendingPeer;
use super::super::RecordingMeasure;
use crate::delegation::DelegateeKey;
use crate::delegation::DelegationDigest;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasurementEvent;
use crate::message::LinkControl;
use crate::message::MessagePayload;
use crate::message::PerSlot;
use crate::message::SlotEncoding;
use crate::message::WirePayload;
use crate::swarm::callback::SwarmTransportCallback;
use crate::swarm::transport::dispatched_link_control_for_test;
use crate::swarm::transport::dispatched_link_control_since;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::LINK_CONTROL_IN_FLIGHT_CAPACITY;
use crate::tests::default::dummy_hooks::PausedDispatchGuard;
use crate::tests::delegatee_key_with_ttl;

/// Both slots by reference.
const BOTH_REFERENCED: PerSlot<SlotEncoding> = PerSlot {
    origin: SlotEncoding::Referenced,
    hop: SlotEncoding::Referenced,
};

/// The origin slot inline and the hop slot by reference: a mix only a link can carry.
const HOP_REFERENCED: PerSlot<SlotEncoding> = PerSlot {
    origin: SlotEncoding::Inline,
    hop: SlotEncoding::Referenced,
};

/// The frame bytes of `payload` with its slots travelling as `encoding` says.
fn wire(payload: &MessagePayload, encoding: PerSlot<SlotEncoding>) -> Result<Vec<u8>> {
    Ok(WirePayload::encoded(payload, encoding)?.to_wire()?.to_vec())
}

/// The typed error behind the callback's boxed refusal, if it is one.
fn refusal_error<'a>(refusal: &'a (dyn std::error::Error + 'static)) -> Option<&'a Error> {
    refusal.downcast_ref::<Error>()
}

/// Whether `dispatched` is a lease on `digest` granted to `peer`, whatever is left of it: the
/// law is about which session is confirmed to whom.
fn confirms(dispatched: &(Did, LinkControl), peer: Did, digest: DelegationDigest) -> bool {
    matches!(
        dispatched,
        (to, LinkControl::Known { digest: leased, .. }) if *to == peer && *leased == digest
    )
}

/// Whether `measure` recorded a receive failure charged to `peer`.
fn charged(measure: &RecordingMeasure, peer: Did) -> Result<bool> {
    Ok(measure
        .snapshot_counters()?
        .contains(&(peer, MeasurementEvent::FailedToReceive)))
}

/// Steady state: once a verified frame carried a session inline, later frames that reference
/// it are resolved, verified, and delivered like any other.
#[tokio::test]
async fn test_referenced_session_resolves_after_a_verified_inline_frame() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;

    pending
        .receive(&pending.custom_message_wire(&transport, b"inline")?)
        .await?;
    let steady = pending.custom_payload(&transport, b"referenced")?;
    pending.receive(&wire(&steady, BOTH_REFERENCED)?).await?;
    application.wait_for_inbounds_at_least(2).await;

    assert_eq!(application.inbound_custom_data(), vec![
        b"inline".to_vec(),
        b"referenced".to_vec(),
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (charging, repair): a frame whose origin slot references a session this link does not
/// hold is dropped uncharged, a miss being this end's clock or fault for an honest peer, and the
/// peer is told `Unknown` for that digest alone; nothing is held, so a frame that resolves
/// passes, and only it is delivered.
#[tokio::test]
async fn test_a_missed_origin_reference_is_dropped_uncharged_and_disclaimed() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    // The peer's own session is taught first, so the frame below misses its origin alone.
    pending
        .receive(&pending.custom_message_wire(&transport, b"teach-the-hop")?)
        .await?;
    application.wait_for_inbounds_at_least(1).await;
    let dispatched_before = dispatched_link_control_for_test().len();

    let stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let missed = pending.relayed_custom_payload(&transport, &stranger, b"missed")?;
    pending.receive(&wire(&missed, BOTH_REFERENCED)?).await?;
    assert_eq!(dispatched_link_control_since(dispatched_before), vec![(
        pending.peer,
        LinkControl::Unknown(stranger.delegation().digest()?)
    )]);

    pending
        .receive(&pending.custom_message_wire(&transport, b"passing")?)
        .await?;
    application.wait_for_inbounds_at_least(2).await;
    assert_eq!(application.inbound_custom_data(), vec![
        b"teach-the-hop".to_vec(),
        b"passing".to_vec(),
    ]);
    assert!(!charged(&measure, pending.peer)?);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (charging, repair): a miss on the hop slot alone, beside an inline origin, is dropped
/// uncharged the same way and disclaimed for the hop's digest; the inline origin it carried is
/// neither learned nor confirmed, since the frame never verified.
#[tokio::test]
async fn test_a_missed_hop_reference_is_dropped_uncharged_and_disclaimed() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let hop_digest = pending.session.delegation().digest()?;
    let dispatched_before = dispatched_link_control_for_test().len();

    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let missed = pending.relayed_custom_payload(&transport, &origin, b"hop-missed")?;
    pending.receive(&wire(&missed, HOP_REFERENCED)?).await?;
    assert_eq!(dispatched_link_control_since(dispatched_before), vec![(
        pending.peer,
        LinkControl::Unknown(hop_digest)
    )]);

    pending
        .receive(&pending.custom_message_wire(&transport, b"passing")?)
        .await?;
    application.wait_for_inbounds_at_least(1).await;
    assert_eq!(application.inbound_custom_data(), vec![b"passing".to_vec()]);
    assert!(!charged(&measure, pending.peer)?);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (scope): the receiver's table lives in the callback of one connection generation. A
/// session the peer's previous generation carried inline is unknown to the next generation of
/// the same peer: a reference to it there is a miss, dropped uncharged and disclaimed, never
/// resolved from the old generation's table.
#[tokio::test]
async fn test_receiver_table_does_not_outlive_the_connection_generation() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let peer_key = SecretKey::random();
    let first = pending_peer_with_key(&transport, peer_key.clone()).await?;
    first.admit(&transport).await?;
    first
        .receive(&first.custom_message_wire(&transport, b"inline")?)
        .await?;
    application.wait_for_inbounds_at_least(1).await;
    transport.disconnect(first.peer).await?;

    let next = pending_peer_with_key(&transport, peer_key).await?;
    next.admit(&transport).await?;
    let digest = next.session.delegation().digest()?;
    let dispatched_before = dispatched_link_control_for_test().len();
    let referenced = next.custom_payload(&transport, b"referenced-on-the-next-generation")?;
    next.receive(&wire(&referenced, BOTH_REFERENCED)?).await?;
    assert_eq!(dispatched_link_control_since(dispatched_before), vec![(
        next.peer,
        LinkControl::Unknown(digest)
    )]);

    next.receive(&next.custom_message_wire(&transport, b"inline-again")?)
        .await?;
    application.wait_for_inbounds_at_least(2).await;
    assert_eq!(application.inbound_custom_data(), vec![
        b"inline".to_vec(),
        b"inline-again".to_vec(),
    ]);
    assert!(!charged(&measure, next.peer)?);
    transport.disconnect(next.peer).await?;
    Ok(())
}

/// Law (generation): a control frame judged on a connection generation that is no longer
/// current is refused, never sent on the newer generation of the same peer, whose tables never
/// saw what it would confirm.
#[tokio::test]
async fn test_control_frames_judged_on_a_superseded_generation_are_not_sent() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let peer_key = SecretKey::random();
    let first = pending_peer_with_key(&transport, peer_key.clone()).await?;
    first.admit(&transport).await?;
    transport.disconnect(first.peer).await?;
    let next = pending_peer_with_key(&transport, peer_key).await?;
    next.admit(&transport).await?;
    let dispatched_before = dispatched_link_control_for_test().len();
    let inbounds_before = application.inbounds();

    // The old generation's callback still verifies the frame (nothing is charged) and would
    // confirm its session; the gate then drops the frame as superseded, so it is not delivered.
    let late = first.custom_message_wire(&transport, b"late-on-the-old-generation")?;
    first.receive(&late).await?;

    assert_eq!(dispatched_link_control_for_test().len(), dispatched_before);
    assert!(!charged(&measure, first.peer)?);
    assert_eq!(first.callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(application.inbounds(), inbounds_before);
    transport.disconnect(next.peer).await?;
    Ok(())
}

/// Law (detachment): a frame's delivery, and so its transport lane, is never paced by a
/// control-frame send. With the dummy transport's dispatch gate closed, so that the
/// confirmation's send cannot complete, an inline frame still completes its delivery; a send
/// awaited in the delivery would sit behind the gate instead, and the frame with it. The gate
/// is then opened and the send goes through.
#[tokio::test]
async fn test_control_frames_never_pace_the_read_loop() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let digest = pending.session.delegation().digest()?;
    let dispatched_before = dispatched_link_control_for_test().len();

    let dispatch_gate = PausedDispatchGuard::new();
    pending
        .receive(&pending.custom_message_wire(&transport, b"delivered-anyway")?)
        .await?;
    application.wait_for_inbounds_at_least(1).await;
    let dispatched = dispatched_link_control_since(dispatched_before);
    assert!(matches!(
        dispatched.as_slice(),
        [confirmation] if confirms(confirmation, pending.peer, digest)
    ));
    drop(dispatch_gate);

    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (bound): at most `LINK_CONTROL_IN_FLIGHT_CAPACITY` control sends are in flight to one
/// peer. With the peer's whole budget held by sends in flight, an inline frame's confirmation
/// is refused, not dispatched, while the frame itself is still delivered; once a send ends,
/// the next frame that teaches the session confirms it.
#[tokio::test]
async fn test_control_sends_beyond_the_in_flight_budget_are_refused() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let digest = pending.session.delegation().digest()?;
    transport.outbound_schedulers.handle(pending.peer)?;
    let in_flight = (0..LINK_CONTROL_IN_FLIGHT_CAPACITY)
        .map(|_| {
            transport
                .outbound_schedulers
                .link_control_permit(pending.peer)
                .flatten()
                .ok_or(Error::LinkControlInFlightCapacity(pending.peer))
        })
        .collect::<Result<Vec<_>>>()?;
    let dispatched_before = dispatched_link_control_for_test().len();

    pending
        .receive(&pending.custom_message_wire(&transport, b"beyond-the-budget")?)
        .await?;
    application.wait_for_inbounds_at_least(1).await;
    assert_eq!(dispatched_link_control_for_test().len(), dispatched_before);

    drop(in_flight);
    pending
        .receive(&pending.custom_message_wire(&transport, b"within-the-budget-again")?)
        .await?;
    application.wait_for_inbounds_at_least(2).await;
    let dispatched = dispatched_link_control_since(dispatched_before);
    assert!(matches!(
        dispatched.as_slice(),
        [confirmation] if confirms(confirmation, pending.peer, digest)
    ));
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (link): a callback bound to no handshake is on no link. A referenced frame there is
/// refused as unresolvable, and nothing is disclaimed, since there is no link to repair.
#[tokio::test]
async fn test_referenced_frame_on_an_unbound_callback_is_refused() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    let unbound = SwarmTransportCallback::new(Arc::clone(&transport));
    let dispatched_before = dispatched_link_control_for_test().len();

    let referenced = pending.custom_payload(&transport, b"referenced-off-link")?;
    let refused = unbound
        .on_admitted_message_for_test(
            &pending.peer.to_string(),
            &wire(&referenced, BOTH_REFERENCED)?,
        )
        .await;

    let refusal = refused.expect_err("a reference resolves only on a link");
    assert!(matches!(
        refusal_error(refusal.as_ref()),
        Some(Error::DelegationReferenceUnresolved(digest))
            if *digest == pending.session.delegation().digest()?
    ));
    assert_eq!(dispatched_link_control_for_test().len(), dispatched_before);
    assert_eq!(application.inbounds(), 0);
    Ok(())
}

/// Law (link): a frame from a peer other than the one the callback is bound to is on no link:
/// a reference in it is refused as unresolvable, not dropped as a miss of the bound link, and
/// nothing is disclaimed on the bound link for it.
#[tokio::test]
async fn test_frames_from_a_peer_other_than_the_bound_one_are_off_the_link() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let stranger_did: Did = SecretKey::random().address().into();
    let dispatched_before = dispatched_link_control_for_test().len();

    let referenced = pending.custom_payload(&transport, b"referenced-by-a-stranger")?;
    let refused = pending
        .callback
        .on_admitted_message_for_test(
            &stranger_did.to_string(),
            &wire(&referenced, BOTH_REFERENCED)?,
        )
        .await;
    let refusal = refused.expect_err("a reference resolves only on the bound link");
    assert!(matches!(
        refusal_error(refusal.as_ref()),
        Some(Error::DelegationReferenceUnresolved(digest))
            if *digest == pending.session.delegation().digest()?
    ));
    assert_eq!(dispatched_link_control_for_test().len(), dispatched_before);
    assert_eq!(application.inbounds(), 0);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (learning): a frame teaches the link before it is gated. Before this end admits the
/// handshake, an inline frame is held for admission and still teaches its session, so a
/// referenced frame that follows resolves instead of missing, is held for admission too, and
/// both are delivered by admission itself.
#[tokio::test]
async fn test_a_frame_teaches_the_link_before_it_is_gated() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;

    pending
        .receive(&pending.custom_message_wire(&transport, b"inline-before-admission")?)
        .await?;
    let referenced = pending.custom_payload(&transport, b"referenced-before-admission")?;
    pending
        .receive(&wire(&referenced, BOTH_REFERENCED)?)
        .await?;

    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 2);
    assert_eq!(application.inbounds(), 0);

    pending.admit(&transport).await?;
    application.wait_for_inbounds_at_least(2).await;
    assert_eq!(application.inbound_custom_data(), vec![
        b"inline-before-admission".to_vec(),
        b"referenced-before-admission".to_vec(),
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// A transport charging into a recording measure, a peer admitted on it whose callback reads
/// the injected inbound clock, the application's recorder, and an origin `pending` relays that
/// lives `LIFETIME_MS`.
struct ExpiryScene {
    measure: Arc<RecordingMeasure>,
    transport: Arc<SwarmTransport>,
    application: ApplicationRecorder,
    pending: PendingPeer,
    origin: DelegateeKey,
    now_ms: Arc<Mutex<u128>>,
}

impl ExpiryScene {
    /// The origin's lifetime: it outlasts the test under the system clock.
    const LIFETIME_MS: u64 = 60_000;

    /// The scene, after one frame from the origin was delivered while it lived.
    async fn new() -> Result<Self> {
        let measure = Arc::new(RecordingMeasure::default());
        let transport = Arc::new(transport_with_measure(measure.clone())?);
        let origin = delegatee_key_with_ttl(Self::LIFETIME_MS)?;
        let now_ms = Arc::new(Mutex::new(crate::utils::get_epoch_ms()));
        let application = ApplicationRecorder::attach(&transport)?;
        let pending =
            pending_peer_with(&transport, SecretKey::random(), Some(Arc::clone(&now_ms))).await?;
        pending.admit(&transport).await?;
        let live =
            pending.relayed_custom_payload(&transport, &origin, b"while-the-origin-lives")?;
        pending.receive(live.to_wire()?.as_ref()).await?;
        application.wait_for_inbounds_at_least(1).await;
        Ok(Self {
            measure,
            transport,
            application,
            pending,
            origin,
            now_ms,
        })
    }

    /// Set the inbound clock just past the origin's expiry, within the grace, and return a
    /// frame of `data` from the origin.
    fn just_expired(&self, data: &[u8]) -> Result<MessagePayload> {
        *self.now_ms.lock().map_err(|_| Error::LockPoisoned)? =
            self.origin.delegation().expires_at_ms() + 1;
        self.pending
            .relayed_custom_payload(&self.transport, &self.origin, data)
    }

    /// Deliver `payload` from the peer, whatever the callback says of it.
    async fn deliver(&self, payload: &MessagePayload) -> Result<()> {
        let _verdict = self
            .pending
            .callback
            .on_admitted_message_for_test(&self.pending.peer.to_string(), &payload.to_wire()?)
            .await;
        Ok(())
    }
}

/// Law (expiry in flight): an origin delegation that verified on one frame, and expired less
/// than `G` before the next frame carrying it arrives, under a valid hop proof, expired in
/// flight, beyond the hop's control: that frame is dropped without an error, not delivered,
/// and not charged to the hop. The inbound clock is injected, so advancing it past the
/// delegation's end is the whole event.
#[tokio::test]
async fn test_an_origin_expired_in_flight_is_dropped_uncharged() -> Result<()> {
    let scene = ExpiryScene::new().await?;
    let pending = &scene.pending;

    let late = scene.just_expired(b"expired-in-flight")?;
    pending
        .callback
        .on_admitted_message_for_test(&pending.peer.to_string(), late.to_wire()?.as_ref())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    pending
        .receive(&pending.custom_message_wire(&scene.transport, b"passing")?)
        .await?;
    scene.application.wait_for_inbounds_at_least(2).await;
    assert_eq!(scene.application.inbound_custom_data(), vec![
        b"while-the-origin-lives".to_vec(),
        b"passing".to_vec(),
    ]);
    assert!(!charged(&scene.measure, pending.peer)?);
    scene.transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (expiry in flight, soundness): the waiver covers only a frame whose sole defect is its
/// origin's expiry in flight. With the origin expired within `G` but the hop's proof invalid,
/// a tampered hop signature or a hop delegation that did not sign, the frame is charged to
/// the hop like any forgery.
#[tokio::test]
async fn test_an_origin_expired_in_flight_under_an_invalid_hop_proof_is_charged() -> Result<()> {
    let scene = ExpiryScene::new().await?;
    let pending = &scene.pending;

    let mut tampered = scene.just_expired(b"tampered-hop-signature")?;
    if let Some(byte) = tampered.verification.sig.first_mut() {
        *byte ^= 1;
    }
    scene.deliver(&tampered).await?;
    assert!(charged(&scene.measure, pending.peer)?);
    let charges = failed_receive_count(&scene.measure, pending.peer)?;

    let mut garbage = scene.just_expired(b"garbage-hop-delegation")?;
    garbage.verification.delegation =
        DelegateeKey::new_with_seckey(&SecretKey::random())?.delegation();
    scene.deliver(&garbage).await?;
    assert_eq!(
        failed_receive_count(&scene.measure, pending.peer)?,
        charges + 1
    );
    assert_eq!(scene.application.inbounds(), 1);
    scene.transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (bounded by its purpose): a link-control frame whose lane holds no credit waits at most
/// `DELIVERY_BOUND`, then is dropped and gives its permit back; the connection is not retired,
/// since a receiver that grants no credit is backpressure, not a dead peer.
#[tokio::test]
async fn test_link_control_without_credit_expires_and_keeps_the_connection() -> Result<()> {
    use crate::swarm::transport::outbound::TransferClass;
    use crate::tests::activity::probe_on_activity;
    use crate::tests::default::TEST_HANG_GUARD;

    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    // Hold every credit the peer has granted on the lane, however much that is.
    let window = transport
        .hold_lane_credit_for_test(pending.peer, TransferClass::DhtControl.channel_lane())?;

    // An inline frame teaches its session; its confirmation now waits for credit, which the
    // held window never grants, so it cannot be sent.
    pending
        .receive(&pending.custom_message_wire(&transport, b"confirmed-without-credit")?)
        .await?;
    application.wait_for_inbounds_at_least(1).await;
    probe_on_activity("the confirmation holds its permit", TEST_HANG_GUARD, || {
        let held = transport
            .outbound_schedulers
            .link_control_in_flight_for_test(pending.peer);
        async move { Ok((held == 1).then_some(())) }
    })
    .await?;
    probe_on_activity("the confirmation expires", TEST_HANG_GUARD, || {
        let held = transport
            .outbound_schedulers
            .link_control_in_flight_for_test(pending.peer);
        async move { Ok((held == 0).then_some(())) }
    })
    .await?;

    assert!(transport.admitted_connection(pending.peer)?.is_some());
    drop(window);
    transport.disconnect(pending.peer).await?;
    Ok(())
}
