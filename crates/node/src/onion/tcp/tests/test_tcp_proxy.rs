use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::super::duplex::TcpDuplexState;
use super::super::NativeOnionOpenStream;
use crate::onion::session::dial::OnionClientStream;
use crate::onion::session::dial::OnionStreamEvent;

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
