//! The mail gateway's SMTP front over a stand-in DHT, on the loopback: RCPT is admitted only for
//! a DID with a live descriptor (550 otherwise), a mail is sealed, written once per DID, and
//! opens into its recipient's INBOX with the trace a receiver adds, a mail too large for one
//! DHT write is refused with 552, one no owner confirms is deferred with 451, and a client may
//! run the whole session under STARTTLS.
//!
//! ```text
//!   test (mail server) ─SMTP─▶ gateway front ─MailDrop─▶ stand-in DHT ─open─▶ recipient INBOX
//! ```
//!
//! The DHT side itself, the descriptor's lookup, the write and the recipient's poll over the
//! Rings overlay, is the node's own test (`rings-node`, `processor::tests::test_mail`).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use common::message;
use common::Client;
use rand::rngs::OsRng;
use rings_mailbox_example::gateway::Gateway;
use rings_mailbox_example::outbound::deliver;
use rings_mailbox_example::outbound::Delivery;
use rings_mailbox_example::receive::StoreInbox;
use rings_mailbox_example::smtp::tls;
use rings_mailbox_example::store::Store;
use rings_mailbox_example::store::INBOX;
use rings_node::mail::descriptor::MailRecipient;
use rings_node::mail::envelope::seal;
use rings_node::mail::envelope::MailDigest;
use rings_node::mail::envelope::MAIL_MESSAGE_MAX;
use rings_node::mail::gateway::MailDrop;
use rings_node::mail::gateway::MailGatewayError;
use rings_node::mail::open::open_mail;
use rings_node::mail::open::Opening;
use rings_node::mail::recipient::MailInbox;
use rings_node::prelude::rings_core::delegation::DelegateeKey;
use rings_node::prelude::rings_core::dht::Did;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::rings_core::message::MessageSigner;
use rings_node::prelude::rings_core::swarm::storage_api::StoreUnconfirmed;
use rings_node::prelude::rings_core::utils::get_epoch_ms;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The overlay the stand-in signs manifests for.
const NETWORK: u32 = 1;

/// The verdict on a write that no owner confirms, for its recipient.
type Unconfirmed = fn(Did) -> MailGatewayError;

/// A DHT in memory: the live descriptors' recipients, every recipient's mail topic, and the
/// verdict on each write when no owner confirms it.
struct StandIn {
    gateway: DelegateeKey,
    recipients: BTreeMap<Did, MailRecipient>,
    topics: Mutex<BTreeMap<Did, Vec<Bytes>>>,
    unconfirmed: Option<Unconfirmed>,
}

impl StandIn {
    /// A DHT where exactly the accounts of `keys` have live descriptors, and an owner confirms
    /// every write.
    fn with(keys: &[&DelegateeKey]) -> Arc<Self> {
        Self::confirming(keys, None)
    }

    /// A DHT where exactly the accounts of `keys` have live descriptors, and every write ends in
    /// `unconfirmed`, if given, after it is written.
    fn confirming(keys: &[&DelegateeKey], unconfirmed: Option<Unconfirmed>) -> Arc<Self> {
        let recipients = keys
            .iter()
            .map(|key| {
                (key.delegator_did(), MailRecipient {
                    did: key.delegator_did(),
                    encryption_key: key.delegatee_public_key(),
                })
            })
            .collect();
        Arc::new(Self {
            gateway: delegatee(),
            recipients,
            topics: Mutex::new(BTreeMap::new()),
            unconfirmed,
        })
    }

