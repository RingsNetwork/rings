use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;

use bytes::Bytes;
use rings_core::dht::Did;

use super::Frame;
use super::Initiator;
use super::Relay;
use super::RelayCommand;
use super::RelayEffect;
use super::RelayState;
use super::SessionId;
use super::SessionKey;
use super::TransportKind;
use super::MAX_RELAY_SESSIONS;
use super::MAX_RELAY_SESSIONS_PER_PEER;
use crate::extension::ext::Ctx;
use crate::extension::ext::Protocol;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;

fn this_node() -> Did {
    Did::from(1u32)
}
fn peer_a() -> Did {
    Did::from(2u32)
}
fn peer_b() -> Did {
    Did::from(3u32)
}
fn web_addr() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}

/// A server-side (peer-opened) key on the TCP relay — the common case in these tests.
fn rkey(peer: Did, session: u64) -> SessionKey {
    SessionKey::new(peer, super::TCP, SessionId(session), Initiator::Remote)
}
/// Peer `Data` on a peer-opened session (`from_opener = true`).
fn data(session: u64, bytes: &'static [u8]) -> Frame {
    Frame::Data {
        session: SessionId(session),
        from_opener: true,
        bytes: Bytes::from_static(bytes),
    }
}
/// Peer `Shutdown` on a peer-opened session (`from_opener = true`).
fn shutdown(session: u64) -> Frame {
    Frame::Shutdown {
        session: SessionId(session),
        from_opener: true,
    }
}
/// Peer `Close` on a peer-opened session (`from_opener = true`).
fn close(session: u64) -> Frame {
    Frame::Close {
        session: SessionId(session),
        from_opener: true,
    }
}
/// Peer `Open` for `service`.
fn open(session: u64, service: &str) -> Frame {
    Frame::Open {
        session: SessionId(session),
        service: service.to_string(),
    }
}

fn web_relay() -> Relay<SocketAddr> {
    let mut config = HashMap::new();
    config.insert("web".to_string(), web_addr());
    Relay::tcp(config)
}

/// Decode a peer frame then step.
fn step_frame(
    relay: &Relay<SocketAddr>,
    state: &RelayState<SocketAddr>,
    from: Did,
    frame: &Frame,
) -> Transition<RelayState<SocketAddr>, RelayEffect<SocketAddr>> {
    let payload = rings_codec::serialize(frame).unwrap();
    let event = relay
        .decode(Wire {
            from,
            me: this_node(),
            payload: payload.as_ref(),
        })
        .unwrap();
    relay.step(
        Ctx {
            did: this_node(),
            state,
        },
        event,
    )
}

/// Decode a self command then step.
fn step_command(
    relay: &Relay<SocketAddr>,
    state: &RelayState<SocketAddr>,
    command: &RelayCommand<SocketAddr>,
) -> Transition<RelayState<SocketAddr>, RelayEffect<SocketAddr>> {
    let payload = rings_codec::serialize(command).unwrap();
    let event = relay
        .decode(Wire {
            from: this_node(),
            me: this_node(),
            payload: payload.as_ref(),
        })
        .unwrap();
    relay.step(
        Ctx {
            did: this_node(),
            state,
        },
        event,
    )
}

#[test]
fn test_open_known_service_connects_and_records_the_session() {
    let relay = web_relay();
    let t = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
    let expected = rkey(peer_a(), 7);
    match t.effects.as_slice() {
        [RelayEffect::Connect { key, target, kind }] => {
            assert_eq!(*key, expected);
            assert_eq!(*target, web_addr());
            assert!(matches!(kind, TransportKind::Tcp));
        }
        other => panic!("expected one Connect, got {other:?}"),
    }
    assert!(t.state.sessions.contains(&expected));
}

#[test]
fn test_duplicate_open_for_a_live_session_is_rejected() {
    let relay = web_relay();
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
    assert!(opened.state.sessions.contains(&rkey(peer_a(), 7)));
    let again = step_frame(&relay, &opened.state, peer_a(), &open(7, "web"));
    assert!(
        again.effects.is_empty(),
        "duplicate Open must emit no effect"
    );
    assert_eq!(again.state.sessions.len(), 1);
}

