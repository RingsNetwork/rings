//! The close law of a provider (#1085): a closed node has released every store it was built
//! over, so the stores open again in this process; a node still shared reports so, and its
//! stores are released once its last clone drops.
//!
//! Every node here is built over the native stores under a fresh root, as the FFI provider's
//! are (`NodeStores::open_under`), and the witness of a release is an open of the same root.
//! Every wait is woken by an event (a processor's activity, a store's release); the hang guard
//! only bounds a hang.

use std::future::Future;
use std::future::IntoFuture;
use std::path::Path;

use futures::FutureExt;

use super::common::*;
use super::*;
use crate::extension::ext::LinkFact;
use crate::extension::BackendObserver;
use crate::measure::EvidenceCollectorIdentity;
use crate::native::stores::NodeStores;
use crate::tests::activity::record_activity;
use crate::tests::test_processor_builder;
use crate::tests::test_processor_config;

/// Records an activity on every link fact, so a probe of a provider's admitted peers wakes on
/// them.
struct LinkActivity;

impl BackendObserver for LinkActivity {
    fn link_fact(&self, _fact: LinkFact) {
        record_activity();
    }
}

/// A processor over the native stores under `root`, its measurement over the two measure
/// stores, carrying the stores' release witnesses.
async fn node_under(root: &Path) -> Arc<Processor> {
    let stores = NodeStores::open_under(root)
        .await
        .expect("the stores under the root open");
    let config = test_processor_config(&SecretKey::random(), 0);
    let collector =
        EvidenceCollectorIdentity::new(config.network_id(), config.delegatee_key().delegator_did());
    let measure =
        PeriodicMeasure::new_with_evidence_storage(stores.measure, stores.evidence, collector)
            .await
            .expect("the measurement opens over its stores");
    let processor = test_processor_builder(&config)
        .storage(stores.data)
        .relay_storage(stores.relay)
        .removal_storage(stores.removals)
        .onion_entry_guard_storage(stores.guards)
        .replay_storage(stores.replay)
        .measure(measure)
        .stores_released(stores.released)
        .build()
        .expect("the processor builds");
    Arc::new(processor)
}

/// Await `close` under the hang guard.
async fn closed(close: impl Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(PROCESSOR_TEST_HANG_GUARD, close)
        .await
        .expect("the close ends")
}

/// Connect `p1` to `p2` by a manual offer and answer, and await both admitting the other.
async fn connected(p1: &Processor, p2: &Processor) {
    let offer = p1.swarm.create_offer(p2.did()).await.unwrap();
    let answer = p2
        .swarm
        .answer_offer(offer)
        .await
        .unwrap()
        .answer()
        .expect("p2 holds no offer of its own, so it answers");
    p1.swarm.accept_answer(answer).await.unwrap();
    wait_processors_connected(p1, p2).await;
}

/// The close law (#1085): a node listening, serving its application and connected to a peer,
/// with a measurement not yet persisted, has released every store once its close returns `Ok`.
/// All seven open again on the same root, the measurement in them.
///
/// The listener runs on a clone of the provider beside the close, in the same task, so the
/// clone drops as the close's stop ends the listener generation, before the close reaches
/// its release (`join!` polls both in one pass).
#[tokio::test]
async fn test_a_closed_provider_releases_every_store_for_a_reopen() {
    let _network_guard = network_test_guard().await;
    let root = tempfile::tempdir().unwrap();
    let p1 = node_under(root.path()).await;
    let p2 = prepare_processor().await;
    let _recorder2 = ApplicationRecorder::attach(&p2);
    let provider = Provider::from_processor(Arc::clone(&p1));
    provider
        .serve_application(Some(Arc::new(LinkActivity)))
        .unwrap();
    connected(&p1, &p2).await;
    let pending: Did = SecretKey::random().address().into();
    p1.record_authenticated_measurement_for_test(pending)
        .await
        .unwrap();
    let node = Arc::downgrade(&p1);
    drop(p1);

    let listening = provider.clone();
    let never = StopSource::new();
    let listen = async move {
        listening.listen_with(never.token()).await.unwrap();
    };
    let ((), close) = tokio::time::timeout(PROCESSOR_TEST_HANG_GUARD, async {
        futures::join!(listen, provider.close())
    })
    .await
    .expect("the close ends the listener generation, then itself");

    close.unwrap();
    assert!(node.upgrade().is_none(), "the close dropped the node");
    let reopened = node_under(root.path()).await;
    assert!(
        reopened
            .peer_measurements()
            .await
            .iter()
            .any(|measurement| measurement.did == pending),
        "the close persisted the pending measurement before releasing its store"
    );
}

/// A node closed while a clone of it is held is stopped and its network closed, but its stores
/// stay held by the clone: the close reports `ProviderStillShared`, whose `released` resolves
/// once the clone drops, and then the stores open again.
#[tokio::test]
async fn test_a_shared_provider_releases_its_stores_when_its_last_clone_drops() {
    let root = tempfile::tempdir().unwrap();
    let p1 = node_under(root.path()).await;
    let provider = Provider::from_processor(Arc::clone(&p1));

    let Err(Error::ProviderStillShared { released }) = closed(provider.close()).await else {
        panic!("a held clone shares the node");
    };

    assert_eq!(released.len(), 7, "every store of the node is witnessed");
    assert!(matches!(
        p1.swarm
            .create_offer(SecretKey::random().address().into())
            .await,
        Err(rings_core::error::Error::SwarmClosed)
    ));
    let mut released = released.into_future();
    assert!(
        (&mut released).now_or_never().is_none(),
        "the held clone holds the stores"
    );
    assert!(
        NodeStores::open_under(root.path()).await.is_err(),
        "a held store stays locked"
    );
    drop(p1);
    released.await;
    NodeStores::open_under(root.path())
        .await
        .expect("the released stores open again");
}

/// A handshake still in flight across the close (an offer the peer answered and this node never
/// accepted) holds no store past the close: the close returns `Ok`, and the stores open again.
#[tokio::test]
async fn test_a_handshake_in_flight_across_the_close_holds_no_store() {
    let _network_guard = network_test_guard().await;
    let root = tempfile::tempdir().unwrap();
    let p1 = node_under(root.path()).await;
    let p2 = prepare_processor().await;
    let provider = Provider::from_processor(Arc::clone(&p1));
    let offer = p1.swarm.create_offer(p2.did()).await.unwrap();
    p2.swarm
        .answer_offer(offer)
        .await
        .unwrap()
        .answer()
        .expect("p2 holds no offer of its own, so it answers");
    drop(p1);

    closed(provider.close()).await.unwrap();

    NodeStores::open_under(root.path())
        .await
        .expect("the stores open again with the handshake abandoned");
}
