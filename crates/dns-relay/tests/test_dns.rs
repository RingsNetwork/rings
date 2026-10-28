//! The DNS authority over real sockets: a resolver's queries for a relayed zone, answered from a
//! fixed directory by the authority alone, and by a whole relay listening where its default
//! configuration does.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hickory_proto::op::Edns;
use hickory_proto::op::Message;
use hickory_proto::op::Query;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::AAAA;
use hickory_proto::rr::rdata::MX;
use hickory_proto::rr::Name;
use hickory_proto::rr::RData;
use hickory_proto::rr::RecordType;
use hickory_server::Server;
use rings_dns_relay::AddressRegistry;
use rings_dns_relay::Announcement;
use rings_dns_relay::AnswerTtl;
use rings_dns_relay::Connection;
use rings_dns_relay::Directory;
use rings_dns_relay::DnsAuthority;
use rings_dns_relay::DnsRelay;
use rings_dns_relay::DnsRelayConfig;
use rings_dns_relay::Forward;
use rings_dns_relay::Owner;
use rings_dns_relay::ProxyProtocol;
use rings_dns_relay::Zone;
use rings_dns_relay::Zones;
use rings_dns_relay::SMTP_PORT;
use socket2::Domain;
use socket2::Protocol;
use socket2::Socket;
use socket2::Type;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;

/// An owner named `o<n>`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Numbered(u32);

impl Owner for Numbered {
    fn from_label(label: &str) -> Option<Self> {
        let owner = Self(label.strip_prefix('o')?.parse().ok()?);
        (owner.label() == label).then_some(owner)
    }

    fn label(&self) -> String {
        format!("o{}", self.0)
    }
}

/// A directory where `o1` receives mail, `o2` serves SSH only, and `o3`'s lookups fail.
struct Fixed(BTreeMap<Numbered, Announcement>);

/// The failure of `o3`'s lookups.
#[derive(Debug, thiserror::Error)]
#[error("the DHT is unreachable")]
struct Unreachable;

#[async_trait]
impl Directory for Fixed {
    type Error = Unreachable;
    type Owner = Numbered;

    async fn announcement(&self, owner: Numbered) -> Result<Option<Announcement>, Unreachable> {
        if owner == Numbered(3) {
            return Err(Unreachable);
        }
        Ok(self.0.get(&owner).cloned())
    }
}

impl Fixed {
    /// The directory the cases share.
    fn new() -> Self {
        Self(BTreeMap::from([
            (
                Numbered(1),
                Announcement::new([(SMTP_PORT, ProxyProtocol::Off)], []).unwrap(),
            ),
            (
                Numbered(2),
                Announcement::new([(22, ProxyProtocol::Off)], []).unwrap(),
            ),
        ]))
    }
}

/// A forward for a relay whose ingress no case connects to.
struct Unused;

#[async_trait]
impl Forward for Unused {
    type Error = std::io::Error;
    type Owner = Numbered;