#[test]
fn test_open_unknown_service_emits_a_retryable_terminal_response() {
    let relay = web_relay();
    let t = step_frame(&relay, &relay.init(), peer_a(), &open(7, "ssh"));
    match t.effects.as_slice() {
        [RelayEffect::SendClose {
            to,
            session,
            from_opener,
        }] => {
            assert_eq!(*to, peer_a());
            assert_eq!(*session, SessionId(7));
            assert!(!from_opener, "we are not the opener of the peer's session");
        }
        other => panic!("expected one SendClose, got {other:?}"),
    }
    assert!(t.state.sessions.is_empty());

    let duplicate = step_frame(&relay, &t.state, peer_a(), &open(7, "ssh"));
    assert!(matches!(duplicate.effects.as_slice(), [
        RelayEffect::SendClose { .. }
    ]));
    assert!(duplicate.state.sessions.is_empty());
}

#[test]
fn test_per_peer_session_budget_rejects_only_the_saturated_peer() {
    let relay = web_relay();
    let mut state = relay.init();
    for session in 0..MAX_RELAY_SESSIONS_PER_PEER as u64 {
        assert!(state.insert_session(rkey(peer_a(), session)));
    }

    let rejected = step_frame(
        &relay,
        &state,
        peer_a(),
        &open(MAX_RELAY_SESSIONS_PER_PEER as u64, "web"),
    );
    assert_eq!(rejected.state.sessions, state.sessions);
    assert!(matches!(rejected.effects.as_slice(), [
        RelayEffect::SendClose { to, .. }
    ] if *to == peer_a()));

    let admitted = step_frame(&relay, &state, peer_b(), &open(0, "web"));
    assert!(admitted.state.sessions.contains(&rkey(peer_b(), 0)));
    assert!(matches!(admitted.effects.as_slice(), [
        RelayEffect::Connect { .. }
    ]));
}

#[test]
fn test_global_session_budget_rejects_without_allocating_state() {
    let relay = web_relay();
    let mut state = relay.init();
    for index in 0..MAX_RELAY_SESSIONS {
        let peer = Did::from(10_u32 + (index / MAX_RELAY_SESSIONS_PER_PEER) as u32);
        let session = (index % MAX_RELAY_SESSIONS_PER_PEER) as u64;
        assert!(state.insert_session(rkey(peer, session)));
    }
    assert_eq!(state.sessions.len(), MAX_RELAY_SESSIONS);

    let rejected = step_frame(&relay, &state, Did::from(100_u32), &open(0, "web"));
    assert_eq!(rejected.state.sessions, state.sessions);
    assert!(matches!(rejected.effects.as_slice(), [
        RelayEffect::SendClose { .. }
    ]));
}

#[test]
fn test_data_writes_to_a_live_keyed_session() {
    let relay = web_relay();
    // Data is now guarded on the session set, so open it first.
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
    let t = step_frame(&relay, &opened.state, peer_a(), &data(7, b"hello"));
    match t.effects.as_slice() {
        [RelayEffect::Write { key, bytes }] => {
            assert_eq!(*key, rkey(peer_a(), 7));
            assert_eq!(bytes.as_ref(), b"hello");
        }
        other => panic!("expected one Write, got {other:?}"),
    }
    assert!(std::sync::Arc::ptr_eq(
        &opened.state.sessions,
        &t.state.sessions
    ));
    assert_eq!(opened.state.session_quota, t.state.session_quota);
    assert!(std::sync::Arc::ptr_eq(
        &opened.state.peer_shutdown,
        &t.state.peer_shutdown
    ));
}

#[test]
fn test_data_for_an_unknown_session_is_dropped_by_the_reducer() {
    let relay = web_relay();
    // No Open first: the reducer (not the engine table) decides there is no such session.
    let t = step_frame(&relay, &relay.init(), peer_a(), &data(7, b"hello"));
    assert!(
        t.effects.is_empty(),
        "Data for an unknown session emits nothing"
    );
}

#[test]
fn test_close_removes_the_session_and_emits_close() {
    let relay = web_relay();
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
    let t = step_frame(&relay, &opened.state, peer_a(), &close(7));
    let expected = rkey(peer_a(), 7);
    match t.effects.as_slice() {
        [RelayEffect::Close { key }] => assert_eq!(*key, expected),
        other => panic!("expected one Close, got {other:?}"),
    }
    assert!(!t.state.sessions.contains(&expected));
}

#[test]
fn test_register_service_via_self_command_then_open_connects() {
    let relay = Relay::tcp(HashMap::new());
    let registered = step_command(&relay, &relay.init(), &RelayCommand::RegisterService {
        name: "web".to_string(),
        target: web_addr(),
    });
    assert!(registered.effects.is_empty());
    let t = step_frame(&relay, &registered.state, peer_a(), &open(1, "web"));
    match t.effects.as_slice() {
        [RelayEffect::Connect { target, .. }] => assert_eq!(*target, web_addr()),
        other => panic!("expected one Connect, got {other:?}"),
    }
}

