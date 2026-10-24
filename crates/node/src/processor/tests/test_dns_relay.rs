//! The DNS relay's ingress over the node (#1049), on the dummy transport: a connection to an
//! owner's relayed address reaches the owner's local service, through the owner's descriptor
//! in the DHT and the TCP relay, or straight to the relay node's own service.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::sync::Arc;

use ppp::v2::Addresses;
use ppp::v2::Header;
use rings_core::lifecycle::StopToken;
use rings_dns_relay::AddressRegistry;
use rings_dns_relay::AdmissionLimits;
use rings_dns_relay::Ingress;
use rings_dns_relay::ProxyProtocol;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use super::common::connect_processors;
use super::common::network_test_guard;
use super::common::wait_for_mutual_dht_topology;
use super::common::PROCESSOR_TEST_HANG_GUARD;
use crate::dns_relay::relay_service;
use crate::dns_relay::DhtDirectory;
use crate::dns_relay::DidOwner;
use crate::dns_relay::RelayForward;
use crate::dns_relay::ServiceOffer;
use crate::dns_relay::ServiceRegistration;
use crate::error::Error;
use crate::error::Result;
use crate::extension::protocols::relay::RelayHandle;
use crate::processor::Processor;
use crate::provider::Provider;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;
use crate::tests::native::prepare_processor;

/// A node with its TCP relay installed and its application inbox served.
struct Node {
    processor: Arc<Processor>,
    relay: RelayHandle,
    _provider: Provider,
}

impl Node {
    /// `processor`, with its relay installed and served.
    fn serve(processor: Processor) -> Result<Self> {
        let processor = Arc::new(processor);
        let provider = Provider::from_processor(Arc::clone(&processor));
        let relay = RelayHandle::install(&provider.extensions())?;
        let inbox = provider.application_service(None, StopToken::never())?;
        rings_runtime::spawn_detached(inbox)
            .map_err(|_| Error::ExtensionError("no runtime to serve the inbox".to_owned()))?;
        Ok(Self {
            processor,
            relay,
            _provider: provider,
        })
    }

    /// Serve `port` of this node's relayed address, behind a PROXY protocol header, with an
    /// echo service on the loopback, and publish the descriptor saying so.
    async fn offer_echo(&self, port: u16) -> Result<SocketAddr> {
        let target = echo_service().await;
        self.relay
            .register_tcp_service(relay_service(port), target)
            .await?;
        let offer = ServiceOffer::new(
            BTreeMap::from([(port, ProxyProtocol::V2)]),
            vec![],
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(90),
        )
        .map_err(|error| Error::InvalidConfig(error.to_string()))?;
        ServiceRegistration::new(self.processor.did(), offer)
            .register_once(&RegistrationContext::new(&self.processor))
            .await?;
        Ok(target)
    }
}

/// A loopback service that sends back every byte a connection sends, then closes it.
async fn echo_service() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut received = Vec::new();
                if stream.read_to_end(&mut received).await.is_ok() {
                    let _ = stream.write_all(&received).await;
                }
            });
        }
    });
    address
}

/// The ingress of `relay` on `listener`, routing `owner`, bound to `::1`, through the relay's
/// DHT; `own_targets` are the relay node's own served ports.
async fn ingress(
    relay: &Node,
    owner: &Node,
    listener: TcpListener,
    own_targets: Vec<(u16, SocketAddr)>,
    state: &std::path::Path,
) -> JoinSet<()> {
    let retention = std::time::Duration::from_secs(60);
    let registry = AddressRegistry::open("::1/128".parse().unwrap(), state, retention, 8)
        .await
        .unwrap();
    let owner = DidOwner(owner.processor.did());
    let address = registry.address_for(owner, 0).await.unwrap();
    assert_eq!(address, Ipv6Addr::LOCALHOST);
    let directory = DhtDirectory::new(Arc::clone(&relay.processor));
    let forward = RelayForward::new(relay.relay.clone(), relay.processor.did(), own_targets);
    Ingress::new(
        Ipv6Addr::LOCALHOST,
        BTreeSet::new(),
        Arc::new(registry),
        Arc::new(directory),
        Arc::new(forward),
        AdmissionLimits::default(),
    )
    .serve(vec![listener])
}

/// Send `hello` to `[::1]:port`, close the sending side, and return all the echo.
async fn exchange(port: u16) -> (SocketAddr, Vec<u8>) {
    let mut client = TcpStream::connect(("::1", port)).await.unwrap();
    let source = client.local_addr().unwrap();
    client.write_all(b"hello").await.unwrap();
    client.shutdown().await.unwrap();
    let mut echoed = Vec::new();
    tokio::time::timeout(PROCESSOR_TEST_HANG_GUARD, client.read_to_end(&mut echoed))
        .await
        .expect("the service answers through the relay")
        .unwrap();
    (source, echoed)
}

/// Check that `echoed` is a PROXY v2 header naming `source` and `[::1]:port`, then `hello`.
fn assert_proxied_hello(echoed: &[u8], source: SocketAddr, port: u16) {
    let header = Header::try_from(echoed).expect("a PROXY v2 header comes first");
    let Addresses::IPv6(addresses) = header.addresses else {
        panic!("IPv6 addresses: {:?}", header.addresses);
    };
    assert_eq!(
        SocketAddr::from((addresses.source_address, addresses.source_port)),
        source
    );
    assert_eq!(
        SocketAddr::from((addresses.destination_address, addresses.destination_port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port))
    );
    assert_eq!(echoed.get(header.len()..), Some(&b"hello"[..]));
}

#[tokio::test]
async fn test_a_connection_reaches_the_owner_through_its_descriptor_and_the_tcp_relay() -> Result<()>
{
    let _network = network_test_guard().await;
    let relay = Node::serve(prepare_processor().await)?;
    let owner = Node::serve(prepare_processor().await)?;
    connect_processors(&relay.processor, &owner.processor).await;
    wait_for_mutual_dht_topology(&relay.processor, &owner.processor).await?;
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    owner.offer_echo(port).await?;
    let state = tempfile::tempdir().unwrap();
    let _ingress = ingress(&relay, &owner, listener, vec![], state.path()).await;
    let (source, echoed) = exchange(port).await;
    assert_proxied_hello(&echoed, source, port);
    Ok(())
}

#[tokio::test]
async fn test_a_relay_node_serves_its_own_ports_without_an_overlay_hop() -> Result<()> {
    let relay = Node::serve(prepare_processor().await)?;
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = relay.offer_echo(port).await?;
    let state = tempfile::tempdir().unwrap();
    let _ingress = ingress(&relay, &relay, listener, vec![(port, target)], state.path()).await;
    let (source, echoed) = exchange(port).await;
    assert_proxied_hello(&echoed, source, port);
    Ok(())
}
