use wasm_bindgen_test::*;
use bns_core::transports::wasm::WasmTransport as Transport;
use bns_core::types::ice_transport::IceTransport;
use bns_core::types::ice_transport::IceTrickleScheme;
use bns_core::types::ice_transport::IceTransportCallback;
use bns_core::types::channel::Channel;
use bns_core::channels::wasm::CbChannel;
use bns_core::ecc::SecretKey;
use anyhow::Result;
use std::sync::Arc;
use web_sys::RtcSdpType;
use wasm_bindgen_test::wasm_bindgen_test_configure;

wasm_bindgen_test_configure!(run_in_browser);


async fn prepare_transport() -> Result<Transport> {
    let ch = Arc::new(CbChannel::new(1));
    let mut trans = Transport::new(ch.sender());
    let stun = "stun:stun.l.google.com:19302";
    trans.start(stun).await?.apply_callback().await?;
    Ok(trans)
}


#[wasm_bindgen_test]
async fn new_transport() {
    prepare_transport().await.unwrap();
}

pub async fn establish_connection(
    transport1: &Transport,
    transport2: &Transport,
) -> Result<()> {
    // Generate key pairs for signing and verification
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();

    // Peer 1 try to connect peer 2
    let handshake_info1 = transport1
        .get_handshake_info(key1, RtcSdpType::Offer)
        .await?;

    // Peer 2 got offer then register
    let addr1 = transport2.register_remote_info(handshake_info1).await?;

    // Peer 2 create answer
    let handshake_info2 = transport2
        .get_handshake_info(key2, RtcSdpType::Answer)
        .await?;

    // Peer 1 got answer then register
    let addr2 = transport1.register_remote_info(handshake_info2).await?;
    assert_eq!(addr2, key2.address());
    Ok(())
}

#[wasm_bindgen_test]
async fn test_ice_connection_establish() {
    let transport1 = prepare_transport().await.unwrap();
    let transport2 = prepare_transport().await.unwrap();

    establish_connection(&transport1, &transport2).await.unwrap();
}
