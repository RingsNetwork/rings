use rings_node::prelude::rings_core::dht::Did;
use rings_node::prelude::rings_core::ecc::SecretKey;
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

/// The code of `next`'s reply; a lookup has none yet.
fn code(next: &Next) -> u16 {
    match next {
        Next::Reply(reply) | Next::ReadData(reply) | Next::Close(reply) | Next::StartTls(reply) => {
            reply.code
        }
        Next::Lookup(_) => 0,
    }
}

/// EHLO from `client`.
fn ehlo() -> Request<String> {
    Request::Ehlo {
        host: "client".to_owned(),
    }
}

/// The lines of the EHLO reply of `session`.
fn ehlo_lines(session: &mut Session) -> Vec<String> {
    match session.step(ehlo()) {
        Next::Reply(reply) => reply.lines,
        next => panic!("EHLO is answered: {next:?}"),
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
        assert_eq!(role.accepts(&accepted), Some(true), "{accepted}");
    }
    for refused in [
        format!("you@{LABEL}.rings.rs"),
        format!("me@{LABEL}"),
        format!("me@x{LABEL}.rings.rs"),
        "me@example.com".to_owned(),
        "no-at-sign".to_owned(),
    ] {
        assert_eq!(role.accepts(&refused), Some(false), "{refused}");
    }
    assert_eq!(Role::Submission.accepts("anyone@example.com"), Some(true));
    assert_eq!(
        Role::Directory.accepts("anyone@example.com"),
        None,
        "the directory decides"
    );
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
            to: vec![Recipient {
                address: format!("me@{LABEL}.rings.rs"),
                mailbox: None,
            }],
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

/// A directory session asks its shell about each recipient, answers by the directory's word,
/// and keeps where an admitted recipient's mail goes for DATA.
#[test]
fn test_a_directory_session_asks_its_shell_about_each_recipient() {
    let mailbox = |did: u32| MailRecipient {
        did: Did::from(did),
        encryption_key: SecretKey::random().pubkey(),
    };
    let (d, e) = (mailbox(1), mailbox(2));
    let mut session = Session::new(Role::Directory, "gw.test", 100, 2);
    session.step(ehlo());
    assert_eq!(
        session.step(rcpt("d@a")),
        Next::Reply(send_mail_first()),
        "no lookup outside a transaction"
    );
    session.step(mail("a@b", 0));
    assert_eq!(session.step(rcpt("d@a")), Next::Lookup("d@a".to_owned()));
    for (lookup, reply) in [
        (Lookup::Refused, 550),
        (Lookup::Offline, 451),
        (Lookup::Unavailable, 451),
        (Lookup::Admitted(d), 250),
    ] {
        assert_eq!(session.admit("d@a".to_owned(), lookup).code, reply);
    }
    assert_eq!(
        session.admit("e@a".to_owned(), Lookup::Admitted(e)).code,
        250
    );
    assert_eq!(
        session
            .admit("f@a".to_owned(), Lookup::Admitted(mailbox(3)))
            .code,
        452,
        "the recipient bound holds for admitted recipients"
    );
    assert!(matches!(session.step(Request::Data), Next::ReadData(_)));
    let (transaction, _) = session.data(b"body".to_vec());
    let admitted = |address: &str, mailbox| Recipient {
        address: address.to_owned(),
        mailbox: Some(mailbox),
    };
    assert_eq!(
        transaction.map(|transaction| transaction.to),
        Some(vec![admitted("d@a", d), admitted("e@a", e)])
    );
}

#[test]
fn test_starttls_is_offered_only_with_a_certificate_and_restarts_the_session() {
    let mut plain = receiver();
    assert!(!ehlo_lines(&mut plain).contains(&"STARTTLS".to_owned()));
    assert_eq!(code(&plain.step(Request::StartTls)), 502);

    let mut session = receiver().offering_starttls();
    assert_eq!(
        code(&session.step(Request::StartTls)),
        503,
        "STARTTLS before EHLO"
    );
    assert!(ehlo_lines(&mut session).contains(&"STARTTLS".to_owned()));
    session.step(mail("a@b", 0));
    assert_eq!(
        code(&session.step(Request::StartTls)),
        503,
        "STARTTLS inside a transaction"
    );
    session.step(Request::Rset);
    assert!(matches!(session.step(Request::StartTls), Next::StartTls(reply) if reply.code == 220));
    session.secured();
    assert_eq!(
        code(&session.step(mail("a@b", 0))),
        503,
        "the client greets again under TLS"
    );
    assert!(!ehlo_lines(&mut session).contains(&"STARTTLS".to_owned()));
    assert_eq!(
        code(&session.step(Request::StartTls)),
        503,
        "TLS already active"
    );
}
