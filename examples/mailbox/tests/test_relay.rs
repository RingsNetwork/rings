//! The whole path, end to end: a mail server on the internet resolves `me@<did>.<zone>` through
//! a DNS relay, connects to the owner's IPv6 address, and the relay carries the connection over
//! the Rings overlay to the owner's node, which hands it to its mailbox.
//!
//! ```text
//!   test (mail server) ─DNS─▶ node A: DNS relay           ─▶ AAAA addr(B), MX <B>.rings.rs
//!   test (mail server) ─TCP─▶ node A: ingress (addr(B), P) ─TCP relay─▶ node B ─▶ mailbox
//! ```
//!
//! Like the relay example's tests, it needs a WebRTC link between two in-process nodes; where
//! the link cannot form (no UDP), it skips with a notice instead of failing.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod common;

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::deliver;
use common::mailbox;
use common::message;
use common::FixedExchangers;
use hickory_proto::op::Message as DnsMessage;
use hickory_proto::op::Query;
use hickory_proto::rr::rdata::AAAA;
use hickory_proto::rr::rdata::MX;
use hickory_proto::rr::Name;
use hickory_proto::rr::RData;
use hickory_proto::rr::RecordType;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::entry::Entry;
use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;
use rings_core::swarm::OfferReply;
use rings_dns_relay::AddressRegistry;
use rings_dns_relay::AdmissionLimits;
use rings_dns_relay::AnswerTtl;
use rings_dns_relay::DnsAuthority;
use rings_dns_relay::Ingress;
use rings_dns_relay::Remembering;
use rings_dns_relay::Zone;
use rings_dns_relay::Zones;
use rings_mailbox_example::store::INBOX;
use rings_node::dns_relay::relay_service;
use rings_node::dns_relay::service_topic;
use rings_node::dns_relay::DhtDirectory;
use rings_node::dns_relay::RelayForward;
use rings_node::dns_relay::ServiceOffer;
use rings_node::extension::protocols::relay::RelayHandle;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use tokio::net::TcpListener;
use tokio::net::UdpSocket;

/// How long the overlay may take to link the two nodes before the test skips.
const CONNECT_BUDGET: Duration = Duration::from_secs(45);
/// How long the owner's descriptor may take to reach the relay's view of the DHT.
const PUBLISH_BUDGET: Duration = Duration::from_secs(30);

/// A node, optionally announcing `offer`, with its relay installed and its loop running.
async fn node(offer: Option<ServiceOffer>) -> (Arc<Processor>, RelayHandle) {
    let key = SecretKey::random();
    let delegatee_key = DelegateeKey::new_with_seckey(&key).unwrap();
    let config = ProcessorConfig::new(
        0,
        "stun://stun.l.google.com:19302".to_owned(),
        delegatee_key,
        3,
    );
    let builder = ProcessorBuilder::from_config(&config)
        .unwrap()
        .storage(Box::new(MemStorage::new()));
    let builder = match offer {
        Some(offer) => builder.announce_services(offer),
        None => builder,
    };
    let processor = Arc::new(builder.build().unwrap());
    let provider = Arc::new(Provider::from_processor(processor.clone()));
    let relay = RelayHandle::install(&provider.extensions()).unwrap();
    provider.set_backend().unwrap();
    let listening = processor.clone();
    tokio::spawn(async move { listening.listen().await });
    (processor, relay)
}

