use bytes::Bytes;
use rings_gateway::BoxGatewayDuplex;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::super::duplex::TcpDuplexState;
use super::super::NativeOnionOpenStream;
use crate::onion::session::dial::OnionClientStream;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::session::witness::OnionSessionCondition;

/// Law (half-close): each half closes independently and stays closed; the stream is over
/// exactly when both are.
#[test]
fn test_tcp_duplex_state_closes_only_after_both_halves_close() {
    let mut state = TcpDuplexState::open();
    assert!(state.can_read() && state.can_write() && !state.is_closed());

    state.close_read();
    assert!(!state.can_read());
    assert!(state.can_write());
    assert!(!state.is_closed());

    state.close_read();
    assert!(!state.can_read());

    state.close_write();
    assert!(state.is_closed());
}

/// Law (commutation): the closed state does not depend on the order the halves close in.
#[test]
fn test_tcp_duplex_state_close_order_commutes() {
    let mut read_first = TcpDuplexState::open();
    read_first.close_read();
    read_first.close_write();
    let mut write_first = TcpDuplexState::open();
    write_first.close_write();
    write_first.close_read();

    assert_eq!(read_first, write_first);
}

/// A connected pair of loopback sockets: the local stream the pump holds, and its peer.
async fn socket_pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let (peer, accepted) = tokio::join!(tokio::net::TcpStream::connect(address), listener.accept());
    (accepted.expect("accept").0, peer.expect("connect"))
}

/// Law (fail closed, #843 D2′ `abort`): a session that fails after delivering bytes resets the
/// local socket, so its peer reads the bytes and then a reset, never an orderly end.
#[tokio::test]
async fn test_a_failed_session_resets_the_local_socket() {
    let (local, mut peer) = socket_pair().await;
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    NativeOnionOpenStream::new(stream).relay(local);

    driver
        .emit(OnionStreamEvent::Data(Bytes::from_static(b"part")))
        .await;
    let mut received = [0_u8; 4];
    peer.read_exact(&mut received)
        .await
        .expect("the bytes before the failure");
    assert_eq!(&received, b"part");
    // Only after the bytes are read: a reset may discard a receive buffer not yet read.
    driver.emit(OnionStreamEvent::Failed).await;
    let end = peer.read(&mut [0_u8; 1]).await;
    assert_eq!(
        end.map_err(|error| error.kind()),
        Err(std::io::ErrorKind::ConnectionReset)
    );
}

/// Law (half-close): a session whose two directions both end in order closes the local socket
/// cleanly, so its peer reads the bytes and then an orderly end.
#[tokio::test]
async fn test_a_completed_session_closes_the_local_socket_cleanly() {
    let (local, mut peer) = socket_pair().await;
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    NativeOnionOpenStream::new(stream).relay(local);

    peer.shutdown().await.expect("the peer's end of stream");
    assert_eq!(driver.next_is_fin().await, Some(true));
    driver
        .emit(OnionStreamEvent::Data(Bytes::from_static(b"whole")))
        .await;
    driver.emit(OnionStreamEvent::Fin).await;

    let mut received = Vec::new();
    peer.read_to_end(&mut received)
        .await
        .expect("an orderly end");
    assert_eq!(received, b"whole");
}

/// Law (fail closed): a failure after the session's `fin` (an upload the exit
/// could no longer write) leaves the complete download intact: its peer reads every byte and
/// an orderly end, never a reset.
#[tokio::test]
async fn test_a_failure_after_the_sessions_fin_keeps_the_complete_stream() {
    let (local, mut peer) = socket_pair().await;
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    NativeOnionOpenStream::new(stream).relay(local);

    driver
        .emit(OnionStreamEvent::Data(Bytes::from_static(b"whole")))
        .await;
    driver.emit(OnionStreamEvent::Fin).await;
    driver.emit(OnionStreamEvent::Failed).await;

    let mut received = Vec::new();
    peer.read_to_end(&mut received)
        .await
        .expect("an orderly end");
    assert_eq!(received, b"whole");
}

/// Law (report before end, #902, #950): a witnessed relay whose session fails reads `Failed` in
/// its witness before its local stream ends, so a boundary that sees only that end (a gateway
/// duplex, whose reset is a drop and reads as EOF) reads the failure, never an orderly end.
#[tokio::test]
async fn test_a_witnessed_relay_reports_a_failure_before_its_local_stream_ends() {
    let (local, mut peer) = tokio::io::duplex(64);
    let local: BoxGatewayDuplex = Box::new(local);
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    let witness = NativeOnionOpenStream::new(stream).relay_witnessed(local);
    assert_eq!(witness.condition(), OnionSessionCondition::Live);

    driver.emit(OnionStreamEvent::Failed).await;
    let end = peer.read(&mut [0_u8; 1]).await;
    assert_eq!(end.map_err(|error| error.kind()), Ok(0));
    assert_eq!(witness.condition(), OnionSessionCondition::Failed);
}

/// Law (whole stream): a witnessed relay whose session's `fin` reached the local stream never
/// reads `Failed`, even when the session fails afterwards: the stream is whole.
#[tokio::test]
async fn test_a_witnessed_relay_keeps_a_whole_stream_unfailed() {
    let (local, mut peer) = tokio::io::duplex(64);
    let local: BoxGatewayDuplex = Box::new(local);
    let (stream, mut driver) = OnionClientStream::driven_by_test();
    let witness = NativeOnionOpenStream::new(stream).relay_witnessed(local);

    driver
        .emit(OnionStreamEvent::Data(Bytes::from_static(b"whole")))
        .await;
    driver.emit(OnionStreamEvent::Fin).await;
    driver.emit(OnionStreamEvent::Failed).await;
    let mut received = Vec::new();
    peer.read_to_end(&mut received)
        .await
        .expect("an orderly end");
    assert_eq!(received, b"whole");
    // The pump is gone once its command queue closes; it recorded nothing on its way out.
    assert_eq!(driver.next_is_fin().await, None);
    assert_eq!(witness.condition(), OnionSessionCondition::Live);
}

/// Law (fail closed at the stop, #1085): the node's stop ends a live pump as failed, since its
/// bytes did not end with the session's `fin`. Once the node's tasks have drained, the witness
/// reads `Failed` and the local socket's peer reads a reset, never the orderly end a dropped
/// pump would leave.
#[tokio::test]
async fn test_the_nodes_stop_resets_a_live_local_socket() {
    let (local, mut peer) = socket_pair().await;
    let (stream, _driver) = OnionClientStream::driven_by_test();
    let tasks = stream.tasks().clone();
    let witness = NativeOnionOpenStream::new(stream).relay_witnessed(local);
    assert_eq!(witness.condition(), OnionSessionCondition::Live);

    tasks.stop();
    tasks.drained().await;

    assert_eq!(witness.condition(), OnionSessionCondition::Failed);
    let end = peer.read(&mut [0_u8; 1]).await;
    assert_eq!(
        end.map_err(|error| error.kind()),
        Err(std::io::ErrorKind::ConnectionReset)
    );
}