#[test]
fn test_accepted_mints_in_the_core_then_untrack_removes() {
    let relay = web_relay();
    // A client-side accept is fed back as `Accepted{token}`. The core mints the session id
    // (0 on fresh state, initiator Local) and replies OpenAccepted with that minted key.
    let accepted = step_command(&relay, &relay.init(), &RelayCommand::Accepted {
        token: 42,
        peer: peer_a(),
        service: "web".to_string(),
    });
    let key = SessionKey::new(peer_a(), super::TCP, SessionId(0), Initiator::Local);
    match accepted.effects.as_slice() {
        [RelayEffect::OpenAccepted {
            token,
            key: k,
            service,
        }] => {
            assert_eq!(*token, 42);
            assert_eq!(*k, key);
            assert_eq!(service, "web");
        }
        other => panic!("expected one OpenAccepted, got {other:?}"),
    }
    assert!(accepted.state.sessions.contains(&key));

    let untracked = step_command(&relay, &accepted.state, &RelayCommand::Untrack {
        peer: peer_a(),
        session: SessionId(0),
        initiator: Initiator::Local,
    });
    assert!(untracked.effects.is_empty());
    assert!(!untracked.state.sessions.contains(&key));
}

#[test]
fn test_backend_abort_removes_session_and_emits_one_peer_close() {
    let relay = web_relay();
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
    let aborted = step_command(&relay, &opened.state, &RelayCommand::Abort {
        peer: peer_a(),
        session: SessionId(7),
        initiator: Initiator::Remote,
    });

    assert!(!aborted.state.sessions.contains(&rkey(peer_a(), 7)));
    assert!(matches!(
        aborted.effects.as_slice(),
        [RelayEffect::SendClose {
            to,
            session: SessionId(7),
            from_opener: false,
        }] if *to == peer_a()
    ));
}

#[test]
fn test_a_peer_cannot_address_another_peers_session() {
    let relay = web_relay();
    let a_open = step_frame(&relay, &relay.init(), peer_a(), &open(0, "web"));
    let key_a = rkey(peer_a(), 0);
    assert!(a_open.state.sessions.contains(&key_a));

    // peer B references session 0 (same id) but never opened it here: the reducer drops
    // both Data and Close — A's session is untouched. (Owner rejection in the core.)
    let b_data = step_frame(&relay, &a_open.state, peer_b(), &data(0, b"x"));
    assert!(
        b_data.effects.is_empty(),
        "B's Data for a session it did not open is dropped"
    );
    let b_close = step_frame(&relay, &a_open.state, peer_b(), &close(0));
    assert!(
        b_close.effects.is_empty(),
        "B's Close for a session it did not open is dropped"
    );
    assert!(b_close.state.sessions.contains(&key_a));
}

#[test]
fn test_local_and_remote_sessions_with_the_same_id_do_not_collide() {
    // Bidirectional open against the same peer, both id 0: a peer-opened (Remote) session
    // and a locally-accepted (Local) session must be distinct keys.
    let relay = web_relay();
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(0, "web"));
    let accepted = step_command(&relay, &opened.state, &RelayCommand::Accepted {
        token: 1,
        peer: peer_a(),
        service: "web".to_string(),
    });
    let remote = SessionKey::new(peer_a(), super::TCP, SessionId(0), Initiator::Remote);
    let local = SessionKey::new(peer_a(), super::TCP, SessionId(0), Initiator::Local);
    assert_ne!(remote, local);
    assert!(accepted.state.sessions.contains(&remote));
    assert!(accepted.state.sessions.contains(&local));
    assert_eq!(accepted.state.sessions.len(), 2);
}

// ── lifecycle property tests (reviewer-requested) ─────────────────────────────────

#[test]
fn test_open_then_close_then_data_does_not_resurrect_the_session() {
    let relay = web_relay();
    let key = rkey(peer_a(), 3);
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(3, "web"));
    assert!(opened.state.sessions.contains(&key));
    let closed = step_frame(&relay, &opened.state, peer_a(), &close(3));
    assert!(!closed.state.sessions.contains(&key));
    // A late Data for the now-closed session is dropped by the reducer (guarded on the
    // session set) — no effect, no resurrection.
    let late = step_frame(&relay, &closed.state, peer_a(), &data(3, b"late"));
    assert!(late.effects.is_empty());
    assert!(late.state.sessions.is_empty());
}

