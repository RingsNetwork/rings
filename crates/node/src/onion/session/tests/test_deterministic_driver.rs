//! Deterministic replay and zero-sleep driver tests for [`OnionClientSession`], [`OnionLoopClient`],
//! [`dial::open_with_capabilities`], and the cross-crate [`ManualClock`] / [`SharedRng`] seams.

use std::sync::Arc;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use std::time::Duration;

use bytes::Bytes;
use futures::channel::mpsc;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use rings_core::ecc::SecretKey;
use rings_core::utils::Clock;
use rings_core::utils::ManualClock;
use rings_core::utils::SharedRng;

use super::CLASS;
use super::NOW_MS;
use super::TARGET;
use crate::mail::descriptor::MailRecipient;
use crate::mail::envelope::seal as seal_mail;
use crate::onion::circuit::OnionClientTags;
use crate::onion::circuit::OnionIdleFloor;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::onion::circuit::OnionLink;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::circuit::OnionLoopClient;
use crate::onion::circuit::OnionReply;
use crate::onion::session::client::OnionClientFailure;
use crate::onion::session::client::OnionClientSession;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::onion::session::client::OnionCreditWindow;
use crate::onion::session::client::ONION_SESSION_DEADLINE_MS;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::onion::session::dial;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::onion::session::dial::OnionSessionRequest;
use crate::onion::session::OnionSessionId;
use crate::onion::tests::fixture::fixture_route_hop;
use crate::onion::tests::fixture::fixture_secret;
use crate::onion::tests::fixture::session_loop;
use crate::onion::OnionProcessEpoch;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionServiceName;
use crate::tests::exit_descriptor::ExitDescriptorFixture;

const EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([0x5a; 16]);

fn fixture_route(now_ms: u128) -> OnionRoute {
    let keys = [1_u8, 2, 3, 4].map(|seed| {
        DelegateeKey::new_with_seckey(&fixture_secret(seed)).expect("fixture delegation")
    });
    let hops = session_loop([
        fixture_route_hop(&keys[0], EPOCH),
        fixture_route_hop(&keys[1], EPOCH),
        fixture_route_hop(&keys[2], EPOCH),
        fixture_route_hop(&keys[3], EPOCH),
    ]);
    let exit = ExitDescriptorFixture::offline(&keys[2], EPOCH, now_ms, now_ms + 300_000)
        .expect("fixture exit")
        .sign()
        .expect("signed fixture exit");
    OnionRoute::new(OnionServiceName::tcp(), hops, exit)
}

