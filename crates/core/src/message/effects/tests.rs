//! Witnesses of the core effect interpreter and of the actor step yield.

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::future::Future;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::sync::atomic::AtomicUsize;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::sync::atomic::Ordering;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::task::Context;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::task::Wake;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use std::task::Waker;

use super::*;
use crate::delegation::DelegateeKey;
use crate::ecc::SecretKey;
use crate::message::types::QueryFor;
use crate::message::MessageSigner;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::callback::SwarmCallback;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::default::prepare_node;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::default::wait_for_connection_state;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;

/// Callback fixture for tests that exercise only interpreter-owned transport effects.
///
/// It intentionally implements no event behavior, ensuring assertions
/// observe request registration, delivery, and cancellation rather than a
/// callback-generated DHT transition.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
struct NoopCallback;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl SwarmCallback for NoopCallback {}

fn did() -> Did {
    SecretKey::random().address().into()
}

fn payload(destination: Did) -> Result<MessagePayload> {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key)?;
    MessagePayload::new_send(
        Message::custom(b"hello")?,
        MessageSigner::new(&delegatee_key, TEST_NETWORK_ID),
        destination,
        destination,
    )
}

fn single_effect<'payload>(
    effect: Result<Option<CoreEffect<'payload>>>,
) -> Result<CoreEffect<'payload>> {
    effect?.ok_or_else(|| Error::InvalidMessage("expected one effect".to_string()))
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
struct WakeCounter(AtomicUsize);

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
#[test]
fn test_core_actor_step_yields_for_exactly_one_poll() {
    let wake_counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake_counter));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(yield_core_actor_step());

    assert_eq!(Future::poll(future.as_mut(), &mut context), Poll::Pending);
    assert_eq!(wake_counter.0.load(Ordering::SeqCst), 1);
    assert_eq!(Future::poll(future.as_mut(), &mut context), Poll::Ready(()));
    assert_eq!(wake_counter.0.load(Ordering::SeqCst), 1);
}

#[test]
fn test_core_actor_steps_marks_only_real_yield_boundaries() {
    assert_eq!(core_actor_steps([1, 2, 3]).collect::<Vec<_>>(), vec![
        (1, true),
        (2, true),
        (3, false),
    ]);
    assert_eq!(core_actor_steps(Vec::<u8>::new()).next(), None);
}

#[test]
fn test_send_report_message_effect_borrows_payload_and_owns_message() -> Result<()> {
    let destination = did();
    let payload = payload(destination)?;
    let effect = CoreEffect::send_report_message(
        &payload,
        Message::NotifyPredecessorSend(NotifyPredecessorSend { did: destination }),
    );

    match effect {
        CoreEffect::SendReportMessage {
            payload: effect_payload,
            msg,
        } => {
            assert!(std::ptr::eq(effect_payload, &payload));
            match *msg {
                Message::NotifyPredecessorSend(notify) => assert_eq!(notify.did, destination),
                msg => {
                    return Err(Error::InvalidMessage(format!(
                        "expected NotifyPredecessorSend, got {msg:?}"
                    )))
                }
            }
        }
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected SendReportMessage, got {effect:?}"
            )))
        }
    }
    Ok(())
}

#[test]
fn test_reset_destination_effect_borrows_payload_and_next_hop() -> Result<()> {
    let destination = did();
    let next_hop = did();
    let payload = payload(destination)?;
    let effect = CoreEffect::reset_destination(&payload, next_hop);

    match effect {
        CoreEffect::ResetDestination {
            payload: effect_payload,
            next_hop: effect_next_hop,
        } => {
            assert!(std::ptr::eq(effect_payload, &payload));
            assert_eq!(effect_next_hop, next_hop);
        }
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected ResetDestination, got {effect:?}"
            )))
        }
    }
    Ok(())
}

