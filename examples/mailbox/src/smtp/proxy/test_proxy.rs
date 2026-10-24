use ppp::v2::Builder;
use ppp::v2::Protocol;
use ppp::v2::Version;
use tokio::io::AsyncWriteExt;

use super::*;

/// The v2 header of `command` from `client` to `relayed`.
fn header(command: Command, client: SocketAddr, relayed: SocketAddr) -> Vec<u8> {
    Builder::with_addresses(Version::Two | command, Protocol::Stream, (client, relayed))
        .build()
        .unwrap()
}

#[tokio::test]
async fn test_a_proxy_header_names_the_client_and_leaves_the_rest() {
    let client: SocketAddr = "[2001:db8::7]:40000".parse().unwrap();
    let relayed: SocketAddr = "[2001:db8:1::9]:25".parse().unwrap();
    let (mut ours, mut theirs) = tokio::io::duplex(1024);
    theirs
        .write_all(&header(Command::Proxy, client, relayed))
        .await
        .unwrap();
    theirs.write_all(b"EHLO").await.unwrap();
    assert_eq!(read_client(&mut ours).await.unwrap(), Some(client));
    let mut rest = [0u8; 4];
    ours.read_exact(&mut rest).await.unwrap();
    assert_eq!(&rest, b"EHLO", "the client's bytes follow untouched");
}

#[tokio::test]
async fn test_a_local_header_names_no_client() {
    let address: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let (mut ours, mut theirs) = tokio::io::duplex(1024);
    theirs
        .write_all(&header(Command::Local, address, address))
        .await
        .unwrap();
    assert_eq!(read_client(&mut ours).await.unwrap(), None);
}

#[tokio::test]
async fn test_bytes_that_are_not_a_header_are_refused() {
    let (mut ours, mut theirs) = tokio::io::duplex(1024);
    theirs.write_all(b"EHLO client.example\r\n").await.unwrap();
    assert!(matches!(
        read_client(&mut ours).await,
        Err(ProxyError::Malformed(_))
    ));
}