    /// The elements written to `did`'s mail topic.
    fn topic(&self, did: Did) -> Vec<Bytes> {
        self.topics
            .lock()
            .unwrap()
            .get(&did)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl MailDrop for StandIn {
    async fn recipient(&self, did: Did) -> Result<Option<MailRecipient>, MailGatewayError> {
        Ok(self.recipients.get(&did).copied())
    }

    async fn deliver(
        &self,
        recipient: &MailRecipient,
        message: &[u8],
    ) -> Result<MailDigest, MailGatewayError> {
        let signer = MessageSigner::new(&self.gateway, NETWORK);
        let sealed = seal(message, recipient, signer, get_epoch_ms(), &mut OsRng)?;
        self.topics
            .lock()
            .unwrap()
            .entry(recipient.did)
            .or_default()
            .extend(sealed.elements);
        match self.unconfirmed {
            Some(unconfirmed) => Err(unconfirmed(recipient.did)),
            None => Ok(sealed.mail_id),
        }
    }
}

/// A fresh delegatee key of a fresh account.
fn delegatee() -> DelegateeKey {
    DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap()
}

/// The gateway over `dht`, listening on a loopback port, offering STARTTLS with `tls` if given.
async fn gateway(dht: Arc<StandIn>, tls: Option<TlsAcceptor>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = Arc::new(Gateway::new(dht)).smtp_service("gw.test".to_owned(), tls);
    tokio::spawn(service.serve(listener));
    address
}

/// A client greeted by the gateway at `address` and through EHLO; the EHLO reply's lines.
async fn greeted(address: SocketAddr) -> (Client, Vec<String>) {
    let mut client = Client::connect(address).await;
    assert!(client.smtp_reply().await.last().unwrap().starts_with("220"));
    client.send("EHLO mx.test\r\n").await;
    let ehlo = client.smtp_reply().await;
    (client, ehlo)
}

#[tokio::test]
async fn test_rcpt_for_a_did_without_a_live_descriptor_is_refused_with_550() {
    let recipient = delegatee();
    let address = gateway(StandIn::with(&[&recipient]), None).await;
    let (mut client, _) = greeted(address).await;
    assert_eq!(client.smtp("MAIL FROM:<alice@example.org>").await, 250);
    let stranger = delegatee().delegator_did();
    assert_eq!(
        client
            .smtp(&format!("RCPT TO:<{stranger}@p2p.rings.rs>"))
            .await,
        550
    );
    assert_eq!(
        client.smtp("RCPT TO:<me@p2p.rings.rs>").await,
        550,
        "not a DID"
    );
    let did = recipient.delegator_did();
    assert_eq!(
        client.smtp(&format!("RCPT TO:<{did}@p2p.rings.rs>")).await,
        250
    );
}

#[tokio::test]
async fn test_a_mail_through_the_gateway_opens_into_its_recipient_inbox() {
    let recipient = delegatee();
    let dht = StandIn::with(&[&recipient]);
    let address = gateway(Arc::clone(&dht), None).await;
    let did = recipient.delegator_did();
    let shouting = format!(
        "0x{}",
        did.to_string().trim_start_matches("0x").to_uppercase()
    );
    let (mut client, _) = greeted(address).await;
    assert_eq!(client.smtp("MAIL FROM:<alice@example.org>").await, 250);
    // One DID at two domains is one mailbox: the mail is written once.
    assert_eq!(
        client.smtp(&format!("RCPT TO:<{did}@a.example>")).await,
        250
    );
    assert_eq!(
        client
            .smtp(&format!("RCPT TO:<{shouting}@b.example>"))
            .await,
        250
    );
    assert_eq!(client.smtp("DATA").await, 354);
    let text = message(
        "alice@example.org",
        &format!("{did}@a.example"),
        "hello",
        "Hi.",
    );
    assert_eq!(client.smtp(&format!("{text}\r\n.")).await, 250);
    assert_eq!(client.smtp("QUIT").await, 221);

    let elements = dht.topic(did);
    let openings = open_mail(
        &elements,
        did,
        &[recipient],
        NETWORK,
        get_epoch_ms(),
        |_| true,
    );
    let [Opening::Opened(mail)] = openings.as_slice() else {
        panic!("one mail opens: {openings:?}");
    };
    assert_eq!(mail.message_id().as_deref(), Some("<hello@test>"));
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), 1024 * 1024).await.unwrap());
    StoreInbox(Arc::clone(&store)).file(mail).await.unwrap();
    let (listed, inbox) = store.mailbox(INBOX).await.unwrap();
    let filed = inbox.messages.first().expect("the mail is filed");
    let filed = String::from_utf8(store.read(listed.id, filed.uid).await.unwrap()).unwrap();
    assert!(filed.starts_with("Return-Path: <alice@example.org>\r\nReceived: from mx.test (["));
    assert!(filed.contains(" by gw.test with ESMTP; "), "{filed}");
    assert!(filed.ends_with(&format!("{text}\r\n")), "{filed}");
}

