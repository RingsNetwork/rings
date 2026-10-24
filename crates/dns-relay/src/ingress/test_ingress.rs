use std::collections::BTreeMap;
use std::net::Ipv6Addr;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use super::*;
use crate::announcement::Announcement;
use crate::capability::ForwardFailure;
use crate::capability::LookupFailure;
use crate::owner::test_owner::TestOwner;

/// A directory holding fixed announcements.
struct FixedDirectory(BTreeMap<TestOwner, Announcement>);

#[async_trait]
impl Directory for FixedDirectory {
    type Owner = TestOwner;

    async fn announcement(&self, owner: TestOwner) -> Result<Option<Announcement>, LookupFailure> {
        Ok(self.0.get(&owner).cloned())
    }
}

/// A forward that greets the client with the service's name and holds the connection until
/// the client closes it.
struct Greeting;

#[async_trait]
impl Forward for Greeting {
    type Owner = TestOwner;

    async fn forward(
        &self,
        mut stream: TcpStream,
        _owner: TestOwner,
        service: &ServiceName,
    ) -> Result<(), ForwardFailure> {
        let fail = |error: std::io::Error| ForwardFailure(error.to_string());
        stream
            .write_all(service.as_str().as_bytes())
            .await
            .map_err(fail)?;
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.map_err(fail)?;
        Ok(())
    }
}

/// An ingress serving `owner` at `::1`, which announces `served` on the first of two loopback
/// listeners; return the set serving it and both listeners' ports.
async fn serve(
    owner: Option<TestOwner>,
    limits: AdmissionLimits,
) -> (JoinSet<()>, u16, u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(
        AddressRegistry::open(
            "::1/128".parse().unwrap(),
            dir.path(),
            Duration::from_secs(60),
        )
        .await
        .unwrap(),
    );
    let served = TcpListener::bind("[::1]:0").await.unwrap();
    let unserved = TcpListener::bind("[::1]:0").await.unwrap();
    let served_port = served.local_addr().unwrap().port();
    let unserved_port = unserved.local_addr().unwrap().port();
    let announcement =
        Announcement::new([(served_port, ServiceName::new("echo").unwrap())], []).unwrap();
    if let Some(owner) = owner {
        assert_eq!(
            registry.address_for(owner, 0).await.unwrap(),
            Ipv6Addr::LOCALHOST
        );
    }
    let ingress = Ingress::new(
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        BTreeSet::new(),
        registry,
        Arc::new(FixedDirectory(BTreeMap::from([(
            TestOwner(7),
            announcement,
        )]))),
        Arc::new(Greeting),
        limits,
    );
    let tasks = ingress.serve(vec![served, unserved]);
    (tasks, served_port, unserved_port, dir)
}

/// Connect to `[::1]:port` and read what the relay sends before closing, up to `len` bytes.
async fn greeting(port: u16, len: usize) -> (TcpStream, Vec<u8>) {
    let mut stream = TcpStream::connect(("::1", port)).await.unwrap();
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
    (stream, received)
}

#[tokio::test]
async fn test_a_connection_reaches_the_service_its_destination_names() {
    let (_tasks, served, _unserved, _dir) =
        serve(Some(TestOwner(7)), AdmissionLimits::default()).await;
    let (_stream, received) = greeting(served, 4).await;
    assert_eq!(received, b"echo");
}

#[tokio::test]
async fn test_a_port_the_owner_does_not_serve_is_closed() {
    let (_tasks, _served, unserved, _dir) =
        serve(Some(TestOwner(7)), AdmissionLimits::default()).await;
    let (_stream, received) = greeting(unserved, 4).await;
    assert!(received.is_empty());
}

#[tokio::test]
async fn test_an_address_the_book_does_not_hold_is_closed() {
    let (_tasks, served, _unserved, _dir) = serve(None, AdmissionLimits::default()).await;
    let (_stream, received) = greeting(served, 4).await;
    assert!(received.is_empty());
}

#[tokio::test]
async fn test_an_owner_at_its_limit_is_refused_until_a_slot_frees() {
    let limits = AdmissionLimits {
        total: 8,
        per_owner: 1,
    };
    let (_tasks, served, _unserved, _dir) = serve(Some(TestOwner(7)), limits).await;
    let (first, received) = greeting(served, 4).await;
    assert_eq!(
        received, b"echo",
        "the first connection holds the only slot"
    );
    let (_second, refused) = greeting(served, 4).await;
    assert!(refused.is_empty(), "the second is closed at once");
    drop(first);
    // The slot frees when the first forward sees its client close; wait for that event by
    // retrying until a new connection is admitted.
    let admitted = loop {
        let (_stream, received) = greeting(served, 4).await;
        if !received.is_empty() {
            break received;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(admitted, b"echo");
}

#[test]
fn test_admission_holds_its_bounds_over_every_schedule() {
    let limits = AdmissionLimits {
        total: 3,
        per_owner: 2,
    };
    // Every sequence of 8 steps over two owners, each step admitting or releasing one.
    for schedule in 0u32..(1 << 16) {
        let mut admission = Admission::new(limits);
        let mut admitted = BTreeMap::<TestOwner, usize>::new();
        for step in 0..8 {
            let bits = (schedule >> (2 * step)) & 0b11;
            let owner = TestOwner(bits & 1);
            if bits & 0b10 == 0 {
                if admission.admit(owner) {
                    *admitted.entry(owner).or_default() += 1;
                }
            } else if admitted.get(&owner).copied().unwrap_or(0) > 0 {
                admission.release(&owner);
                *admitted.entry(owner).or_default() -= 1;
            }
            let total = admitted.values().sum::<usize>();
            assert!(total <= limits.total, "schedule {schedule:#x}");
            for (owner, live) in &admitted {
                assert!(*live <= limits.per_owner, "schedule {schedule:#x}");
                assert_eq!(admission.live(owner), *live, "schedule {schedule:#x}");
            }
            let full = total == limits.total;
            for owner in [TestOwner(0), TestOwner(1)] {
                let at_limit = admitted.get(&owner).copied().unwrap_or(0) == limits.per_owner;
                assert_eq!(admission.permits(&owner), !full && !at_limit);
            }
        }
    }
}
