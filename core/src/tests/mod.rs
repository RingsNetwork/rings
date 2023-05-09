use crate::err::Result;
use crate::prelude::RTCSdpType;
use crate::swarm::Swarm;
use crate::transports::manager::TransportManager;
use crate::types::ice_transport::IceTrickleScheme;
use std::collections::HashMap;

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

/// Context for unitest to store nodes
pub type TestContext<'a> = HashMap<crate::dht::Did, &'a crate::message::MessageHandler>;

pub async fn manually_establish_connection(swarm1: &Swarm, swarm2: &Swarm) -> Result<()> {
    assert!(swarm1.get_transport(swarm2.did()).is_none());
    assert!(swarm2.get_transport(swarm1.did()).is_none());

    let transport1 = swarm1.new_transport().await.unwrap();
    swarm1
        .register(swarm2.did(), transport1.clone())
        .await
        .unwrap();

    let transport2 = swarm2.new_transport().await.unwrap();
    swarm2
        .register(swarm1.did(), transport2.clone())
        .await
        .unwrap();

    let handshake_info1 = transport1.get_handshake_info(RTCSdpType::Offer).await?;
    transport2
        .register_remote_info(&handshake_info1, swarm1.did())
        .await?;

    let handshake_info2 = transport2.get_handshake_info(RTCSdpType::Answer).await?;
    transport1
        .register_remote_info(&handshake_info2, swarm2.did())
        .await?;

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