#[test]
fn test_storage_repair_due_lowers_to_a_repair_request() -> Result<()> {
    let effect = single_effect(lower_dht_action(&PeerRingAction::StorageRepairDue, |_| {
        false
    }))?;

    match effect {
        CoreEffect::RequestStorageRepair => Ok(()),
        effect => Err(Error::InvalidMessage(format!(
            "expected RequestStorageRepair, got {effect:?}"
        ))),
    }
}

#[test]
fn test_dht_find_successor_for_connect_sends_direct_report() -> Result<()> {
    let next = did();
    let target = did();

    let effect = single_effect(lower_dht_action(
        &PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForConnect(target)),
        |_| true,
    ))?;

    match effect {
        CoreEffect::SendDirectMessage { msg, destination } => match *msg {
            Message::FindSuccessorSend(msg) => {
                assert_eq!(destination, next);
                assert_eq!(msg.did, target);
                assert!(!msg.strict);
                match msg.then {
                    FindSuccessorThen::Report(FindSuccessorReportHandler::Connect) => {}
                    handler => {
                        return Err(Error::InvalidMessage(format!(
                            "expected connect report handler, got {handler:?}"
                        )))
                    }
                }
            }
            msg => {
                return Err(Error::InvalidMessage(format!(
                    "expected FindSuccessorSend, got {msg:?}"
                )))
            }
        },
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected SendDirectMessage FindSuccessorSend, got {effect:?}"
            )))
        }
    }
    Ok(())
}

#[test]
fn test_dht_find_successor_for_connect_to_self_is_noop() -> Result<()> {
    let target = did();

    assert!(lower_dht_action(
        &PeerRingAction::RemoteAction(
            target,
            PeerRingRemoteAction::FindSuccessorForConnect(target),
        ),
        |_| true,
    )?
    .is_none());
    Ok(())
}

/// Proves that lowering a finger lookup preserves its complete range token.
///
/// The test checks the remote hop, lookup position, strictness flag, slot,
/// and UUID after lowering, preventing the effect layer from degrading a
/// range-aware request back into an uncorrelated slot update.
#[test]
fn test_dht_find_successor_for_fix_echoes_range_request() -> Result<()> {
    let next = did();
    let target = did();
    let request = crate::dht::FingerFixRequest::new(11, uuid::Uuid::from_u128(7))
        .ok_or_else(|| Error::InvalidMessage("invalid test finger request".to_owned()))?;

    let effect = single_effect(lower_dht_action(
        &PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessorForFix {
            did: target,
            request,
        }),
        |_| true,
    ))?;

    match effect {
        CoreEffect::SendDirectMessage { msg, destination } => match *msg {
            Message::FindSuccessorSend(msg) => {
                assert_eq!(destination, next);
                assert_eq!(msg.did, target);
                assert!(!msg.strict);
                match msg.then {
                    FindSuccessorThen::Report(FindSuccessorReportHandler::FixFingerTable {
                        request: reported_request,
                    }) => assert_eq!(reported_request, request),
                    handler => {
                        return Err(Error::InvalidMessage(format!(
                            "expected fix-finger report handler, got {handler:?}"
                        )))
                    }
                }
            }
            msg => {
                return Err(Error::InvalidMessage(format!(
                    "expected FindSuccessorSend, got {msg:?}"
                )))
            }
        },
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected SendDirectMessage FindSuccessorSend, got {effect:?}"
            )))
        }
    }
    Ok(())
}

#[test]
fn test_dht_query_successor_list_connects_before_query() -> Result<()> {
    let target = did();

    let effect = single_effect(lower_dht_action(
        &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
        |_| false,
    ))?;

    match effect {
        CoreEffect::ConnectDhtPeer { peer } => {
            assert_eq!(peer, target)
        }
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected ConnectDhtPeer, got {effect:?}"
            )))
        }
    }
    Ok(())
}

