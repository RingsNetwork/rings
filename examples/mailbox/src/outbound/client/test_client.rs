use std::sync::Mutex;

use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;

use super::*;

#[test]
fn test_data_stuffs_dots_ends_lines_with_crlf_and_closes() {
    assert_eq!(data_of(b".a\r\nb\n..c"), b"..a\r\nb\r\n...c\r\n.\r\n");
    assert_eq!(data_of(b"line\r\n"), b"line\r\n.\r\n");
    assert_eq!(data_of(b""), b".\r\n");
}

#[tokio::test]
async fn test_starttls_is_read_from_the_ehlo_extension_lines() {
    let (client, mut server) = tokio::io::duplex(1024);
    server
        .write_all(b"250-mx.example\r\n250-PIPELINING\r\n250-STARTTLS\r\n250 SIZE 100\r\n")
        .await
        .unwrap();
    let extensions = Wire::new(client).expect(Step::Ehlo, &[250]).await.unwrap();
    assert!(offers_starttls(&extensions), "{extensions:?}");
    assert!(!offers_starttls("mx.example STARTTLS\nSIZE 100"));
}

/// A mail exchanger that answers RCPT with `rcpt`, offers no STARTTLS, and keeps the commands
/// and message it receives.
async fn exchanger(rcpt: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&received);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        write.write_all(b"220 mx.example\r\n").await.unwrap();
        let mut in_data = false;
        while let Ok(Some(line)) = lines.next_line().await {
            kept.lock().unwrap().push(line.clone());
            let reply: &[u8] = match (in_data, line.get(..4).unwrap_or(&line)) {
                (true, _) if line == "." => b"250 queued\r\n",
                (true, _) => continue,
                (false, "EHLO") => b"250-mx.example\r\n250 SIZE 1000\r\n",
                (false, "MAIL") => b"250 ok\r\n",
                (false, "RCPT") => rcpt.as_bytes(),
                (false, "DATA") => b"354 go on\r\n",
                (false, _) => b"221 bye\r\n",
            };
            in_data = (in_data && line != ".") || line == "DATA";
            write.write_all(reply).await.unwrap();
        }
    });
    (address, received)
}

/// A delivery of `message` from `a@example.com` to `b@example.net`.
fn delivery(message: &[u8]) -> Delivery<'_> {
    Delivery {
        helo: "client.example",
        from: "a@example.com",
        recipient: "b@example.net",
        message,
    }
}

#[tokio::test]
async fn test_a_delivery_speaks_the_whole_transaction_in_plain() {
    let (address, received) = exchanger("250 ok\r\n").await;
    deliver(address, delivery(b"Subject: hi\r\n\r\n.dot"))
        .await
        .unwrap();
    // The client leaves after QUIT without waiting, so only the transaction is certain to be in.
    let received = received.lock().unwrap().clone();
    assert_eq!(
        received.get(..8),
        Some(
            &[
                "EHLO client.example",
                "MAIL FROM:<a@example.com>",
                "RCPT TO:<b@example.net>",
                "DATA",
                "Subject: hi",
                "",
                "..dot",
                ".",
            ]
            .map(String::from)[..]
        )
    );
}

#[tokio::test]
async fn test_a_refused_recipient_fails_for_good_or_for_now_by_its_code() {
    for (reply, permanent) in [("550 no such user\r\n", true), ("451 try later\r\n", false)] {
        let (address, _received) = exchanger(reply).await;
        let failed = deliver(address, delivery(b"x")).await.unwrap_err();
        assert!(
            matches!(failed, SendError::Refused {
                step: Step::RcptTo,
                ..
            }),
            "{failed}"
        );
        assert_eq!(failed.is_permanent(), permanent, "{failed}");
    }
}

#[tokio::test]
async fn test_bytes_pipelined_after_the_starttls_reply_are_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        write.write_all(b"220 mx.example\r\n").await.unwrap();
        lines.next_line().await.unwrap();
        write
            .write_all(b"250-mx.example\r\n250 STARTTLS\r\n")
            .await
            .unwrap();
        lines.next_line().await.unwrap();
        // A reply to STARTTLS with a forged reply behind it, in the clear.
        write
            .write_all(b"220 go ahead\r\n250 forged\r\n")
            .await
            .unwrap();
        let _held = lines.next_line().await;
    });
    let failed = deliver(address, delivery(b"x")).await.unwrap_err();
    assert!(matches!(failed, SendError::Injected), "{failed}");
}
