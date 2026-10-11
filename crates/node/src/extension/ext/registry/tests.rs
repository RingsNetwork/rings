use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use super::super::Ctx;
use super::super::Envelope;
use super::super::Interpret;
use super::super::Protocol;
use super::super::Reject;
use super::super::Transition;
use super::super::Wire;
use super::AsyncMutex;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::registry::DynHandler;
use crate::extension::ext::registry::Runner;
use crate::extension::ext::EffectScope;
use crate::extension::ext::Scope;
use crate::extension::protocols::relay::ControlSendTestHook;
use crate::extension::protocols::relay::NativeRelay;
use crate::extension::protocols::relay::Relay;
use crate::extension::protocols::relay::RelayCommand;
use crate::extension::protocols::relay::RelayEffect;
use crate::extension::protocols::relay::RelayEvent;
use crate::extension::protocols::relay::TCP;
use crate::extension::transport::engine::TransportSessions;
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
use crate::extension::transport::RelaySessionId;
use crate::extension::transport::SessionKey;
use crate::extension::Extensions;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;
use crate::sync_lock::lock;

struct OrderedProtocol;

impl Protocol for OrderedProtocol {
    type State = u8;
    type Event = u8;
    type Effect = u8;

    fn namespace(&self) -> &str {
        "ordered-effects"
    }

    fn init(&self) -> Self::State {
        0
    }

    fn decode(&self, wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
        let event = wire
            .payload
            .first()
            .copied()
            .ok_or_else(|| Reject("missing effect value".to_string()))?;
        Ok(event)
    }

    fn step(
        &self,
        ctx: Ctx<'_, Self::State>,
        event: Self::Event,
    ) -> Transition<Self::State, Self::Effect> {
        Transition::with(ctx.state.saturating_add(1), vec![event])
    }
}

#[derive(Default)]
struct BlockingOrderedInterpreter {
    first_effect_started: Notify,
    release_first_effect: Notify,
    observed: Mutex<Vec<u8>>,
}

#[async_trait]
impl Interpret for Arc<BlockingOrderedInterpreter> {
    type Effect = u8;