#[tokio::test]
async fn test_a_message_above_the_mail_bound_is_refused_with_552() {
    let recipient = delegatee();
    let dht = StandIn::with(&[&recipient]);
    let address = gateway(Arc::clone(&dht), None).await;
    let did = recipient.delegator_did();
    let (mut client, ehlo) = greeted(address).await;
    assert!(ehlo
        .iter()
        .any(|line| line.ends_with(&format!("SIZE {MAIL_MESSAGE_MAX}"))));
    let above = MAIL_MESSAGE_MAX + 1;
    assert_eq!(
        client
            .smtp(&format!("MAIL FROM:<alice@example.org> SIZE={above}"))
            .await,
        552
    );
    // A message at the bound, with the trace the gateway adds, needs one chunk too many.
    assert_eq!(client.smtp("MAIL FROM:<alice@example.org>").await, 250);
    assert_eq!(
        client.smtp(&format!("RCPT TO:<{did}@a.example>")).await,
        250
    );
    assert_eq!(client.smtp("DATA").await, 354);
    let body = "x".repeat(MAIL_MESSAGE_MAX - 2);
    assert_eq!(client.smtp(&format!("{body}\r\n.")).await, 552);
    assert!(dht.topic(did).is_empty(), "nothing is written");
}

/// A mail no owner reported holding, every one having refused it or none having reported in
/// time, is deferred with `451`: its sender retries, and a copy an owner did hold is a
/// duplicate its recipient files once.
#[tokio::test]
async fn test_a_mail_no_owner_confirms_is_deferred_with_451() {
    let verdicts: [Unconfirmed; 2] = [
        |did| MailGatewayError::Unreported { did },
        |did| MailGatewayError::Unstored {
            did,
            cause: StoreUnconfirmed::Refused { placement: did },
        },
    ];
    for unconfirmed in verdicts {
        let recipient = delegatee();
        let address = gateway(StandIn::confirming(&[&recipient], Some(unconfirmed)), None).await;
        let did = recipient.delegator_did();
        let (mut client, _) = greeted(address).await;
        assert_eq!(client.smtp("MAIL FROM:<alice@example.org>").await, 250);
        assert_eq!(
            client.smtp(&format!("RCPT TO:<{did}@a.example>")).await,
            250
        );
        assert_eq!(client.smtp("DATA").await, 354);
        let text = message(
            "alice@example.org",
            &format!("{did}@a.example"),
            "late",
            "Hi.",
        );
        assert_eq!(client.smtp(&format!("{text}\r\n.")).await, 451);
    }
}

#[tokio::test]
async fn test_a_session_under_starttls_delivers_through_the_gateway() {
    let certified = rcgen::generate_simple_self_signed(vec!["gw.test".to_owned()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (certificate, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&certificate, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();
    let acceptor = tls::acceptor(&certificate, &key).unwrap();

    let recipient = delegatee();
    let dht = StandIn::with(&[&recipient]);
    let address = gateway(Arc::clone(&dht), Some(acceptor)).await;
    let (_, ehlo) = greeted(address).await;
    assert!(
        ehlo.iter().any(|line| line.ends_with("STARTTLS")),
        "{ehlo:?}"
    );

    // The example's client takes STARTTLS whenever it is offered, then greets again and runs the
    // transaction under TLS.
    let did = recipient.delegator_did();
    let to = format!("{did}@a.example");
    let text = message("alice@example.org", &to, "secured", "Hi.");
    deliver(address, Delivery {
        helo: "mx.test",
        from: "alice@example.org",
        recipient: &to,
        message: text.as_bytes(),
    })
    .await
    .unwrap();
    let openings = open_mail(
        &dht.topic(did),
        did,
        &[recipient],
        NETWORK,
        get_epoch_ms(),
        |_| true,
    );
    assert!(matches!(
        openings.as_slice(),
        [Opening::Opened(mail)] if mail.message_id().as_deref() == Some("<secured@test>")
    ));
}
