use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use super::*;

/// A connected loopback pair: the dialing end and the accepted end.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dialing = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (accepted, _) = listener.accept().await.unwrap();
    (dialing, accepted)
}

#[tokio::test]
async fn test_splice_carries_both_directions_and_each_end_of_stream() {
    let (mut client, client_side) = pair().await;
    let (upstream_side, mut upstream) = pair().await;
    let spliced = tokio::spawn(splice(client_side, upstream_side, Duration::from_secs(60)));
    client.write_all(b"ping").await.unwrap();
    client.shutdown().await.unwrap();
    let mut received = Vec::new();
    upstream.read_to_end(&mut received).await.unwrap();
    assert_eq!(received, b"ping", "the client's end reaches the upstream");
    upstream.write_all(b"pong").await.unwrap();
    upstream.shutdown().await.unwrap();
    let mut answered = Vec::new();
    client.read_to_end(&mut answered).await.unwrap();
    assert_eq!(answered, b"pong");
    spliced.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_a_silent_splice_ends_after_its_idle_time() {
    let (_client, client_side) = pair().await;
    let (upstream_side, _upstream) = pair().await;
    let idle = Duration::from_millis(50);
    let ended = tokio::time::timeout(
        Duration::from_secs(5),
        splice(client_side, upstream_side, idle),
    )
    .await
    .expect("the splice ends on its own");
    assert!(matches!(ended, Err(ForwardError::Idle(after)) if after == idle));
}