    async fn run(&self, _scope: &EffectScope, effect: Self::Effect) -> Result<Vec<Bytes>> {
        if effect == 1 {
            self.first_effect_started.notify_one();
            self.release_first_effect.notified().await;
        }
        lock(&self.observed)?.push(effect);
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct RelayFeedbackInterpreter {
    first_effect_started: Notify,
    release_first_effect: Notify,
    first_connect_seen: Mutex<bool>,
    observed_connects: Mutex<Vec<RelaySessionId>>,
}

#[async_trait]
impl Interpret for Arc<RelayFeedbackInterpreter> {
    type Effect = RelayEffect<SocketAddr>;

    async fn run(&self, _scope: &EffectScope, effect: Self::Effect) -> Result<Vec<Bytes>> {
        match effect {
            RelayEffect::Connect { key, .. } => {
                let first_connect = {
                    let mut seen = lock(&self.first_connect_seen)?;
                    let first_connect = !*seen;
                    *seen = true;
                    first_connect
                };
                if first_connect {
                    self.first_effect_started.notify_one();
                    self.release_first_effect.notified().await;
                    let feedback = RelayCommand::<SocketAddr>::Untrack {
                        peer: key.peer,
                        session: key.session,
                        initiator: key.initiator,
                    };
                    return rings_codec::serialize(&feedback)
                        .map(Bytes::from)
                        .map(|payload| vec![payload])
                        .map_err(|_| Error::EncodeError);
                }
                lock(&self.observed_connects)?.push(key.session);
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }
}

#[derive(Default)]
struct FailingOrderedInterpreter {
    observed: Mutex<Vec<u8>>,
}

#[async_trait]
impl Interpret for Arc<FailingOrderedInterpreter> {
    type Effect = u8;

    async fn run(&self, _scope: &EffectScope, effect: Self::Effect) -> Result<Vec<Bytes>> {
        lock(&self.observed)?.push(effect);
        if effect == 1 {
            return Err(Error::ExtensionError(
                "intentional effect failure".to_string(),
            ));
        }
        Ok(Vec::new())
    }
}

fn extensions() -> Result<Extensions> {
    let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let config = ProcessorConfig::new(1, String::new(), session, 1);
    let processor = ProcessorBuilder::from_config(&config)?
        .advertise_presence(false)
        .build()?;
    Ok(Extensions::new(Arc::new(processor)))
}

/// A protocol that delegates its admission, or not.
struct DelegatingProtocol {
    /// The declaration.
    delegates: bool,
}

impl Protocol for DelegatingProtocol {
    type State = ();
    type Event = ();
    type Effect = u8;

    fn namespace(&self) -> &str {
        "delegating"
    }

    fn init(&self) -> Self::State {}

    fn decode(&self, _wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
        Ok(())
    }

    fn step(&self, _ctx: Ctx<'_, Self::State>, _event: Self::Event) -> Transition<(), u8> {
        Transition::pure(())
    }

    fn delegates_admission(&self) -> bool {
        self.delegates
    }
}

/// Registration records the declaration with the handler; a replacement that declares
/// nothing withdraws it, so at admission time a namespace delegates exactly while a
/// delegating protocol owns it.
#[tokio::test]
async fn test_registration_installs_and_replacement_withdraws_delegation() -> Result<()> {
    let extensions = extensions()?;
    let wire = Envelope::new("delegating", Bytes::from_static(b"cell")).encode()?;
    let other = Envelope::new("ordered-effects", Bytes::from_static(b"x")).encode()?;
    let interpreter = Arc::new(FailingOrderedInterpreter::default());

    extensions.register(
        DelegatingProtocol { delegates: true },
        Arc::clone(&interpreter),
    )?;
    assert!(extensions.delegates_admission(&wire));
    assert!(!extensions.delegates_admission(&other));

    extensions.replace(DelegatingProtocol { delegates: false }, interpreter)?;
    assert!(!extensions.delegates_admission(&wire));
    Ok(())
}

#[tokio::test]
async fn test_unknown_legacy_namespace_is_a_nonfatal_drop() -> Result<()> {
    let extensions = extensions()?;
    let from = extensions.core().did();

    extensions
        .dispatch(
            from,
            Envelope::new("snark", Bytes::from_static(b"legacy-task")),
        )
        .await?;

    assert!(extensions.core.handler("snark").is_none());
    Ok(())
}

#[tokio::test]
async fn test_committed_transitions_execute_effects_in_commit_order() -> Result<()> {
    // Invariant: while A's effect is blocked, B cannot commit or emit; releasing A
    // produces the unique effect trace [A, B] for the protocol's state-transition order.
    let extensions = extensions()?;
    let interpreter = Arc::new(BlockingOrderedInterpreter::default());
    let gate_wait = Arc::new(Notify::new());
    let gate_contention = Arc::new(Mutex::new(Vec::new()));
    let gate_observer = {
        let gate_wait = Arc::clone(&gate_wait);
        let gate_contention = Arc::clone(&gate_contention);
        Arc::new(move |contended| {
            gate_contention
                .lock()
                .expect("test gate witness lock")
                .push(contended);
            gate_wait.notify_one();
        }) as Arc<dyn Fn(bool) + Send + Sync>
    };
    let committed = Arc::new(Mutex::new(0_u8));
    let commit_observer = {
        let committed = Arc::clone(&committed);
        Arc::new(move || {
            *committed.lock().expect("test commit witness lock") += 1;
        }) as Arc<dyn Fn() + Send + Sync>
    };
    let runner: Arc<DynHandler> = Arc::new(Runner {
        protocol: OrderedProtocol,
        interpret: Arc::clone(&interpreter),
        state: Mutex::new(0),
        transition_gate: AsyncMutex::new(()),
        after_decode_for_test: None,
        after_commit_for_test: Some(commit_observer),
        before_gate_wait_for_test: Some(gate_observer),
    });
    extensions
        .core
        .handlers
        .write()
        .map_err(|_| Error::Lock)?
        .insert("ordered-effects".to_string(), runner);
    let from = extensions.core().did();

    let first_extensions = extensions.clone();
    let first = tokio::spawn(async move {
        first_extensions
            .dispatch(
                from,
                Envelope::new("ordered-effects", Bytes::from_static(&[1])),
            )
            .await
    });
    interpreter.first_effect_started.notified().await;
    gate_wait.notified().await;

    let second_extensions = extensions.clone();
    let second = tokio::spawn(async move {
        second_extensions
            .dispatch(
                from,
                Envelope::new("ordered-effects", Bytes::from_static(&[2])),
            )
            .await
    });
    gate_wait.notified().await;
    assert_eq!(*lock(&gate_contention)?, vec![false, true]);
    assert!(!second.is_finished());
    assert!(lock(&interpreter.observed)?.is_empty());
    assert_eq!(*lock(&committed)?, 1);

    interpreter.release_first_effect.notify_one();
    first
        .await
        .map_err(|error| Error::ExtensionError(error.to_string()))??;
    second
        .await
        .map_err(|error| Error::ExtensionError(error.to_string()))??;
    assert_eq!(*lock(&committed)?, 2);
    assert_eq!(*lock(&interpreter.observed)?, vec![1, 2]);
    Ok(())
}

#[tokio::test]
async fn test_failed_effect_releases_ordered_turn_for_later_transition() -> Result<()> {
    // Law: failure is an outcome of the committed transition, not a leaked gate. The next
    // transition can run after the failed application has ended.
    let extensions = extensions()?;
    let interpreter = Arc::new(FailingOrderedInterpreter::default());
    extensions.register(OrderedProtocol, Arc::clone(&interpreter))?;
    let from = extensions.core().did();

    let failed = extensions
        .dispatch(
            from,
            Envelope::new("ordered-effects", Bytes::from_static(&[1])),
        )
        .await;
    assert!(matches!(failed, Err(Error::ExtensionError(_))));
    extensions
        .dispatch(
            from,
            Envelope::new("ordered-effects", Bytes::from_static(&[2])),
        )
        .await?;

    assert_eq!(*lock(&interpreter.observed)?, vec![1, 2]);
    Ok(())
}

#[tokio::test]
async fn test_returned_feedback_precedes_a_waiting_transition() -> Result<()> {
    // Invariant: the first inbound Open returns a real RelayCommand::Untrack feedback before
    // a duplicate Open may inspect relay state. Therefore the duplicate is accepted and emits
    // its own Connect; the old queue-after-gate implementation dropped it as a live duplicate.
    let extensions = extensions()?;
    let interpreter = Arc::new(RelayFeedbackInterpreter::default());
    let decoded = Arc::new(Notify::new());
    let observer = {
        let decoded = Arc::clone(&decoded);
        Arc::new(move || decoded.notify_one()) as Arc<dyn Fn() + Send + Sync>
    };
    let protocol = Relay::tcp();
    let initial = protocol.init();
    let state = protocol
        .step(
            Ctx {
                did: extensions.core.did(),
                state: &initial,
            },
            RelayEvent::Command(RelayCommand::RegisterService {
                name: "web".to_string(),
                target: "127.0.0.1:80"
                    .parse::<SocketAddr>()
                    .map_err(|error| Error::ExtensionError(error.to_string()))?,
            }),
        )
        .state;
    let runner: Arc<DynHandler> = Arc::new(Runner {
        protocol,
        interpret: Arc::clone(&interpreter),
        state: Mutex::new(state),
        transition_gate: AsyncMutex::new(()),
        after_decode_for_test: Some(observer),
        after_commit_for_test: None,
        before_gate_wait_for_test: None,
    });
    extensions
        .core
        .handlers
        .write()
        .map_err(|_| Error::Lock)?
        .insert(TCP.to_string(), runner);
    let from: Did = SecretKey::random().address().into();
    let open = rings_codec::serialize(&Frame::Open {
        session: RelaySessionId(0),
        service: "web".to_string(),
    })
    .map(Bytes::from)
    .map_err(|_| Error::EncodeError)?;
    let first_open = open.clone();

    let first_extensions = extensions.clone();
    let first = tokio::spawn(async move {
        first_extensions
            .dispatch(from, Envelope::new(TCP, first_open))
            .await
    });
    interpreter.first_effect_started.notified().await;
    decoded.notified().await;

    let second_extensions = extensions.clone();
    let second = tokio::spawn(async move {
        second_extensions
            .dispatch(from, Envelope::new(TCP, open))
            .await
    });
    decoded.notified().await;
    assert!(!second.is_finished());

    interpreter.release_first_effect.notify_one();
    let timeout = std::time::Duration::from_secs(1);
    tokio::time::timeout(timeout, first)
        .await
        .map_err(|_| Error::ExtensionError("first feedback turn timed out".to_string()))?
        .map_err(|error| Error::ExtensionError(error.to_string()))??;
    tokio::time::timeout(timeout, second)
        .await
        .map_err(|_| Error::ExtensionError("second feedback turn timed out".to_string()))?
        .map_err(|error| Error::ExtensionError(error.to_string()))??;
    assert_eq!(*lock(&interpreter.observed_connects)?, vec![
        RelaySessionId(0)
    ]);
    Ok(())
}

#[tokio::test]
async fn test_missing_open_accepted_resource_returns_synchronous_untrack() -> Result<()> {
    let extensions = extensions()?;
    let effect_scope = EffectScope::new(Scope::new(extensions.core(), TCP.to_string()));
    let interpreter = NativeRelay::new(Arc::new(TransportSessions::new()));
    let peer: Did = SecretKey::random().address().into();
    let key = SessionKey::new(peer, TCP, RelaySessionId(9), Initiator::Local);

    let feedback = interpreter
        .run(&effect_scope, RelayEffect::OpenAccepted {
            token: 77,
            key: key.clone(),
            service: "missing-pending-resource".to_string(),
        })
        .await?;

    assert_eq!(feedback.len(), 1);
    assert!(matches!(
        rings_codec::deserialize::<RelayCommand<SocketAddr>>(feedback[0].as_ref()),
        Ok(RelayCommand::Untrack {
            peer: actual_peer,
            session: RelaySessionId(9),
            initiator: Initiator::Local,
        }) if actual_peer == peer
    ));
    Ok(())
}

/// A repeated `Connect` effect is an idempotent engine observation, not a
/// backend failure that may roll back the reducer's committed session.
#[tokio::test]
async fn test_repeated_connect_preserves_reducer_state_without_close_feedback() -> Result<()> {
    let extensions = extensions()?;
    let engine = Arc::new(TransportSessions::new());
    let interpreter = NativeRelay::new(Arc::clone(&engine));
    let effect_scope = EffectScope::new(Scope::new(extensions.core(), TCP.to_string()));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| Error::ExtensionError(format!("bind test listener: {error}")))?;
    let target = listener
        .local_addr()
        .map_err(|error| Error::ExtensionError(format!("read test listener: {error}")))?;
    let relay = Relay::tcp();
    let did = extensions.core().did();
    let peer = Did::from(7_u32);
    let initial = relay.init();
    let registered = relay.step(
        Ctx {
            did,
            state: &initial,
        },
        RelayEvent::Command(RelayCommand::RegisterService {
            name: "web".to_string(),
            target,
        }),
    );
    let opened = relay.step(
        Ctx {
            did,
            state: &registered.state,
        },
        RelayEvent::Frame {
            from: peer,
            frame: Frame::Open {
                session: RelaySessionId(9),
                service: "web".to_string(),
            },
        },
    );
    let (connect, key) = match opened.effects.as_slice() {
        [effect @ RelayEffect::Connect { key, .. }] => (effect.clone(), key.clone()),
        effects => {
            return Err(Error::ExtensionError(format!(
                "expected one Connect effect, got {effects:?}"
            )));
        }
    };

    let first_feedback = interpreter.run(&effect_scope, connect.clone()).await?;
    assert!(first_feedback.is_empty());
    let repeated_feedback = interpreter.run(&effect_scope, connect).await?;

    let mut state = opened.state;
    let mut feedback_effects = Vec::new();
    for payload in repeated_feedback {
        let event = relay
            .decode(Wire {
                from: did,
                me: did,
                payload: payload.as_ref(),
            })
            .map_err(|Reject(why)| Error::ExtensionError(why))?;
        let transition = relay.step(Ctx { did, state: &state }, event);
        state = transition.state;
        feedback_effects.extend(transition.effects);
    }
    assert!(
        feedback_effects
            .iter()
            .all(|effect| !matches!(effect, RelayEffect::SendClose { .. })),
        "an occupied engine slot must not reduce to a peer Close"
    );

    let duplicate_open = relay.step(Ctx { did, state: &state }, RelayEvent::Frame {
        from: peer,
        frame: Frame::Open {
            session: RelaySessionId(9),
            service: "web".to_string(),
        },
    });
    assert!(
        duplicate_open.effects.is_empty(),
        "the reducer must still track the original session"
    );
    engine.close_for_effect(&key);
    Ok(())
}

#[tokio::test]
async fn test_terminal_relay_control_effect_does_not_await_overlay_send() -> Result<()> {
    let extensions = extensions()?;
    let hook = Arc::new(ControlSendTestHook::default());
    let interpreter = Arc::new(NativeRelay::new_with_control_send_test_hook(
        Arc::new(TransportSessions::new()),
        Arc::clone(&hook),
    ));
    let peer: Did = SecretKey::random().address().into();
    let core = extensions.core();
    let application = tokio::spawn(async move {
        let effect_scope = EffectScope::new(Scope::new(core, TCP.to_string()));
        interpreter
            .run(&effect_scope, RelayEffect::SendClose {
                to: peer,
                session: RelaySessionId(5),
                from_opener: false,
            })
            .await
    });

    // Await a real outbox-worker suspension before observing completion. The interpreter
    // must already have returned; otherwise this join times out deterministically while the
    // hook remains held.
    tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
        .await
        .map_err(|_| Error::ExtensionError("control outbox did not reach test gate".to_string()))?;
    let applied = tokio::time::timeout(std::time::Duration::from_secs(1), application)
        .await
        .map_err(|_| Error::ExtensionError("terminal control effect held the gate".to_string()))?
        .map_err(|error| Error::ExtensionError(error.to_string()))??;

    assert!(applied.is_empty());
    hook.release();
    Ok(())
}

#[tokio::test]
async fn test_saturated_peer_control_lane_does_not_block_another_peer() -> Result<()> {
    let extensions = extensions()?;
    let hook = Arc::new(ControlSendTestHook::default());
    let interpreter = NativeRelay::new_with_control_send_test_hook(
        Arc::new(TransportSessions::new()),
        Arc::clone(&hook),
    );
    let blocked_peer: Did = SecretKey::random().address().into();
    let independent_peer: Did = SecretKey::random().address().into();
    let effect_scope = EffectScope::new(Scope::new(extensions.core(), TCP.to_string()));

    interpreter
        .run(&effect_scope, RelayEffect::SendClose {
            to: blocked_peer,
            session: RelaySessionId(0),
            from_opener: false,
        })
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(1), hook.wait_until_blocked())
        .await
        .map_err(|_| Error::ExtensionError("first peer control lane did not block".to_string()))?;

    let mut saturated = false;
    for session in 1..=8 {
        let result = interpreter
            .run(&effect_scope, RelayEffect::SendClose {
                to: blocked_peer,
                session: RelaySessionId(session),
                from_opener: false,
            })
            .await;
        if result.is_err() {
            saturated = true;
            break;
        }
    }
    assert!(saturated, "the blocked peer must have a finite lane budget");

    interpreter
        .run(&effect_scope, RelayEffect::SendClose {
            to: independent_peer,
            session: RelaySessionId(9),
            from_opener: false,
        })
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        hook.wait_until_completed(independent_peer),
    )
    .await
    .map_err(|_| {
        Error::ExtensionError("independent peer control lane was blocked".to_string())
    })??;

    hook.release();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        hook.wait_until_completed(blocked_peer),
    )
    .await
    .map_err(|_| Error::ExtensionError("blocked peer lane did not resume".to_string()))??;
    Ok(())
}
