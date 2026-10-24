use std::collections::BTreeMap;
use std::net::Ipv6Addr;
use std::time::Duration;

use async_trait::async_trait;
use ppp::v2::Addresses;
use ppp::v2::Header;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::*;
use crate::announcement::Announcement;
use crate::owner::test_owner::TestOwner;

/// The directory failure of the cases.
#[derive(Debug, Error)]
#[error("unreachable")]
struct Unreachable;

/// A directory holding fixed announcements.
struct FixedDirectory(BTreeMap<TestOwner, Announcement>);

#[async_trait]
impl Directory for FixedDirectory {
    type Error = Unreachable;
    type Owner = TestOwner;

    async fn announcement(&self, owner: TestOwner) -> Result<Option<Announcement>, Unreachable> {
        Ok(self.0.get(&owner).cloned())
    }
}

/// A forward that sends the client its preamble, then the greeting `owner:port`, holds the
/// connection until the client closes it, and reports each finished connection on `finished`.
struct Greeting {
    finished: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl Forward for Greeting {
    type Error = std::io::Error;
    type Owner = TestOwner;

    async fn forward(&self, connection: Connection<TestOwner>) -> Result<(), std::io::Error> {
        let Connection {
            mut stream,
            owner,
            port,
            preamble,
        } = connection;
        stream.write_all(&preamble).await?;
        let greeting = format!("{}:{port}", owner.label());
        stream.write_all(greeting.as_bytes()).await?;
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await?;
        let _receiver_gone = self.finished.send(());
        Ok(())
    }
}

/// A running ingress: owner 7 serves the first of two loopback listeners.
struct Running {
    _tasks: JoinSet<()>,
    _dir: tempfile::TempDir,
    served: u16,
    unserved: u16,
    finished: mpsc::UnboundedReceiver<()>,
}

/// An ingress whose owner 7 serves its first listener's port with `proxy`; the registry binds
/// owner 7 to `::1` only when `bound`.
async fn serve(bound: bool, proxy: ProxyProtocol, limits: AdmissionLimits) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let registry = AddressRegistry::open(
        "::1/128".parse().unwrap(),
        dir.path(),
        Duration::from_secs(60),
        8,
    )
    .await
    .unwrap();
    if bound {
        let address = registry.address_for(TestOwner(7), 0).await.unwrap();
        assert_eq!(address, Ipv6Addr::LOCALHOST);
    }
    let served = TcpListener::bind("[::1]:0").await.unwrap();
    let unserved = TcpListener::bind("[::1]:0").await.unwrap();
    let served_port = served.local_addr().unwrap().port();
    let unserved_port = unserved.local_addr().unwrap().port();
    let announcement = Announcement::new([(served_port, proxy)], []).unwrap();
    let directory = FixedDirectory(BTreeMap::from([(TestOwner(7), announcement)]));
    let (finished, finished_events) = mpsc::unbounded_channel();
    let ingress = Ingress::new(
        Ipv6Addr::LOCALHOST,
        BTreeSet::new(),
        Arc::new(registry),
        Arc::new(directory),
        Arc::new(Greeting { finished }),
        limits,
    );
    Running {
        _tasks: ingress.serve(vec![served, unserved]),
        _dir: dir,
        served: served_port,
        unserved: unserved_port,
        finished: finished_events,
    }
}

/// Read from `stream` until `len` bytes or the end of the stream.
async fn read_up_to(stream: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut received = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let read = stream.read(&mut received[filled..]).await.unwrap();
        if read == 0 {
            break;
        }
        filled += read;
    }
    received.truncate(filled);
    received
}

/// Connect to `[::1]:port` and read up to `len` bytes of what the relay sends.
async fn greeting(port: u16, len: usize) -> (TcpStream, Vec<u8>) {
    let mut stream = TcpStream::connect(("::1", port)).await.unwrap();
    let received = read_up_to(&mut stream, len).await;
    (stream, received)
}

/// The greeting owner 7 sends from `port`.
fn greeted(port: u16) -> Vec<u8> {
    format!("o7:{port}").into_bytes()
}

#[tokio::test]
async fn test_a_connection_reaches_the_port_its_destination_names() {
    let running = serve(true, ProxyProtocol::Off, AdmissionLimits::default()).await;
    let expected = greeted(running.served);
    let (_stream, received) = greeting(running.served, expected.len()).await;
    assert_eq!(received, expected);
}

#[tokio::test]
async fn test_a_port_the_owner_does_not_serve_is_closed() {
    let running = serve(true, ProxyProtocol::Off, AdmissionLimits::default()).await;
    let (_stream, received) = greeting(running.unserved, 16).await;
    assert!(received.is_empty());
}

#[tokio::test]
async fn test_an_address_the_book_does_not_hold_is_closed() {
    let running = serve(false, ProxyProtocol::Off, AdmissionLimits::default()).await;
    let (_stream, received) = greeting(running.served, 16).await;
    assert!(received.is_empty());
}