/// Link `a` and `b` over WebRTC; `false` when the link does not form in time.
async fn link(a: &Arc<Processor>, b: &Arc<Processor>) -> bool {
    let linked = async {
        let offer = a.swarm.create_offer(b.swarm.did()).await.ok()?;
        match b.swarm.answer_offer(offer).await.ok()? {
            OfferReply::Answer(answer) => a.swarm.accept_answer(answer).await.ok()?,
            OfferReply::CounterOffer(counter) => {
                let answer = a.swarm.answer_offer(counter).await.ok()?.answer()?;
                b.swarm.accept_answer(answer).await.ok()?;
            }
        }
        let peer = b.swarm.did().to_string();
        loop {
            let connected = a
                .swarm
                .peers()
                .iter()
                .any(|p| p.did == peer && p.state.to_lowercase().contains("connected"));
            if connected {
                return Some(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    matches!(
        tokio::time::timeout(CONNECT_BUDGET, linked).await,
        Ok(Some(()))
    )
}

/// The absolute name `name`.
fn name(name: &str) -> Name {
    let mut name = Name::from_ascii(name).unwrap();
    name.set_fqdn(true);
    name
}

/// Ask the DNS server at `server` for `qname` of type `qtype`.
async fn ask(server: SocketAddr, qname: &str, qtype: RecordType) -> DnsMessage {
    let socket = UdpSocket::bind("[::1]:0").await.unwrap();
    let mut query = DnsMessage::query();
    query.metadata.id = 1;
    query.add_query(Query::query(name(qname), qtype));
    socket
        .send_to(&query.to_vec().unwrap(), server)
        .await
        .unwrap();
    let mut buffer = vec![0u8; 4096];
    let (len, _) = tokio::time::timeout(Duration::from_secs(10), socket.recv_from(&mut buffer))
        .await
        .expect("the relay answers")
        .unwrap();
    DnsMessage::from_vec(&buffer[..len]).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mail_reaches_the_owners_mailbox_through_the_dns_relay() {
    // The ingress listener comes first: the owner announces the port it got. Binding port 25
    // needs privileges a test does not have, so the owner announces 25, which earns its name an
    // MX record, and the unprivileged port the mail is delivered to in its stead.
    let ingress = TcpListener::bind("[::1]:0").await.unwrap();
    let port = ingress.local_addr().unwrap().port();
    let offer = ServiceOffer::new(
        BTreeSet::from([25, port]),
        Vec::new(),
        Duration::from_secs(1),
        Duration::from_secs(3),
    )
    .unwrap();
    let (relay_node, relay_handle) = node(None).await;
    let (owner_node, owner_handle) = node(Some(offer)).await;
    if !link(&relay_node, &owner_node).await {
        eprintln!("SKIP: the overlay link between the two nodes did not form");
        return;
    }

    // The owner's node serves its mailbox's receiver as the announced port.
    let owner = owner_node.swarm.did().to_string();
    let mailbox_dir = tempfile::tempdir().unwrap();
    let mailbox = mailbox(
        mailbox_dir.path(),
        &owner,
        Arc::new(FixedExchangers::default()),
    )
    .await;
    for served in [25, port] {
        owner_handle
            .register_tcp_service(relay_service(served), mailbox.smtp)
            .await
            .unwrap();
    }

    // Wait until the owner's heartbeat has put its descriptor where the relay reads it.
    let key = Entry::gen_did(&service_topic(owner_node.swarm.did())).unwrap();
    let published = tokio::time::timeout(PUBLISH_BUDGET, async {
        loop {
            let _ = relay_node.storage_fetch(key).await;
            if relay_node
                .storage_check_cache(key)
                .await
                .is_some_and(|entry| !entry.data.is_empty())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    assert!(
        published.is_ok(),
        "the owner's descriptor reaches the relay's DHT view"
    );

    // The relay: DNS for rings.rs, every owner on ::1 (a /128 holds one owner), and the
    // ingress on the announced port.
    let state = tempfile::tempdir().unwrap();
    let registry = Arc::new(
        AddressRegistry::open(
            "::1/128".parse().unwrap(),
            state.path(),
            Duration::from_secs(600),
        )
        .await
        .unwrap(),
    );
    let directory = Arc::new(
        Remembering::open(
            Arc::new(DhtDirectory::new(relay_node.clone())),
            state.path(),
            Duration::from_secs(600),
        )
        .await
        .unwrap(),
    );
    let zones = Zones::new(vec![Zone::new(
        name("rings.rs"),
        vec![name("ns.example.net")],
        name("hostmaster.example.net"),
    )])
    .unwrap();
    let authority = DnsAuthority::new(
        Arc::new(zones),
        AnswerTtl {
            positive: 60,
            negative: 60,
        },
        Arc::clone(&registry),
        Arc::clone(&directory),
    );
    let dns_socket = UdpSocket::bind("[::1]:0").await.unwrap();
    let dns = dns_socket.local_addr().unwrap();
    let mut dns_server = hickory_server::Server::new(authority);
    dns_server.register_socket(dns_socket);
    let _ingress_tasks = Ingress::new(
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        BTreeSet::new(),
        registry,
        directory,
        Arc::new(RelayForward::new(relay_handle, relay_node.swarm.did(), [])),
        AdmissionLimits::default(),
    )
    .serve(vec![ingress]);

    // A mail server resolves the owner's domain...
    let domain = format!("{owner}.rings.rs");
    let mx = ask(dns, &domain, RecordType::MX).await;
    assert_eq!(
        mx.answers
            .iter()
            .map(|r| r.data.clone())
            .collect::<Vec<_>>(),
        vec![RData::MX(MX::new(10, name(&domain)))],
        "the owner serves SMTP, so its name is its own mail exchanger"
    );
    let aaaa = ask(dns, &domain, RecordType::AAAA).await;
    let [record] = aaaa.answers.as_slice() else {
        panic!("one address: {aaaa:?}");
    };
    let RData::AAAA(AAAA(address)) = record.data else {
        panic!("an AAAA record: {record:?}");
    };
    assert_eq!(address, Ipv6Addr::LOCALHOST);

    // ...and delivers to it: the relay carries the connection to the owner's mailbox.
    let to = format!("me@{domain}");
    let text = message(
        "alice@example.com",
        &to,
        "through",
        "Delivered over the overlay.",
    );
    let code = deliver(
        SocketAddr::new(IpAddr::V6(address), port),
        "alice@example.com",
        &to,
        &text,
    )
    .await;
    assert_eq!(code, 250);
    let (listed, inbox) = mailbox.store.mailbox(INBOX).await.unwrap();
    let delivered = inbox
        .messages
        .first()
        .expect("the message is in the owner's inbox");
    let bytes = mailbox.store.read(listed.id, delivered.uid).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("Delivered over the overlay."));
    drop(dns_server);
}
