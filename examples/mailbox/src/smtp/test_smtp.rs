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