/// Verifies that lowering a successor-list query for an admitted peer emits
/// one correlated send effect addressed to that exact peer.
#[test]
fn test_dht_query_successor_list_sends_when_connected() -> Result<()> {
    let target = did();

    let effect = single_effect(lower_dht_action(
        &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::QueryForSuccessorList),
        |_| true,
    ))?;

    match effect {
        CoreEffect::SendSuccessorQuery { query, destination } => {
            assert_eq!(destination, target);
            assert_eq!(query.did, target);
            match query.then {
                QueryFor::SyncSuccessor => {}
                then => {
                    return Err(Error::InvalidMessage(format!(
                        "expected SyncSuccessor query, got {then:?}"
                    )))
                }
            }
        }
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected SendSuccessorQuery, got {effect:?}"
            )))
        }
    }
    Ok(())
}

/// Proves that successor-sync authority is installed before delivery and
/// removed when delivery fails.
///
/// A successful send leaves the exact reporter/token pair claimable. A send
/// to a missing peer returns an error and leaves the same pair unclaimable,
/// witnessing both sides of the interpreter's transactional boundary.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_successor_query_effect_registers_before_send_and_cancels_send_failure() -> Result<()>
{
    let first = prepare_node(SecretKey::random()).await;
    let second = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&first.swarm, &second.swarm).await;
    wait_for_connection_state(
        &first,
        second.did(),
        rings_transport::core::transport::WebrtcConnectionState::Connected,
    )
    .await?;
    first.dht().admit_connected(second.did(), None)?;

    let callback: SharedSwarmCallback = Arc::new(NoopCallback);
    let interpreter = CoreEffectInterpreter::new(&first.swarm.transport, &callback);
    let sent = QueryForTopoInfoSend::new_for_sync(second.did());
    // Keep the id before the query is moved into the effect; the assertion
    // below proves the interpreter registered this exact request.
    let sent_request_id = sent.request_id;
    interpreter
        .run(CoreEffect::send_successor_query(sent, second.did()))
        .await?;
    assert!(first
        .dht()
        .claim_successor_sync_report(second.did(), sent_request_id)?
        .is_some());

    let missing = did();
    first.dht().admit_connected(missing, None)?;
    let failed = QueryForTopoInfoSend::new_for_sync(missing);
    // Failed sends must remove the otherwise claimable successor-sync slot.
    let failed_request_id = failed.request_id;
    assert!(interpreter
        .run(CoreEffect::send_successor_query(failed, missing))
        .await
        .is_err());
    assert!(first
        .dht()
        .claim_successor_sync_report(missing, failed_request_id)?
        .is_none());
    Ok(())
}

#[test]
fn test_dht_notify_sends_predecessor_to_target() -> Result<()> {
    let target = did();
    let predecessor = did();

    let effect = single_effect(lower_dht_action(
        &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
        |_| true,
    ))?;

    match effect {
        CoreEffect::SendMessage { msg, destination } => match *msg {
            Message::NotifyPredecessorSend(msg) => {
                assert_eq!(destination, target);
                assert_eq!(msg.did, predecessor);
            }
            msg => {
                return Err(Error::InvalidMessage(format!(
                    "expected NotifyPredecessorSend, got {msg:?}"
                )))
            }
        },
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected SendMessage NotifyPredecessorSend, got {effect:?}"
            )))
        }
    }
    Ok(())
}

#[test]
fn test_dht_notify_connects_target_before_sending() -> Result<()> {
    let target = did();
    let predecessor = did();

    let effect = single_effect(lower_dht_action(
        &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(predecessor)),
        |_| false,
    ))?;

    match effect {
        CoreEffect::ConnectDhtPeer { peer } => {
            assert_eq!(peer, target)
        }
        effect => {
            return Err(Error::InvalidMessage(format!(
                "expected ConnectDhtPeer, got {effect:?}"
            )))
        }
    }
    Ok(())
}

#[test]
fn test_dht_notify_to_self_is_noop() -> Result<()> {
    let target = did();

    assert!(lower_dht_action(
        &PeerRingAction::RemoteAction(target, PeerRingRemoteAction::Notify(target)),
        |_| true,
    )?
    .is_none());
    Ok(())
}