    async fn forward(&self, _connection: Connection<Numbered>) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// The absolute name `name`.
fn name(name: &str) -> Name {
    let mut name = Name::from_ascii(name).unwrap();
    name.set_fqdn(true);
    name
}

/// A running relay: the server answering while it lives, its address, its registry and its
/// state directory.
struct Relay {
    _server: Server<DnsAuthority<Fixed>>,
    address: SocketAddr,
    registry: Arc<AddressRegistry<Numbered>>,
    _dir: tempfile::TempDir,
}

/// A relay for `rings.rs.` answering on a loopback UDP socket.
async fn relay() -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(
        AddressRegistry::open(
            "2001:db8:1::/64".parse().unwrap(),
            dir.path(),
            Duration::from_secs(60),
            64,
        )
        .await
        .unwrap(),
    );
    let zones = Zones::new(vec![Zone::new(
        name("rings.rs"),
        name("ns.example.net"),
        vec![],
        name("hostmaster.example.net"),
    )])
    .unwrap();
    let authority = DnsAuthority::new(
        Arc::new(zones),
        AnswerTtl {
            positive: 60,
            negative: 30,
        },
        Arc::clone(&registry),
        Arc::new(Fixed::new()),
    );
    let socket = UdpSocket::bind("[::1]:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let mut server = Server::new(authority);
    server.register_socket(socket);
    Relay {
        _server: server,
        address,
        registry,
        _dir: dir,
    }
}

/// Send `query` to `server` from the loopback address of its family, and return the response.
async fn ask(server: SocketAddr, query: Message) -> Message {
    let loopback = match server {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
    };
    let socket = UdpSocket::bind(loopback).await.unwrap();
    socket
        .send_to(&query.to_vec().unwrap(), server)
        .await
        .unwrap();
    let mut buffer = vec![0u8; 4096];
    let (len, _from) = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buffer))
        .await
        .expect("the relay answers")
        .unwrap();
    Message::from_vec(&buffer[..len]).unwrap()
}

/// A standard query for `qname` of type `qtype`, with EDNS.
fn query(qname: &str, qtype: RecordType) -> Message {
    let mut message = Message::query();
    message.metadata.id = 7;
    message.add_query(Query::query(name(qname), qtype));
    message.set_edns(Edns::new());
    message
}

/// A port free for UDP and TCP in both families while the returned sockets live: a dual-stack
/// UDP socket and TCP socket on the IPv6 wildcard, which the kernel binds only where no socket
/// of either family holds the port.
fn reserve_port() -> (u16, [Socket; 2]) {
    let dual_stack = |kind, protocol| {
        let socket = Socket::new(Domain::IPV6, kind, Some(protocol)).unwrap();
        socket.set_only_v6(false).unwrap();
        socket
    };
    let udp = dual_stack(Type::DGRAM, Protocol::UDP);
    udp.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)).into())
        .unwrap();
    let port = udp.local_addr().unwrap().as_socket().unwrap().port();
    let tcp = dual_stack(Type::STREAM, Protocol::TCP);
    tcp.set_reuse_address(true).unwrap();
    tcp.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())
        .unwrap();
    (port, [udp, tcp])
}

#[tokio::test]
async fn test_an_owner_resolves_to_its_bound_address() {
    let relay = relay().await;
    let (server, registry) = (relay.address, &relay.registry);
    let response = ask(server, query("o1.rings.rs", RecordType::AAAA)).await;
    assert_eq!(response.metadata.response_code, ResponseCode::NoError);
    assert!(response.metadata.authoritative);
    let [answer] = response.answers.as_slice() else {
        panic!("one answer: {:?}", response.answers);
    };
    let RData::AAAA(AAAA(address)) = answer.data else {
        panic!("an AAAA record: {answer:?}");
    };
    assert!(registry.prefix().contains(&address));
    assert_eq!(registry.owner_at(&address).await, Some(Numbered(1)));
    assert!(response.edns.is_some(), "EDNS is answered");
}

#[tokio::test]
async fn test_mail_goes_to_the_owner_that_serves_port_25_only() {
    let relay = relay().await;
    let server = relay.address;
    let mail = ask(server, query("O1.Rings.RS", RecordType::MX)).await;
    assert_eq!(
        mail.answers
            .iter()
            .map(|record| record.data.clone())
            .collect::<Vec<_>>(),
        vec![RData::MX(MX::new(10, name("o1.rings.rs")))]
    );
    let none = ask(server, query("o2.rings.rs", RecordType::MX)).await;
    assert_eq!(none.metadata.response_code, ResponseCode::NoError);
    assert!(none.answers.is_empty());
    assert_eq!(none.authorities.len(), 1, "NODATA carries the SOA");
}

