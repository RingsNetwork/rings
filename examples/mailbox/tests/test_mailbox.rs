//! The mailbox over real sockets: mail received over SMTP, read and managed over IMAP, and sent
//! from one mailbox to another through submission.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use std::sync::Arc;

use common::deliver;
use common::logged_in;
use common::mailbox;
use common::message;
use common::Client;
use common::FixedExchangers;
use common::DEADLINE;
use rings_mailbox_example::store::INBOX;

/// The owner label of the first mailbox.
const ALICE: &str = "0x00000000000000000000000000000000000a11ce";
/// The owner label of the second mailbox.
const BOB: &str = "0x0000000000000000000000000000000000000b0b";

/// Whether some line of `lines` contains `needle`.
fn any_contains(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

#[tokio::test]
async fn test_the_receiver_takes_mail_for_its_own_address_only() {
    let dir = tempfile::tempdir().unwrap();
    let running = mailbox(dir.path(), ALICE, Arc::new(FixedExchangers::default())).await;
    let mine = format!("me@{ALICE}.rings.rs");
    let elsewhere = format!("me@{ALICE}.p2p.example.com");
    let text = message("bob@example.com", &mine, "hello", "first");
    assert_eq!(
        deliver(running.smtp, "bob@example.com", &mine, &text).await,
        250
    );
    assert_eq!(
        deliver(running.smtp, "bob@example.com", &elsewhere, &text).await,
        250,
        "every zone that relays the owner delivers to the same mailbox"
    );
    for refused in [
        "you@example.com",
        &format!("you@{ALICE}.rings.rs"),
        &format!("me@{BOB}.rings.rs"),
    ] {
        assert_eq!(
            deliver(running.smtp, "bob@example.com", refused, &text).await,
            550,
            "{refused}"
        );
    }
    let (_, inbox) = running.store.mailbox(INBOX).await.unwrap();
    assert_eq!(inbox.messages.len(), 2);
}

#[tokio::test]
async fn test_imap_reads_flags_and_expunges_received_mail() {
    let dir = tempfile::tempdir().unwrap();
    let running = mailbox(dir.path(), ALICE, Arc::new(FixedExchangers::default())).await;
    let mine = format!("me@{ALICE}.rings.rs");
    let text = message(
        "bob@example.com",
        &mine,
        "greetings",
        "Hello from the overlay.",
    );
    assert_eq!(
        deliver(running.smtp, "bob@example.com", &mine, &text).await,
        250
    );

    let mut imap = logged_in(&running).await;
    let capability = imap.imap("a1", "CAPABILITY").await;
    for extension in ["IMAP4REV1", "IDLE", "UIDPLUS", "MOVE", "SPECIAL-USE"] {
        assert!(
            any_contains(&capability, extension),
            "{extension}: {capability:?}"
        );
    }
    let list = imap.imap("a2", "LIST \"\" \"*\"").await;
    assert!(any_contains(&list, "\\Sent"), "{list:?}");
    assert!(any_contains(&list, "INBOX"), "{list:?}");

    let select = imap.imap("a3", "SELECT INBOX").await;
    assert!(any_contains(&select, "* 1 EXISTS"), "{select:?}");
    assert!(any_contains(&select, "UIDVALIDITY"), "{select:?}");
    assert!(
        select.last().unwrap().contains("[READ-WRITE]"),
        "{select:?}"
    );

    let headers = imap
        .imap(
            "a4",
            "FETCH 1 (UID FLAGS ENVELOPE BODY.PEEK[HEADER.FIELDS (SUBJECT)])",
        )
        .await;
    assert!(any_contains(&headers, "Subject: greetings"), "{headers:?}");
    assert!(
        any_contains(&headers, "\"greetings\""),
        "the envelope: {headers:?}"
    );
    assert!(
        !any_contains(&headers, "\\Seen"),
        "a peek leaves the message unseen"
    );

    let body = imap.imap("a5", "FETCH 1 BODY[TEXT]").await;
    assert!(any_contains(&body, "Hello from the overlay."), "{body:?}");
    assert!(
        any_contains(&body, "\\Seen"),
        "reading marks the message seen: {body:?}"
    );

    let structure = imap.imap("a6", "FETCH 1 BODYSTRUCTURE").await;
    let structure_text = structure.join(" ").to_ascii_lowercase();
    assert!(
        structure_text.contains("(\"text\" \"plain\""),
        "{structure:?}"
    );

    let search = imap.imap("a7", "UID SEARCH SUBJECT greetings").await;
    assert!(any_contains(&search, "* SEARCH 1"), "{search:?}");

    let append = imap
        .imap("a8", "APPEND Sent (\\Seen) {12}\r\nSubject: x\r\n")
        .await;
    assert!(append.last().unwrap().contains("[APPENDUID"), "{append:?}");

    imap.imap("a9", "STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    let expunge = imap.imap("a10", "EXPUNGE").await;
    assert!(any_contains(&expunge, "* 1 EXPUNGE"), "{expunge:?}");
    let logout = imap.imap("a11", "LOGOUT").await;
    assert!(any_contains(&logout, "* BYE"), "{logout:?}");
}

#[tokio::test]
async fn test_a_wrong_password_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let running = mailbox(dir.path(), ALICE, Arc::new(FixedExchangers::default())).await;
    let mut imap = Client::connect(running.imap).await;
    imap.line().await;
    let login = imap.imap("a1", "LOGIN me wrong").await;
    assert!(login.last().unwrap().starts_with("a1 NO"), "{login:?}");
    let select = imap.imap("a2", "SELECT INBOX").await;
    assert!(select.last().unwrap().starts_with("a2 BAD"), "{select:?}");
}

#[tokio::test]
async fn test_an_idling_client_hears_of_new_mail() {
    let dir = tempfile::tempdir().unwrap();
    let running = mailbox(dir.path(), ALICE, Arc::new(FixedExchangers::default())).await;
    let mut imap = logged_in(&running).await;
    imap.imap("a1", "SELECT INBOX").await;
    imap.send("a2 IDLE\r\n").await;
    assert!(
        imap.line().await.starts_with('+'),
        "the server accepts IDLE"
    );
    let mine = format!("me@{ALICE}.rings.rs");
    let text = message("bob@example.com", &mine, "news", "while you idled");
    assert_eq!(
        deliver(running.smtp, "bob@example.com", &mine, &text).await,
        250
    );
    assert_eq!(imap.line().await, "* 1 EXISTS");
    imap.send("DONE\r\n").await;
    let done = imap.until_tagged("a2").await;
    assert!(done.last().unwrap().starts_with("a2 OK"), "{done:?}");
}

#[tokio::test]
async fn test_one_mailbox_writes_to_another_through_submission() {
    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let exchangers = Arc::new(FixedExchangers::default());
    let alice = mailbox(alice_dir.path(), ALICE, exchangers.clone()).await;
    let bob = mailbox(bob_dir.path(), BOB, exchangers.clone()).await;
    let bob_zone = format!("{BOB}.rings.rs");
    exchangers
        .0
        .lock()
        .unwrap()
        .insert(bob_zone.clone(), bob.smtp);

    let from = format!("me@{ALICE}.rings.rs");
    let to = format!("me@{bob_zone}");
    let text = message(&from, &to, "over", "From one node to another.");
    assert_eq!(deliver(alice.submission, &from, &to, &text).await, 250);

    let mut watch = bob.store.watch();
    tokio::time::timeout(DEADLINE, async {
        loop {
            let (_, inbox) = bob.store.mailbox(INBOX).await.unwrap();
            if !inbox.messages.is_empty() {
                return;
            }
            let _ = watch.recv().await;
        }
    })
    .await
    .expect("the message reaches the other mailbox");
}

#[tokio::test]
async fn test_a_refused_recipient_bounces_to_the_senders_inbox() {
    let alice_dir = tempfile::tempdir().unwrap();
    let bob_dir = tempfile::tempdir().unwrap();
    let exchangers = Arc::new(FixedExchangers::default());
    let alice = mailbox(alice_dir.path(), ALICE, exchangers.clone()).await;
    let bob = mailbox(bob_dir.path(), BOB, exchangers.clone()).await;
    let bob_zone = format!("{BOB}.rings.rs");
    exchangers
        .0
        .lock()
        .unwrap()
        .insert(bob_zone.clone(), bob.smtp);

    let from = format!("me@{ALICE}.rings.rs");
    let to = format!("nobody@{bob_zone}");
    let text = message(&from, &to, "lost", "To no one.");
    assert_eq!(deliver(alice.submission, &from, &to, &text).await, 250);

    let mut watch = alice.store.watch();
    let bounce = tokio::time::timeout(DEADLINE, async {
        loop {
            let (listed, inbox) = alice.store.mailbox(INBOX).await.unwrap();
            if let Some(message) = inbox.messages.first() {
                return alice.store.read(listed.id, message.uid).await.unwrap();
            }
            let _ = watch.recv().await;
        }
    })
    .await
    .expect("the bounce arrives");
    let bounce = String::from_utf8_lossy(&bounce);
    assert!(
        bounce.contains("Undelivered Mail Returned to Sender"),
        "{bounce}"
    );
    assert!(bounce.contains(&to), "{bounce}");
}
