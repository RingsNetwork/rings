use crate::dht::Did;
use crate::dht::PeerRing;
use crate::err::Result;
use crate::prelude::RTCSdpType;
use crate::storage::PersistenceStorage;
use crate::swarm::Swarm;
use crate::transports::manager::TransportManager;
use crate::types::ice_transport::IceTrickleScheme;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(all(not(feature = "wasm")))]
pub mod default;

#[allow(dead_code)]
pub fn setup_tracing() {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

pub async fn manually_establish_connection(swarm1: &Swarm, swarm2: &Swarm) -> Result<()> {
    assert!(swarm1.get_transport(swarm2.did()).is_none());
    assert!(swarm2.get_transport(swarm1.did()).is_none());

    let sm1 = swarm1.session_manager();
    let sm2 = swarm2.session_manager();

    let transport1 = swarm1.new_transport().await.unwrap();
    swarm1
        .register(swarm2.did(), transport1.clone())
        .await
        .unwrap();

    let handshake_info1 = transport1
        .get_handshake_info(sm1, RTCSdpType::Offer)
        .await?;

    let transport2 = swarm2.new_transport().await.unwrap();
    swarm2
        .register(swarm1.did(), transport2.clone())
        .await
        .unwrap();

    let addr1 = transport2.register_remote_info(handshake_info1).await?;

    assert_eq!(addr1, swarm1.did());

    let handshake_info2 = transport2
        .get_handshake_info(sm2, RTCSdpType::Answer)
        .await?;

    let addr2 = transport1.register_remote_info(handshake_info2).await?;

    assert_eq!(addr2, swarm2.did());

    #[cfg(all(not(feature = "wasm")))]
    {
        let promise_1 = transport1.connect_success_promise().await?;
        let promise_2 = transport2.connect_success_promise().await?;
        promise_1.await?;
        promise_2.await?;
    }

    assert!(swarm1.get_transport(swarm2.did()).is_some());
    assert!(swarm2.get_transport(swarm1.did()).is_some());

    Ok(())
}

pub async fn gen_pure_dht(did: Did) -> Result<PeerRing> {
    let db_path = PersistenceStorage::random_path("./tmp");
    let db = PersistenceStorage::new_with_path(db_path.as_str()).await?;
    Ok(PeerRing::new_with_storage(did, 3, db))
}

pub async fn gen_sorted_dht(s: usize) -> Vec<PeerRing> {
    let mut keys: Vec<crate::ecc::SecretKey> = vec![];
    for _i in 0..s {
        keys.push(crate::ecc::SecretKey::random());
    }
    keys.sort_by_key(|a| a.address());
    let dids: Vec<crate::dht::Did> = keys
        .iter()
	.map(|sk| crate::dht::Did::from(sk.address()));
    let mut iter = dids.into_iter();
    let mut ret: Vec<crate::dht::PeerRing> = vec![];
    for _ in 0..s {
        ret.push(
            crate::tests::gen_pure_dht(iter.next().unwrap())
                .await
                .unwrap(),
        )
    }
    ret
}
