//! The relay in front of the mailbox, end to end on the loopback: a mail server resolves
//! `me@<owner>.<zone>` through the relay's DNS, connects to the owner's IPv6 address, and the
//! ingress hands the connection to the mailbox's receiver behind a PROXY header, so the
//! `Received` header names the mail server, not the relay.
//!
//! ```text
//!   test (mail server) ─DNS─▶ relay: DNS authority         ─▶ AAAA addr(o1), MX o1.rings.rs
//!   test (mail server) ─TCP─▶ relay: ingress (addr(o1), P) ─PROXY header, splice─▶ mailbox
//! ```
//!
//! The hop between a relay and the owner's node, over the Rings overlay, is the node's own test
//! (`rings-node`, `processor::tests::test_dns_relay`); here the forward splices on the loopback.

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

use async_trait::async_trait;
use common::config;
use common::deliver;
use common::message;
use common::FixedExchangers;
use hickory_proto::op::Message as DnsMessage;
use hickory_proto::op::Query;
use hickory_proto::rr::rdata::AAAA;
use hickory_proto::rr::rdata::MX;
use hickory_proto::rr::Name;
use hickory_proto::rr::RData;
use hickory_proto::rr::RecordType;
use hickory_server::Server;
use rings_dns_relay::AddressRegistry;
use rings_dns_relay::AdmissionLimits;
use rings_dns_relay::Announcement;
use rings_dns_relay::AnswerTtl;
use rings_dns_relay::Connection;
use rings_dns_relay::Directory;
use rings_dns_relay::DnsAuthority;
use rings_dns_relay::Forward;
use rings_dns_relay::Ingress;
use rings_dns_relay::Owner;
use rings_dns_relay::ProxyProtocol;
use rings_dns_relay::Zone;
use rings_dns_relay::Zones;
use rings_mailbox_example::mailbox::start;
use rings_mailbox_example::store::INBOX;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use tokio::task::JoinSet;

/// The one owner of the case, named `o1`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct One;

impl Owner for One {
    fn from_label(label: &str) -> Option<Self> {
        (label == "o1").then_some(Self)
    }

    fn label(&self) -> String {
        "o1".to_owned()
    }
}

/// The directory failure of the case; it never happens.
#[derive(Debug, thiserror::Error)]
#[error("unreachable")]
struct Unreachable;

/// A directory where `o1` serves port 25, which earns its name an MX record, and the
/// unprivileged port `port` the mail is delivered to in its stead, both behind a PROXY header.
struct Serving {
    port: u16,
}

#[async_trait]
impl Directory for Serving {
    type Error = Unreachable;
    type Owner = One;

    async fn announcement(&self, _owner: One) -> Result<Option<Announcement>, Unreachable> {
        let ports = [(25, ProxyProtocol::V2), (self.port, ProxyProtocol::V2)];
        Ok(Some(Announcement::new(ports, []).unwrap()))
    }
}

/// A forward that splices every connection with the mailbox's receiver, its preamble first.
struct Splice {
    receiver: SocketAddr,
}

#[async_trait]
impl Forward for Splice {
    type Error = std::io::Error;
    type Owner = One;

    async fn forward(&self, connection: Connection<One>) -> std::io::Result<()> {
        let mut upstream = TcpStream::connect(self.receiver).await?;
        upstream.write_all(&connection.preamble).await?;
        let mut client = connection.stream;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        Ok(())
    }
}

/// The absolute name `name`.
fn name(name: &str) -> Name {
    let mut name = Name::from_ascii(name).unwrap();
    name.set_fqdn(true);
    name
}

/// A running relay for `rings.rs.`: its DNS server, the address it answers on, its ingress, and
/// its state directory.
struct Relay {
    _dns: Server<DnsAuthority<Serving>>,
    dns: SocketAddr,
    _ingress: JoinSet<()>,
    _state: tempfile::TempDir,
}