#[tokio::test]
async fn test_a_proxy_protocol_port_reads_the_client_and_destination_first() {
    let running = serve(true, ProxyProtocol::V2, AdmissionLimits::default()).await;
    let mut stream = TcpStream::connect(("::1", running.served)).await.unwrap();
    let client = stream.local_addr().unwrap();
    // A v2 header is 16 fixed bytes, the last two the length of the addresses that follow.
    let mut header = read_up_to(&mut stream, 16).await;
    let length = u16::from_be_bytes([header[14], header[15]]);
    header.extend(read_up_to(&mut stream, usize::from(length)).await);
    let parsed = Header::try_from(header.as_slice()).unwrap();
    let Addresses::IPv6(addresses) = parsed.addresses else {
        panic!("IPv6 addresses: {:?}", parsed.addresses);
    };
    assert_eq!(
        SocketAddr::from((addresses.source_address, addresses.source_port)),
        client
    );
    assert_eq!(
        SocketAddr::from((addresses.destination_address, addresses.destination_port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, running.served))
    );
    let expected = greeted(running.served);
    assert_eq!(read_up_to(&mut stream, expected.len()).await, expected);
}

// The single-threaded runtime matters here: a forward reports its end and returns within one
// poll of its connection's task, which drops the slot in that same poll, so the slot is free by
// the time the test sees the report.
#[tokio::test(flavor = "current_thread")]
async fn test_an_owner_at_its_limit_is_refused_until_a_slot_frees() {
    let limits = AdmissionLimits {
        total: 8,
        per_owner: 1,
    };
    let mut running = serve(true, ProxyProtocol::Off, limits).await;
    let expected = greeted(running.served);
    let (first, received) = greeting(running.served, expected.len()).await;
    assert_eq!(
        received, expected,
        "the first connection holds the only slot"
    );
    let (_second, refused) = greeting(running.served, 16).await;
    assert!(refused.is_empty(), "the second is closed at once");
    drop(first);
    running.finished.recv().await.unwrap();
    let (_third, admitted) = greeting(running.served, expected.len()).await;
    assert_eq!(admitted, expected);
}

/// One step of an admission schedule.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// A connection is accepted.
    Enter,
    /// An accepted, unrouted connection is routed to the owner.
    Bind(TestOwner),
    /// An accepted connection ends before it is routed.
    LeaveUnbound,
    /// A connection routed to the owner ends.
    LeaveBound(TestOwner),
}

/// The step that three bits name.
fn step(bits: u32) -> Step {
    match bits {
        1 => Step::Bind(TestOwner(0)),
        2 => Step::Bind(TestOwner(1)),
        3 => Step::LeaveUnbound,
        4 => Step::LeaveBound(TestOwner(0)),
        5 => Step::LeaveBound(TestOwner(1)),
        _ => Step::Enter,
    }
}

#[test]
fn test_admission_holds_its_invariant_over_every_schedule() {
    let limits = AdmissionLimits {
        total: 3,
        per_owner: 2,
    };
    let owners = [TestOwner(0), TestOwner(1)];
    // Every schedule of 6 steps; a step whose precondition does not hold is skipped, as no
    // connection could take it.
    for schedule in 0u32..(1 << 18) {
        let mut admission = Admission::new(limits);
        let mut unbound = 0usize;
        let mut bound = BTreeMap::<TestOwner, usize>::new();
        for at in 0..6 {
            let live = unbound + bound.values().sum::<usize>();
            match step((schedule >> (3 * at)) & 0b111) {
                Step::Enter => {
                    assert_eq!(admission.enter(), live < limits.total, "{schedule:#x}");
                    if live < limits.total {
                        unbound += 1;
                    }
                }
                Step::Bind(owner) if unbound > 0 => {
                    let to_owner = bound.get(&owner).copied().unwrap_or(0);
                    let admits = to_owner < limits.per_owner;
                    assert_eq!(admission.bind(owner), admits, "{schedule:#x}");
                    unbound -= 1;
                    if admits {
                        *bound.entry(owner).or_default() += 1;
                    } else {
                        // A refused connection is closed, giving its total slot back.
                        admission.leave(None);
                    }
                }
                Step::LeaveUnbound if unbound > 0 => {
                    admission.leave(None);
                    unbound -= 1;
                }
                Step::LeaveBound(owner) if bound.get(&owner).copied().unwrap_or(0) > 0 => {
                    admission.leave(Some(&owner));
                    *bound.entry(owner).or_default() -= 1;
                }
                Step::Bind(_) | Step::LeaveUnbound | Step::LeaveBound(_) => {}
            }
            let live = unbound + bound.values().sum::<usize>();
            assert_eq!(admission.live(), live, "{schedule:#x}");
            assert!(admission.live() <= limits.total, "{schedule:#x}");
            for owner in owners {
                let to_owner = bound.get(&owner).copied().unwrap_or(0);
                assert_eq!(admission.live_for(&owner), to_owner, "{schedule:#x}");
                assert!(to_owner <= limits.per_owner, "{schedule:#x}");
            }
            assert!(
                admission.per_owner.values().all(|live| *live > 0),
                "no owner is held at zero: {schedule:#x}"
            );
        }
    }
}