#[test]
fn test_close_after_close_is_idempotent() {
    let relay = web_relay();
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(5, "web"));
    let c1 = step_frame(&relay, &opened.state, peer_a(), &close(5));
    assert!(matches!(c1.effects.as_slice(), [RelayEffect::Close { .. }]));
    // The second close hits no live session: the reducer drops it (no effect, no panic).
    let c2 = step_frame(&relay, &c1.state, peer_a(), &close(5));
    assert!(c2.effects.is_empty());
    assert!(c2.state.sessions.is_empty());
}

#[test]
fn test_tcp_shutdown_is_affine_and_blocks_late_peer_data_without_closing_reverse() {
    let relay = web_relay();
    let key = rkey(peer_a(), 9);
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(9, "web"));

    let shutdown_once = step_frame(&relay, &opened.state, peer_a(), &shutdown(9));
    assert!(matches!(shutdown_once.effects.as_slice(), [
        RelayEffect::Shutdown { key: effect_key }
    ] if *effect_key == key));
    assert!(shutdown_once.state.sessions.contains(&key));
    assert!(shutdown_once.state.peer_shutdown.contains(&key));

    let shutdown_twice = step_frame(&relay, &shutdown_once.state, peer_a(), &shutdown(9));
    assert!(shutdown_twice.effects.is_empty());
    let late_data = step_frame(&relay, &shutdown_twice.state, peer_a(), &data(9, b"late"));
    assert!(late_data.effects.is_empty());

    let closed = step_frame(&relay, &late_data.state, peer_a(), &close(9));
    assert!(matches!(closed.effects.as_slice(), [
        RelayEffect::Close { .. }
    ]));
    assert!(!closed.state.sessions.contains(&key));
    assert!(!closed.state.peer_shutdown.contains(&key));
}

#[test]
fn test_udp_ignores_stream_shutdown_and_keeps_accepting_datagrams() {
    let mut config = HashMap::new();
    config.insert("dns".to_string(), web_addr());
    let relay = Relay::udp(config);
    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(4, "dns"));

    let shutdown = step_frame(&relay, &opened.state, peer_a(), &shutdown(4));
    assert!(shutdown.effects.is_empty());
    let data = step_frame(&relay, &shutdown.state, peer_a(), &data(4, b"datagram"));
    assert!(matches!(data.effects.as_slice(), [
        RelayEffect::Write { .. }
    ]));
}

#[test]
fn test_malformed_payload_is_rejected_at_the_boundary() {
    let relay = web_relay();
    let bad = [0xFFu8, 0xFF, 0xFF, 0xFF, 0xFF];
    let result = relay.decode(Wire {
        from: peer_a(),
        me: this_node(),
        payload: &bad,
    });
    assert!(result.is_err(), "a malformed frame must be rejected");
}

/// Property: across a long, deterministic, collision-prone interleaving of peer frames
/// (Open/Data/Close) from several peers, the pure `State.sessions` never diverges from an
/// independent model, and Data/Close only ever act on live sessions (reducer authority).
#[test]
fn test_lifecycle_property_state_never_diverges_from_model() {
    let relay = web_relay();
    let peers = [peer_a(), peer_b(), Did::from(4u32)];
    let mut state = relay.init();
    let mut model: HashSet<SessionKey> = HashSet::new();
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for _ in 0..4000 {
        let r = next();
        let peer = peers[(r % 3) as usize];
        let session = (r >> 2) & 0x7; // 8 ids → frequent collisions
        let key = rkey(peer, session);
        let transition = match (r >> 8) % 4 {
            0 => {
                let t = step_frame(&relay, &state, peer, &open(session, "web"));
                if model.contains(&key) {
                    assert!(t.effects.is_empty(), "live Open must emit nothing");
                } else {
                    assert!(matches!(t.effects.as_slice(), [
                        RelayEffect::Connect { .. }
                    ]));
                    model.insert(key.clone());
                }
                t
            }
            1 => {
                let t = step_frame(&relay, &state, peer, &open(session, "nope"));
                if model.contains(&key) {
                    assert!(t.effects.is_empty());
                } else {
                    assert!(matches!(t.effects.as_slice(), [
                        RelayEffect::SendClose { .. }
                    ]));
                }
                t
            }
            2 => {
                let t = step_frame(&relay, &state, peer, &data(session, b"x"));
                if model.contains(&key) {
                    match t.effects.as_slice() {
                        [RelayEffect::Write { key: k, .. }] => assert_eq!(*k, key),
                        other => panic!("expected one Write, got {other:?}"),
                    }
                } else {
                    assert!(
                        t.effects.is_empty(),
                        "Data on an unknown session is dropped"
                    );
                }
                t
            }
            _ => {
                let t = step_frame(&relay, &state, peer, &close(session));
                if model.contains(&key) {
                    assert!(matches!(t.effects.as_slice(), [RelayEffect::Close { .. }]));
                } else {
                    assert!(
                        t.effects.is_empty(),
                        "Close on an unknown session is dropped"
                    );
                }
                model.remove(&key);
                t
            }
        };
        state = transition.state;
        assert_eq!(
            state.sessions.as_ref(),
            &model,
            "State.sessions diverged from the model"
        );
        let mut projected_session_counts = HashMap::new();
        for key in &model {
            *projected_session_counts.entry(key.peer).or_insert(0) += 1;
        }
        assert_eq!(state.session_quota.total(), model.len());
        for (peer, count) in projected_session_counts {
            assert_eq!(state.session_quota.peer_total(peer), count);
        }
    }
}