/// The relay of the case, every owner on `::1`, its ingress on `ingress`, splicing to
/// `receiver`.
async fn relay(ingress: TcpListener, receiver: SocketAddr) -> Relay {
    let state = tempfile::tempdir().unwrap();
    let retention = Duration::from_secs(600);
    let prefix = "::1/128".parse().unwrap();
    let registry = Arc::new(
        AddressRegistry::open(prefix, state.path(), retention, 8)
            .await
            .unwrap(),
    );
    let port = ingress.local_addr().unwrap().port();
    let directory = Arc::new(Serving { port });
    let zone = Zone::new(
        name("rings.rs"),
        name("ns.example.net"),
        vec![],
        name("hostmaster.example.net"),
    );
    let ttl = AnswerTtl {
        positive: 60,
        negative: 60,
    };
    let zones = Arc::new(Zones::new(vec![zone]).unwrap());
    let authority = DnsAuthority::new(zones, ttl, Arc::clone(&registry), Arc::clone(&directory));
    let socket = UdpSocket::bind("[::1]:0").await.unwrap();
    let dns = socket.local_addr().unwrap();
    let mut server = Server::new(authority);
    server.register_socket(socket);
    let forward = Arc::new(Splice { receiver });
    let limits = AdmissionLimits::default();
    let ingress = Ingress::new(
        Ipv6Addr::LOCALHOST,
        BTreeSet::new(),
        registry,
        directory,
        forward,
        limits,
    )
    .serve(vec![ingress]);
    Relay {
        _dns: server,
        dns,
        _ingress: ingress,
        _state: state,
    }
}

/// The records of type `qtype` the DNS server at `server` answers for `qname`.
async fn ask(server: SocketAddr, qname: &str, qtype: RecordType) -> Vec<RData> {
    let socket = UdpSocket::bind("[::1]:0").await.unwrap();
    let mut query = DnsMessage::query();
    query.metadata.id = 1;
    query.add_query(Query::query(name(qname), qtype));
    let bytes = query.to_vec().unwrap();
    socket.send_to(&bytes, server).await.unwrap();
    let mut buffer = vec![0u8; 4096];
    let (len, _) = tokio::time::timeout(common::DEADLINE, socket.recv_from(&mut buffer))
        .await
        .expect("the relay answers")
        .unwrap();
    let answer = DnsMessage::from_vec(&buffer[..len]).unwrap();
    answer
        .answers
        .iter()
        .map(|record| record.data.clone())
        .collect()
}

#[tokio::test]
async fn test_mail_reaches_the_mailbox_through_the_relay_with_its_client_named() {
    let mailbox_dir = tempfile::tempdir().unwrap();
    let mut configured = config(mailbox_dir.path(), "o1");
    configured.receiver_proxy_protocol = true;
    let mailbox = start(&configured, Arc::new(FixedExchangers::default()))
        .await
        .unwrap();
    let ingress = TcpListener::bind("[::1]:0").await.unwrap();
    let port = ingress.local_addr().unwrap().port();
    let relay = relay(ingress, mailbox.smtp).await;

    // A mail server resolves the owner's domain...
    let domain = "o1.rings.rs";
    let mx = ask(relay.dns, domain, RecordType::MX).await;
    assert_eq!(mx, vec![RData::MX(MX::new(10, name(domain)))]);
    let aaaa = ask(relay.dns, domain, RecordType::AAAA).await;
    assert_eq!(aaaa, vec![RData::AAAA(AAAA(Ipv6Addr::LOCALHOST))]);

    // ...and delivers to it, as `test.example`, from `::1`.
    let to = format!("me@{domain}");
    let text = message(
        "alice@example.com",
        &to,
        "through",
        "Delivered by the relay.",
    );
    let at = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
    assert_eq!(deliver(at, "alice@example.com", &to, &text).await, 250);
    let (listed, inbox) = mailbox.store.mailbox(INBOX).await.unwrap();
    let delivered = inbox.messages.first().expect("the message is in the inbox");
    let bytes = mailbox.store.read(listed.id, delivered.uid).await.unwrap();
    let bytes = String::from_utf8_lossy(&bytes);
    assert!(bytes.contains("Delivered by the relay."), "{bytes}");
    assert!(
        bytes.contains("Received: from test.example ([IPv6:::1])"),
        "the receiver names the client the PROXY header named: {bytes}"
    );
}
