use super::*;

#[test]
fn test_the_trace_names_the_client_by_address_literal() {
    let transaction = Transaction {
        client: "mx.example".to_owned(),
        from: "a@example.com".to_owned(),
        to: vec![Recipient {
            address: "me@o1.rings.rs".to_owned(),
            mailbox: None,
        }],
        message: b"Subject: hi\r\n\r\nbody".to_vec(),
    };
    for (client, literal) in [
        ("192.0.2.7:40000", "[192.0.2.7]"),
        ("[2001:db8::7]:40000", "[IPv6:2001:db8::7]"),
    ] {
        let traced = with_trace("o1.rings", client.parse().unwrap(), &transaction);
        let traced = String::from_utf8(traced).unwrap();
        let received = format!("Received: from mx.example ({literal}) by o1.rings with ESMTP;");
        assert!(traced.contains(&received), "{traced}");
        assert!(traced.ends_with("\r\n\r\nbody"), "the message follows");
    }
}

#[test]
fn test_store_too_large_maps_to_552_instead_of_451() {
    let reply = CommitError::Store(StoreError::TooLarge(1024)).reply();
    assert_eq!(reply.code, 552);
    assert_eq!(reply.status, "5.3.4");

    let gateway_bound_reply = CommitError::TooLarge(1024).reply();
    assert_eq!(gateway_bound_reply.code, 552);
    assert_eq!(gateway_bound_reply.status, "5.3.4");
}

#[tokio::test]
async fn test_inbound_receiver_accounts_for_trace_header_bytes_in_max_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let label = "0x00000000000000000000000000000000000a11ce";
    let host = format!("{label}.rings");
    let client: SocketAddr = "127.0.0.1:41234".parse().expect("client addr");
    let trace_len = trace_header(&host, client, "mx.example", "alice@example.org").len();
    let max_size = trace_len + 20;

    let store = Arc::new(Store::open(dir.path(), max_size).await.expect("open store"));
    let service = SmtpService {
        role: Role::Inbound {
            local: "me".to_owned(),
            label: label.to_owned(),
        },
        host: host.clone(),
        max_size,
        sink: Sink::Inbox(Arc::clone(&store)),
        proxy_protocol: false,
        tls: None,
    };
    let session = Session::new(
        service.role.clone(),
        &service.host,
        max_size,
        MAX_RECIPIENTS,
    );
    let mut conversation = Conversation::new(session, client);

    let feed = async |conversation: &mut Conversation, input: &[u8]| -> String {
        let mut out = Vec::new();
        let mut iter = input.iter();
        conversation
            .ingest(&service, &mut iter, &mut out)
            .await
            .expect("ingest succeeds");
        String::from_utf8(out).expect("utf8 smtp output")
    };

    let ehlo = feed(&mut conversation, b"EHLO mx.example\r\n").await;
    assert!(ehlo.starts_with("250-"), "{ehlo}");

    // Declared SIZE that fits raw max_size (`25 < max_size`) but overflows once trace is added.
    let declared_too_big = feed(
        &mut conversation,
        format!("MAIL FROM:<alice@example.org> SIZE={}\r\n", 21).as_bytes(),
    )
    .await;
    assert!(
        declared_too_big.starts_with("552 5.3.4"),
        "{declared_too_big}"
    );

    // Undeclared SIZE with 21-byte message body (`trace_len + 21 > max_size`) is rejected with 552.
    assert!(
        feed(&mut conversation, b"MAIL FROM:<alice@example.org>\r\n")
            .await
            .starts_with("250 2.1.0")
    );
    assert!(feed(
        &mut conversation,
        format!("RCPT TO:<me@{label}.rings.rs>\r\n").as_bytes(),
    )
    .await
    .starts_with("250 2.1.5"));
    assert!(feed(&mut conversation, b"DATA\r\n")
        .await
        .starts_with("354 "));
    let too_big_body = "a".repeat(19); // + "\r\n" = 21 bytes
    let data_reply = feed(
        &mut conversation,
        format!("{too_big_body}\r\n.\r\n").as_bytes(),
    )
    .await;
    assert!(data_reply.starts_with("552 5.3.4"), "{data_reply}");

    // A 20-byte message body (`trace_len + 20 == max_size`) succeeds with 250.
    assert!(
        feed(&mut conversation, b"MAIL FROM:<alice@example.org>\r\n")
            .await
            .starts_with("250 2.1.0")
    );
    assert!(feed(
        &mut conversation,
        format!("RCPT TO:<me@{label}.rings.rs>\r\n").as_bytes(),
    )
    .await
    .starts_with("250 2.1.5"));
    assert!(feed(&mut conversation, b"DATA\r\n")
        .await
        .starts_with("354 "));
    let exact_body = "b".repeat(18); // + "\r\n" = 20 bytes
    let ok_reply = feed(
        &mut conversation,
        format!("{exact_body}\r\n.\r\n").as_bytes(),
    )
    .await;
    assert!(ok_reply.starts_with("250 2.0.0"), "{ok_reply}");
}