/// A faithful in-test model of a relay engine's resource table: `key → generation`,
/// mirroring `register` (insert a fresh generation), `close` (drop the current handle) and
/// `close_if_current` (drop only if the generation matches, returning whether it did).
/// This logic is identical in the native [`TransportSessions`] and browser `WtSessions`
/// engines, so the model covers both — only the socket vs. WebTransport plumbing differs.
struct EngineModel {
    map: HashMap<SessionKey, u64>,
    next_gen: u64,
}

impl EngineModel {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_gen: 0,
        }
    }
    fn register(&mut self, key: SessionKey) -> Option<u64> {
        let gen = self.next_gen;
        self.next_gen = self.next_gen.checked_add(1)?;
        self.map.insert(key, gen);
        Some(gen)
    }
    fn close(&mut self, key: &SessionKey) {
        self.map.remove(key);
    }
    /// Returns whether it was the current owner (and removed it). A `false` here means the
    /// caller is a stale task: it must send the peer **no** `Close` either.
    fn close_if_current(&mut self, key: &SessionKey, gen: u64) -> bool {
        if self.map.get(key) == Some(&gen) {
            self.map.remove(key);
            true
        } else {
            false
        }
    }
}

/// Apply a step's effects to the engine model, returning the generation of any handle the
/// effects registered (a relay task's captured generation). Asserts `Write`/`Shutdown`
/// only ever hit a live handle — i.e. the reducer, not the engine table, decided.
fn apply_effects(
    eng: &mut EngineModel,
    effects: &[RelayEffect<SocketAddr>],
) -> Option<(SessionKey, u64)> {
    let mut registered = None;
    for effect in effects {
        match effect {
            RelayEffect::Connect { key, .. } | RelayEffect::OpenAccepted { key, .. } => {
                registered = eng
                    .register(key.clone())
                    .map(|generation| (key.clone(), generation));
            }
            RelayEffect::Write { key, .. } | RelayEffect::Shutdown { key } => {
                assert!(
                    eng.map.contains_key(key),
                    "effect targeted a non-live session"
                );
            }
            RelayEffect::Close { key } => eng.close(key),
            RelayEffect::SendClose { .. } | RelayEffect::RejectAccepted { .. } => {}
        }
    }
    registered
}

#[test]
fn test_engine_model_refines_non_reusing_generation_exhaustion() {
    let mut engine = EngineModel::new();
    engine.next_gen = u64::MAX;
    let key = rkey(peer_a(), 1);

    assert_eq!(engine.register(key), None);
    assert_eq!(engine.next_gen, u64::MAX);
    assert!(engine.map.is_empty());
}

#[test]
fn test_exhausted_session_allocator_rejects_accept_without_mutating_state() {
    let relay = web_relay();
    let mut state = relay.init();
    state.next_session = u64::MAX;

    let transition = step_command(&relay, &state, &RelayCommand::Accepted {
        token: 77,
        peer: peer_a(),
        service: "web".to_string(),
    });

    assert_eq!(transition.state.next_session, u64::MAX);
    assert!(transition.state.sessions.is_empty());
    assert!(matches!(transition.effects.as_slice(), [
        RelayEffect::RejectAccepted { token: 77 }
    ]));
}