#[tokio::test]
async fn test_unknown_owners_and_names_do_not_exist() {
    let relay = relay().await;
    let (server, registry) = (relay.address, &relay.registry);
    for qname in ["o9.rings.rs", "www.rings.rs"] {
        let response = ask(server, query(qname, RecordType::AAAA)).await;
        assert_eq!(
            response.metadata.response_code,
            ResponseCode::NXDomain,
            "{qname}"
        );
        assert!(response.metadata.authoritative);
    }
    let unbound = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 9);
    assert_eq!(registry.owner_at(&unbound).await, None);
}

#[tokio::test]
async fn test_a_failed_lookup_is_a_server_failure_not_a_negative_answer() {
    let relay = relay().await;
    let server = relay.address;
    let response = ask(server, query("o3.rings.rs", RecordType::AAAA)).await;
    assert_eq!(response.metadata.response_code, ResponseCode::ServFail);
}

#[tokio::test]
async fn test_foreign_names_are_refused() {
    let relay = relay().await;
    let server = relay.address;
    let response = ask(server, query("example.org", RecordType::A)).await;
    assert_eq!(response.metadata.response_code, ResponseCode::Refused);
    assert!(!response.metadata.authoritative);
}

#[tokio::test]
async fn test_a_later_edns_version_is_refused_with_badvers() {
    let relay = relay().await;
    let server = relay.address;
    let mut message = query("o1.rings.rs", RecordType::AAAA);
    let mut edns = Edns::new();
    edns.set_version(1);
    message.set_edns(edns);
    let response = ask(server, message).await;
    // BADVERS shares its code, 16, with TSIG's BADSIG: compare the code on the wire.
    assert_eq!(
        u16::from(response.metadata.response_code),
        u16::from(ResponseCode::BADVERS)
    );
}

/// A relay listening as the default `dns_listen` does, on `[::]:p` and `0.0.0.0:p`, binds both
/// and answers over each family. Were `[::]:p` dual-stack, as Linux makes it by default, it would
/// hold IPv4 port `p` and the second bind would fail.
#[tokio::test]
async fn test_a_relay_serves_dns_on_both_wildcards_of_one_port() {
    let (dns, dns_reservation) = reserve_port();
    let (ingress, ingress_reservation) = reserve_port();
    let state = tempfile::tempdir().unwrap();
    let wildcards = [
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, dns)),
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, dns)),
    ];
    let config: DnsRelayConfig = serde_json::from_value(serde_json::json!({
        "zones": [{
            "origin": "rings.rs",
            "nameservers": ["ns.example.net"],
            "contact": "hostmaster.example.net",
        }],
        "prefix": "2001:db8:1::/64",
        "ports": [ingress],
        "state_dir": state.path(),
        "dns_listen": wildcards,
        "ingress_listen": Ipv6Addr::LOCALHOST,
    }))
    .unwrap();
    let relay = DnsRelay::open(&config, Arc::new(Fixed::new()), Arc::new(Unused))
        .await
        .unwrap();
    let (serving, served) = oneshot::channel();
    let (stop, stopped) = oneshot::channel();
    drop((dns_reservation, ingress_reservation));
    let running = relay.run(async move {
        let _case_gone = serving.send(());
        let _stop_dropped = stopped.await;
    });
    let asking = async move {
        // `run` polls its stop once every socket is served, and drops it unpolled when a bind
        // fails: its error is then the case's failure.
        if served.await.is_err() {
            return;
        }
        for server in [
            SocketAddr::from((Ipv6Addr::LOCALHOST, dns)),
            SocketAddr::from((Ipv4Addr::LOCALHOST, dns)),
        ] {
            let response = ask(server, query("o1.rings.rs", RecordType::AAAA)).await;
            assert_eq!(
                response.metadata.response_code,
                ResponseCode::NoError,
                "{server}"
            );
            assert_eq!(response.answers.len(), 1, "{server}");
        }
        let _relay_gone = stop.send(());
    };
    let (ran, ()) = tokio::join!(running, asking);
    ran.unwrap();
}
