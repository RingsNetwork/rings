//! Engine table tests: slot registration, bounded local queues and UDP source admission.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::net::UdpSocket;

use super::Outbound;
use super::Pending;
use super::TransportSessions;
use super::MAX_PENDING_ACCEPTS;
use crate::extension::transport::EffectEnqueue;
use crate::extension::transport::Initiator;
use crate::extension::transport::RelaySessionId;
use crate::extension::transport::SessionKey;
use crate::extension::transport::SlotRegistration;
use crate::extension::transport::TransportKind;

/// Without a current runtime the connect effect is refused before its slot is claimed, so
/// no session stays registered for relay work that never starts.
#[test]
fn test_connect_without_a_runtime_claims_no_slot() -> crate::error::Result<()> {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let (task, sessions, _) = runtime.block_on(async { super::relay_task_for_test("tcp") })?;
    let key = SessionKey::new(
        Did::from(8_u32),
        "tcp",
        RelaySessionId(12),
        Initiator::Remote,
    );
    let effect = crate::test_support::without_runtime(|| {
        Arc::clone(&sessions).connect(
            task.scope.clone(),
            key.clone(),
            SocketAddr::from(([127, 0, 0, 1], 9)),
            TransportKind::Tcp,
        )
    });

    assert_eq!(effect, EffectEnqueue::Failed);
    assert_eq!(sessions.current_generation(&key), None);
    Ok(())
}

/// A repeated engine effect cannot cancel or replace the resource admitted first.
#[test]
fn test_duplicate_registration_preserves_live_owner() {
    let sessions = TransportSessions::new();
    let key = SessionKey::new(
        Did::from(7_u32),
        "tcp",
        RelaySessionId(10),
        Initiator::Remote,
    );
    let (original_cancel, original_generation) = match sessions.register(key.clone(), None) {
        SlotRegistration::Registered((_receiver, cancel, generation)) => (cancel, generation),
        SlotRegistration::AlreadyPresent | SlotRegistration::Failed => {
            panic!("first registration must occupy a vacant slot");
        }
    };

    assert!(matches!(
        sessions.register(key.clone(), None),
        SlotRegistration::AlreadyPresent
    ));
    assert!(!original_cancel.is_cancelled());
    assert_eq!(sessions.current_generation(&key), Some(original_generation));
}

#[test]
fn test_saturated_local_queue_fails_closed_without_waiting() {
    let sessions = TransportSessions::new();
    let key = SessionKey::new(
        Did::from(7_u32),
        "tcp",
        RelaySessionId(11),
        Initiator::Remote,
    );
    let _registration = match sessions.register(key.clone(), None) {
        SlotRegistration::Registered(registration) => registration,
        SlotRegistration::AlreadyPresent | SlotRegistration::Failed => {
            panic!("first registration must occupy a vacant slot");
        }
    };

    for seq in 0..1024 {
        assert_eq!(
            sessions.deliver(&key, Outbound::Segment {
                seq,
                bytes: Bytes::from_static(b"x")
            }),
            EffectEnqueue::Enqueued
        );
    }
    assert_eq!(
        sessions.deliver(&key, Outbound::Segment {
            seq: 1024,
            bytes: Bytes::from_static(b"overflow")
        }),
        EffectEnqueue::Failed
    );
    assert!(!sessions.is_live(&key));
    assert_eq!(
        sessions.deliver(&key, Outbound::Segment {
            seq: 1025,
            bytes: Bytes::from_static(b"stale")
        }),
        EffectEnqueue::Missing
    );
}

#[tokio::test]
async fn test_pending_accept_table_rejects_above_its_hard_bound() {
    let sessions = TransportSessions::new();
    let socket = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind test UDP socket"),
    );
    for index in 0..MAX_PENDING_ACCEPTS {
        let src = SocketAddr::from(([127, 0, 0, 1], 10_000 + index as u16));
        assert!(sessions
            .stash_pending(Pending::Udp {
                socket: Arc::clone(&socket),
                src,
                first: Bytes::new(),
            })
            .is_some());
    }

    assert!(sessions
        .stash_pending(Pending::Udp {
            socket,
            src: SocketAddr::from(([127, 0, 0, 1], 20_000)),
            first: Bytes::new(),
        })
        .is_none());
    assert_eq!(
        sessions
            .pending
            .lock()
            .expect("pending table remains readable")
            .len(),
        MAX_PENDING_ACCEPTS
    );
}

#[tokio::test]
async fn test_udp_source_is_unique_and_invisible_until_open_admission() {
    let sessions = TransportSessions::new();
    let socket = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind test UDP socket"),
    );
    let src = SocketAddr::from(([127, 0, 0, 1], 12_345));
    let token = sessions
        .reserve_pending_udp(Arc::clone(&socket), src, Bytes::from_static(b"first"))
        .expect("reserve first UDP source");

    assert!(sessions
        .reserve_pending_udp(socket, src, Bytes::from_static(b"overtaking"))
        .is_none());
    assert_eq!(sessions.pending.lock().expect("pending table").len(), 1);
    assert_eq!(sessions.udp_flow(&src), None);

    let pending = sessions
        .pending
        .lock()
        .expect("pending table")
        .remove(&token);
    assert!(matches!(pending, Some(Pending::Udp { .. })));
    let key = SessionKey::new(
        Did::from(8_u32),
        "udp",
        RelaySessionId(12),
        Initiator::Local,
    );
    assert!(sessions.promote_udp_flow(src, token, &key));
    assert!(matches!(
        sessions.register(key.clone(), Some(src)),
        SlotRegistration::Registered(_)
    ));
    assert_eq!(sessions.udp_flow(&src), None);

    assert!(sessions.activate_udp_flow(src, &key));
    assert_eq!(sessions.udp_flow(&src), Some(key));
}

#[tokio::test]
async fn test_failed_udp_accept_releases_its_source_reservation() {
    let sessions = TransportSessions::new();
    let socket = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind test UDP socket"),
    );
    let src = SocketAddr::from(([127, 0, 0, 1], 12_346));
    let token = sessions
        .reserve_pending_udp(Arc::clone(&socket), src, Bytes::new())
        .expect("reserve first UDP source");

    sessions.evict_pending(token);

    assert!(sessions
        .reserve_pending_udp(socket, src, Bytes::new())
        .is_some());
}
