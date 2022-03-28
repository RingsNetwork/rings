#[cfg(feature = "wasm")]
#[cfg(test)]
pub mod test {
    use anyhow::Result;
    use bns_core::channels::wasm::CbChannel;
    use bns_core::ecc::SecretKey;

    use bns_core::transports::wasm::WasmTransport as Transport;
    use bns_core::types::channel::Channel;
    use bns_core::types::channel::Event;
    use bns_core::types::ice_transport::IceServer;
    use bns_core::types::ice_transport::IceTransport;
    use bns_core::types::ice_transport::IceTransportCallback;
    use bns_core::types::ice_transport::IceTrickleScheme;
    use log::Level;
    use std::str::FromStr;
    use std::sync::Arc;

    use wasm_bindgen_test::wasm_bindgen_test_configure;
    use wasm_bindgen_test::*;
    use web_sys::RtcIceConnectionState;
    use web_sys::RtcSdpType;

    wasm_bindgen_test_configure!(run_in_browser);

    fn setup_log() {
        console_log::init_with_level(Level::Debug).expect("error initializing log");
    }

    async fn prepare_transport(channel: Option<Arc<CbChannel<Event>>>) -> Result<Transport> {
        let ch = match channel {
            Some(c) => Arc::clone(&c),
            None => Arc::new(<CbChannel<Event> as Channel<Event>>::new(1)),
        };
        let mut trans = Transport::new(ch.sender());
        let stun = IceServer::from_str("stun://stun.l.google.com:19302").unwrap();
        trans.start(&stun).await.unwrap();
        trans.apply_callback().await.unwrap();
        Ok(trans)
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
            .await
            .unwrap();

        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RtcIceConnectionState::New)
        );

        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RtcIceConnectionState::New)
        );

        // Peer 2 got offer then register
        let addr1 = transport2.register_remote_info(handshake_info1).await?;

        assert_eq!(addr1, key1.address());
        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RtcIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RtcIceConnectionState::New)
        );

        // Peer 2 create answer
        let handshake_info2 = transport2
            .get_handshake_info(key2, RtcSdpType::Answer)
            .await
            .unwrap();

        assert_eq!(
            transport1.ice_connection_state().await,
            Some(RtcIceConnectionState::New)
        );
        assert_eq!(
            transport2.ice_connection_state().await,
            Some(RtcIceConnectionState::Checking)
        );

        // Peer 1 got answer then register
        let addr2 = transport1
            .register_remote_info(handshake_info2)
            .await
            .unwrap();
        // assert_eq!(
        //     transport1.ice_connection_state().await,
        //     Some(RtcIceConnectionState::Checking)
        // );

        assert_eq!(addr2, key2.address());
        // let promise_1 = transport1.connect_success_promise().await.unwrap();
        // let promise_2 = transport2.connect_success_promise().await.unwrap();
        // promise_1.await.unwrap();
        // promise_2.await.unwrap();
        // assert_eq!(
        //     transport1.ice_connection_state().await,
        //     Some(RtcIceConnectionState::Connected)
        // );
        // assert_eq!(
        //     transport2.ice_connection_state().await,
        //     Some(RtcIceConnectionState::Connected)
        // );
        Ok(())
    }

    #[wasm_bindgen_test]
    async fn test_ice_connection_establish() {
        setup_log();
        let transport1 = prepare_transport(None).await.unwrap();
        let transport2 = prepare_transport(None).await.unwrap();
        establish_connection(&transport1, &transport2)
            .await
            .unwrap();
    }
}