#[test]
fn test_seeded_rng_and_manual_clock_replay_session_ids_surbs_and_mail_seals() {
    let route = fixture_route(NOW_MS);
    let recipient_key = DelegateeKey::new_with_seckey(&fixture_secret(9)).expect("recipient key");
    let recipient = MailRecipient {
        did: recipient_key.delegator_did(),
        encryption_key: recipient_key.delegatee_public_key(),
    };

    let run_trace = |seed: u64| {
        let clock = ManualClock::new(NOW_MS);
        let shared_clock = Arc::new(clock.clone());
        let rng = SharedRng::seed_from_u64(seed);

        let session_id = OnionSessionId::from_rng(&mut rng.lock_rng());
        let mut session = OnionClientSession::with_clock(Bytes::from_static(TARGET), &clock);
        let open_frame = session
            .data(Bytes::from_static(b"GET / HTTP/1.1\r\n\r\n"))
            .expect("open data frame")
            .encode(CLASS)
            .expect("encodes")
            .to_vec();

        let tags = OnionClientTags::default();
        let link_sender = OnionLinkSender::new_with_rng(OnionIdleFloor::DEFAULT, rng.clone());
        let loops = OnionLoopClient::new_with_capabilities(
            Did::from(99_u32),
            tags,
            link_sender,
            shared_clock,
            rng.clone(),
        );
        let (sink, _replies) = mpsc::channel::<OnionReply>(8);
        let (surbs, _tags, expiry) = loops
            .surbs(&route, CLASS, 4, &sink)
            .expect("deterministic surbs");
        let credit_frame = session
            .credit(surbs)
            .expect("credit frame")
            .encode(CLASS)
            .expect("encodes")
            .to_vec();

        // Advance ManualClock without any wall-clock sleep: session stays live before 2V and
        // fails closed at 2V.
        clock.advance(ONION_SESSION_DEADLINE_MS - 1);
        assert_eq!(session.expire(clock.now_ms()), Ok(()));
        clock.advance(1);
        assert_eq!(
            session.expire(clock.now_ms()),
            Err(OnionClientFailure::Silent)
        );

        // Mail seal determinism under the same SharedRng capability used by MailGateway::deliver.
        let sealed = seal_mail(
            b"Subject: Replay\r\n\r\nDeterministic body",
            &recipient,
            &mut rng.lock_rng(),
        )
        .expect("mail seals");

        (
            session_id,
            open_frame,
            expiry,
            credit_frame,
            sealed.mail_id,
            sealed.elements,
        )
    };

    let first = run_trace(1230);
    let second = run_trace(1230);
    let different = run_trace(1231);

    assert_eq!(
        first, second,
        "identical seed + ManualClock must replay byte-for-byte"
    );
    assert_ne!(
        first, different,
        "different seeds must produce distinct protocol artifacts"
    );
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test(start_paused = true)]
async fn test_session_driver_and_cross_crate_dummy_clock_advance_without_wall_clock_sleeps() {
    use rings_core::dht::delivery::NextHop;
    use rings_core::dummy_simulation::DummySimulationGuard;
    use rings_core::message::HandlerCtx;
    use rings_core::message::Message;
    use rings_core::message::MessagePayload;
    use rings_core::message::MessageSigner;
    use rings_core::message::Principal;
    use rings_core::utils::get_epoch_ms;

    use crate::extension::ext::Scope;
    use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
    use crate::provider::Provider;
    use crate::tests::test_processor_builder;
    use crate::tests::test_processor_config;
    use crate::tests::TEST_NETWORK_ID;

    let initial_ms = 1_700_000_000_000_u128;
    let guard = DummySimulationGuard::enter(1230, initial_ms).expect("guard installs");
    assert_eq!(
        get_epoch_ms(),
        initial_ms,
        "cross-crate dummy clock override must apply"
    );

    let clock = guard.shared_clock();
    let rng = guard.shared_rng();

    // Verify HandlerCtx in rings-node reads the shared ManualClock without wall-clock sleeps.
    let node_sk = SecretKey::random_with_rng(&mut rng.lock_rng());
    let sender_del = DelegateeKey::new_with_seckey(&node_sk).expect("delegation at initial_ms");
    let self_did = sender_del.delegator_did();
    let payload = MessagePayload::new_send_with_sequence(
        Message::custom(b"node-ctx-check").expect("custom msg"),
        MessageSigner::new(&sender_del, TEST_NETWORK_ID),
        NextHop::toward(self_did),
        self_did,
        0,
        None,
    )
    .expect("signed payload");
    let ctx = HandlerCtx::new_with_clock(&payload, Principal::Unbound, clock.as_ref());
    assert_eq!(ctx.now_ms(), initial_ms);

    // Build a client processor + scope wired with the deterministic clock and RNG.
    let config = test_processor_config(&node_sk, 0);
    let processor = Arc::new(
        test_processor_builder(&config)
            .clock(clock.clone())
            .rng(rng.clone())
            .build()
            .expect("processor builds"),
    );
    let provider = Provider::from_processor(Arc::clone(&processor));
    let scope = Scope::new(
        provider.extensions().core().clone(),
        ONION_CIRCUIT_NAMESPACE.to_string(),
    );

    let hop_keys = [1_u8, 2, 3, 4].map(|seed| {
        DelegateeKey::new_with_seckey(&fixture_secret(seed)).expect("fixture delegation")
    });
    let guard_did = hop_keys[0].delegator_did();
    let hops = session_loop([
        fixture_route_hop(&hop_keys[0], EPOCH),
        fixture_route_hop(&hop_keys[1], EPOCH),
        fixture_route_hop(&hop_keys[2], EPOCH),
        fixture_route_hop(&hop_keys[3], EPOCH),
    ]);
    let exit_desc =
        ExitDescriptorFixture::offline(&hop_keys[2], EPOCH, initial_ms, initial_ms + 600_000)
            .expect("exit fixture")
            .sign()
            .expect("signed exit");
    let route = OnionRoute::new(OnionServiceName::tcp(), hops, exit_desc);

    let tags = OnionClientTags::default();
    let link_sender = OnionLinkSender::new_with_rng(OnionIdleFloor::DEFAULT, rng.clone());
    link_sender
        .open(scope.clone(), OnionLink::new(guard_did))
        .expect("guard lane opens");
    let loops = OnionLoopClient::new_with_capabilities(
        processor.did(),
        tags,
        link_sender,
        clock.clone(),
        rng.clone(),
    );

    let request = OnionSessionRequest {
        route,
        symbol: OnionServiceName::tcp(),
        target: OnionProxyTarget::parse_authority("example.com:443").expect("valid target"),
        class: CLASS,
        window: OnionCreditWindow::new(16),
    };

    let driver_clock = clock.clone();
    let open_fut = tokio::spawn(async move {
        dial::open_with_capabilities(loops, scope, request, driver_clock, &rng).await
    });
    tokio::task::yield_now().await;

    // Advance 50 seconds of simulated protocol + Tokio timer time in 0 ms wall-clock time.
    guard
        .advance(Duration::from_secs(50))
        .await
        .expect("advance succeeds");
    let open_res = open_fut.await.expect("task joins");
    assert!(
        open_res.is_err(),
        "unanswered session open must fail deterministically without wall-clock sleeps"
    );
    assert_eq!(ctx.now_ms(), initial_ms + 50_000);
}
