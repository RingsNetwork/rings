use std::sync::Arc;

use tokio::time::sleep;
use tokio::time::Duration;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::Stabilization;
use crate::ecc::SecretKey;
use crate::err::Error;
use crate::err::Result;
use crate::message::handlers::tests::manually_establish_connection;
use crate::message::MessageHandler;
use crate::session::SessionManager;
use crate::storage::PersistenceStorage;
use crate::swarm::Swarm;
use crate::transports::manager::TransportManager;
use crate::types::message::MessageListener;

async fn new_chord(did: Did, path: &str) -> PeerRing {
    PeerRing::new_with_storage(
        did,
        Arc::new(PersistenceStorage::new_with_path(path).await.unwrap()),
    )
}

fn new_swarm(key: &SecretKey) -> Swarm {
    let stun = "stun://stun.l.google.com:19302";
    let session = SessionManager::new_with_seckey(key).unwrap();
    Swarm::new(stun, key.address(), session)
}

pub async fn establish_connection(swarm1: Arc<Swarm>, swarm2: Arc<Swarm>) -> Result<()> {
    assert!(swarm1.get_transport(&swarm2.address()).is_none());
    assert!(swarm2.get_transport(&swarm1.address()).is_none());

    manually_establish_connection(&swarm1, &swarm2).await
}

async fn run_stabilize(chord: Arc<PeerRing>, swarm: Arc<Swarm>) {
    let mut result = Result::<()>::Ok(());
    let stabilization = Stabilization::new(chord, swarm, 5usize);
    let timeout_in_secs = stabilization.get_timeout();
    println!("RUN Stabilization");
    while result.is_ok() {
        let timeout = sleep(Duration::from_secs(timeout_in_secs as u64));
        tokio::pin!(timeout);
        tokio::select! {
            _ = timeout.as_mut() => {
                result = stabilization
                    .stabilize()
                    .await;
            }
        }
    }
}

#[tokio::test]
async fn test_stabilization_once() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let path1 = PersistenceStorage::random_path("./tmp");
    let path2 = PersistenceStorage::random_path("./tmp");
    let dht1 = Arc::new(new_chord(key1.address().into(), path1.as_str()).await);
    let dht2 = Arc::new(new_chord(key2.address().into(), path2.as_str()).await);
    let swarm1 = Arc::new(new_swarm(&key1));
    let swarm2 = Arc::new(new_swarm(&key2));
    establish_connection(Arc::clone(&swarm1), Arc::clone(&swarm2)).await?;

    let handler1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
    let handler2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));
    println!(
        "swarm1: {:?}, swarm2: {:?}",
        swarm1.address(),
        swarm2.address()
    );

    tokio::select! {
        _ = async {
            futures::join!(
                async {
                    loop {
                        Arc::new(handler1.clone()).listen().await;
                    }
                },
                async {
                    loop {
                        Arc::new(handler2.clone()).listen().await;
                    }
                },
            );
        } => { unreachable!(); }
        _ = async {
            let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(dht1.lock_successor()?.list().contains(&key2.address().into()));
            assert!(dht2.lock_successor()?.list().contains(&key1.address().into()));
            let stabilization = Stabilization::new(Arc::clone(&dht1), Arc::clone(&swarm1), 5usize);
            let _ = stabilization.stabilize().await;
            sleep(Duration::from_millis(10000)).await;
            assert_eq!(*dht2.lock_predecessor()?, Some(key1.address().into()));
            assert!(dht1.lock_successor()?.list().contains(&key2.address().into()));

            Ok::<(), Error>(())
        } => {}
    }
    tokio::fs::remove_dir_all("./tmp").await.ok();
    Ok(())
}

#[tokio::test]
async fn test_stabilization() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let path1 = PersistenceStorage::random_path("./tmp");
    let path2 = PersistenceStorage::random_path("./tmp");
    let dht1 = Arc::new(new_chord(key1.address().into(), path1.as_str()).await);
    let dht2 = Arc::new(new_chord(key2.address().into(), path2.as_str()).await);

    let swarm1 = Arc::new(new_swarm(&key1));
    let swarm2 = Arc::new(new_swarm(&key2));
    establish_connection(Arc::clone(&swarm1), Arc::clone(&swarm2)).await?;

    let handler1 = MessageHandler::new(Arc::clone(&dht1), Arc::clone(&swarm1));
    let handler2 = MessageHandler::new(Arc::clone(&dht2), Arc::clone(&swarm2));

    tokio::select! {
        _ = async {
            tokio::join!(
                async {
                    loop {
                        Arc::new(handler1.clone()).listen().await;
                    }
                },
                async {
                    loop {
                        Arc::new(handler2.clone()).listen().await;
                    }
                },
                async {
                    run_stabilize(Arc::clone(&dht1), Arc::clone(&swarm1)).await;
                },
                async {
                    run_stabilize(Arc::clone(&dht2), Arc::clone(&swarm2)).await;
                }
            );
        } => { unreachable!(); }
        _ = async {
            let transport_1_to_2 = swarm1.get_transport(&swarm2.address()).unwrap();
            sleep(Duration::from_millis(1000)).await;
            transport_1_to_2.wait_for_data_channel_open().await.unwrap();
            assert!(dht1.lock_successor()?.list().contains(&key2.address().into()));
            assert!(dht2.lock_successor()?.list().contains(&key1.address().into()));
            sleep(Duration::from_millis(10000)).await;
            assert_eq!(*dht2.lock_predecessor()?, Some(key1.address().into()));
            assert_eq!(*dht1.lock_predecessor()?, Some(key2.address().into()));
            Ok::<(), Error>(())
        } => {}
    }
    tokio::fs::remove_dir_all("./tmp").await.ok();
    Ok(())
}