#[test]
fn test_accepted_session_obeys_the_same_per_peer_budget() {
    let relay = web_relay();
    let mut state = relay.init();
    for session in 0..MAX_RELAY_SESSIONS_PER_PEER as u64 {
        assert!(state.insert_session(rkey(peer_a(), session)));
    }

    let transition = step_command(&relay, &state, &RelayCommand::Accepted {
        token: 78,
        peer: peer_a(),
        service: "web".to_string(),
    });

    assert_eq!(transition.state.next_session, state.next_session);
    assert_eq!(transition.state.sessions, state.sessions);
    assert!(matches!(transition.effects.as_slice(), [
        RelayEffect::RejectAccepted { token: 78 }
    ]));
}

#[test]
fn test_generation_prevents_a_slow_old_task_deleting_a_reopened_handle() {
    // Server session id 7 is opener-chosen, so it can be reused after close. Open → close
    // → reopen, then let the *old* task tear down: with generations it must not delete the
    // new handle (ABA safety).
    let relay = web_relay();
    let mut eng = EngineModel::new();

    let opened = step_frame(&relay, &relay.init(), peer_a(), &open(7, "web"));
    let (key, gen_old) = apply_effects(&mut eng, &opened.effects).expect("registered");

    // Peer closes; the reducer removes it and the engine drops the current handle.
    let closed = step_frame(&relay, &opened.state, peer_a(), &close(7));
    apply_effects(&mut eng, &closed.effects);
    assert!(!eng.map.contains_key(&key));

    // Peer reopens the same id → a new handle with a fresh generation.
    let reopened = step_frame(&relay, &closed.state, peer_a(), &open(7, "web"));
    let (_, gen_new) = apply_effects(&mut eng, &reopened.effects).expect("registered");
    assert_ne!(gen_old, gen_new);

    // The slow OLD task finally tears down with its stale generation: it must neither
    // remove the new handle nor (since this returns false) send the peer a `Close`.
    let removed = eng.close_if_current(&key, gen_old);
    assert!(
        !removed,
        "stale task must not remove — and so must send no peer Close"
    );
    assert_eq!(
        eng.map.get(&key),
        Some(&gen_new),
        "old task must not delete the reopened handle"
    );
}

#[test]
fn test_engine_model_stays_consistent_with_step_under_interleaving() {
    // Drive Open/Data/Close (peer frames) with *deferred* generation-checked teardowns,
    // asserting the pure session set and the engine model's live keys never diverge.
    let relay = web_relay();
    let peers = [peer_a(), peer_b()];
    let mut state = relay.init();
    let mut eng = EngineModel::new();
    let mut tasks: Vec<(SessionKey, u64)> = Vec::new();
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for _ in 0..3000 {
        let r = next();
        let peer = peers[(r % 2) as usize];
        let session = (r >> 2) & 0x3; // 4 ids → frequent reuse
        match (r >> 8) % 4 {
            0 => {
                let t = step_frame(&relay, &state, peer, &open(session, "web"));
                if let Some(task) = apply_effects(&mut eng, &t.effects) {
                    tasks.push(task);
                }
                state = t.state;
            }
            1 => {
                let t = step_frame(&relay, &state, peer, &data(session, b"x"));
                apply_effects(&mut eng, &t.effects);
                state = t.state;
            }
            2 => {
                // Peer close: reducer removes, engine drops current; the matching task is
                // now stale (its later teardown will be a generation no-op).
                let t = step_frame(&relay, &state, peer, &close(session));
                apply_effects(&mut eng, &t.effects);
                state = t.state;
            }
            _ => {
                // A deferred task teardown fires. `close_if_current` returns whether this
                // task was still current; ONLY then does it Untrack and (would) send the
                // peer a Close. A stale task must do neither — modelled by the `removed`
                // gate, so a reopened session is never torn down by an old task.
                if !tasks.is_empty() {
                    let idx = (r >> 16) as usize % tasks.len();
                    let (tkey, tgen) = tasks.swap_remove(idx);
                    let removed = eng.close_if_current(&tkey, tgen);
                    if removed {
                        let untrack = RelayCommand::Untrack {
                            peer: tkey.peer,
                            session: tkey.session,
                            initiator: tkey.initiator,
                        };
                        state = step_command(&relay, &state, &untrack).state;
                    }
                }
            }
        }
        // The pure session set and the engine's live keys must agree at every step.
        let live: HashSet<SessionKey> = eng.map.keys().cloned().collect();
        assert_eq!(
            state.sessions.as_ref(),
            &live,
            "pure state diverged from the engine model"
        );
    }
}
