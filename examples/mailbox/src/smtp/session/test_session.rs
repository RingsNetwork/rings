use smtp_proto::MailFrom;
use smtp_proto::RcptTo;

use super::*;

/// The owner's label in every case.
const LABEL: &str = "0x00000000000000000000000000000000000a11ce";

/// A receiver session for `me@<LABEL>.<zone>`.
fn receiver() -> Session {
    Session::new(
        Role::Inbound {
            local: "me".to_owned(),
            label: LABEL.to_owned(),
        },
        "mx.test",
        100,
        2,
    )
}

/// MAIL FROM `from`, declaring `size`.
fn mail(from: &str, size: usize) -> Request<String> {
    Request::Mail {
        from: MailFrom {
            address: from.to_owned(),
            size,
            ..MailFrom::default()
        },
    }
}

/// RCPT TO `to`.
fn rcpt(to: &str) -> Request<String> {
    Request::Rcpt {
        to: RcptTo {
            address: to.to_owned(),
            orcpt: None,
            rrvs: 0,
            flags: 0,
        },
    }
}

/// The code of `next`'s reply.
fn code(next: &Next) -> u16 {
    match next {
        Next::Reply(reply) | Next::ReadData(reply) | Next::Close(reply) => reply.code,
    }
}

#[test]
fn test_the_receiver_accepts_its_own_address_in_every_zone() {
    let role = Role::Inbound {
        local: "me".to_owned(),
        label: LABEL.to_owned(),
    };
    for accepted in [
        format!("me@{LABEL}.rings.rs"),
        format!("ME@{}.P2P.example.com", LABEL.to_uppercase()),
    ] {
        assert!(role.accepts(&accepted), "{accepted}");
    }
    for refused in [
        format!("you@{LABEL}.rings.rs"),
        format!("me@{LABEL}"),
        format!("me@x{LABEL}.rings.rs"),
        "me@example.com".to_owned(),
        "no-at-sign".to_owned(),
    ] {
        assert!(!role.accepts(&refused), "{refused}");
    }
    assert!(Role::Submission.accepts("anyone@example.com"));
}

#[test]
fn test_a_transaction_runs_greeting_to_data() {
    let mut session = receiver();
    assert_eq!(code(&session.step(mail("a@b", 0))), 503, "MAIL before HELO");
    assert_eq!(
        code(&session.step(Request::Ehlo {
            host: "client".to_owned()
        })),
        250
    );
    assert_eq!(code(&session.step(Request::Data)), 503, "DATA before MAIL");
    assert_eq!(code(&session.step(mail("a@b", 0))), 250);
    assert_eq!(code(&session.step(mail("a@b", 0))), 503, "nested MAIL");
    assert_eq!(code(&session.step(rcpt("you@example.com"))), 550);
    assert_eq!(
        code(&session.step(Request::Data)),
        503,
        "no valid recipient"
    );
    assert_eq!(
        code(&session.step(rcpt(&format!("me@{LABEL}.rings.rs")))),
        250
    );
    assert!(matches!(session.step(Request::Data), Next::ReadData(_)));
    let (transaction, reply) = session.data(b"Subject: hi\r\n\r\nbody".to_vec());
    assert_eq!(reply.code, 250);
    assert_eq!(
        transaction,
        Some(Transaction {
            client: "client".to_owned(),
            from: "a@b".to_owned(),
            to: vec![format!("me@{LABEL}.rings.rs")],
            message: b"Subject: hi\r\n\r\nbody".to_vec(),
        })
    );
    assert_eq!(
        code(&session.step(mail("c@d", 0))),
        250,
        "ready for the next transaction"
    );
}

#[test]
fn test_bounds_on_size_and_recipients_hold() {
    let mut session = Session::new(Role::Submission, "mx.test", 100, 2);
    session.step(Request::Helo {
        host: "client".to_owned(),
    });
    assert_eq!(code(&session.step(mail("a@b", 101))), 552);
    assert_eq!(code(&session.step(mail("a@b", 100))), 250);
    assert_eq!(code(&session.step(rcpt("1@x"))), 250);
    assert_eq!(code(&session.step(rcpt("2@x"))), 250);
    assert_eq!(code(&session.step(rcpt("3@x"))), 452);
    session.step(Request::Data);
    let (transaction, reply) = session.data(vec![0; 101]);
    assert_eq!((transaction, reply.code), (None, 552));
}

#[test]
fn test_rset_keeps_the_greeting_and_quit_closes() {
    let mut session = receiver();
    session.step(Request::Ehlo {
        host: "client".to_owned(),
    });
    session.step(mail("a@b", 0));
    assert_eq!(code(&session.step(Request::Rset)), 250);
    assert_eq!(
        code(&session.step(mail("a@b", 0))),
        250,
        "RSET keeps the greeting"
    );
    assert!(matches!(session.step(Request::Quit), Next::Close(_)));
}

#[test]
fn test_replies_encode_every_line_with_its_status() {
    let reply = Reply {
        code: 250,
        status: "2.0.0",
        lines: vec!["one".to_owned(), "two".to_owned()],
    };
    assert_eq!(reply.encode(), b"250-2.0.0 one\r\n250 2.0.0 two\r\n");
    let mut session = receiver();
    let Next::Reply(ehlo) = session.step(Request::Ehlo {
        host: "client".to_owned(),
    }) else {
        panic!("EHLO is answered");
    };
    let text = String::from_utf8(ehlo.encode()).unwrap();
    assert!(text.starts_with("250-mx.test greets client\r\n"), "{text}");
    assert!(text.ends_with("250 ENHANCEDSTATUSCODES\r\n"), "{text}");
}
