#[cfg(test)]
pub mod test {
    use rings_core::channels::Channel as CbChannel;
    use rings_core::ecc::SecretKey;
    use rings_core::err::Result;
    use rings_core::session::SessionManager;
    use log::Level;
    use rings_core::transports::Transport;
    use rings_core::types::channel::Channel;
    use rings_core::types::channel::Event;
    use rings_core::types::ice_transport::IceServer;
    use rings_core::types::ice_transport::IceTransport;
    use rings_core::types::ice_transport::IceTransportCallback;
    use rings_core::types::ice_transport::IceTrickleScheme;
    use std::str::FromStr;
    use std::sync::Arc;
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::wasm_bindgen_test_configure;
    use wasm_bindgen_test::*;
    use wasm_bindgen::JsValue;
    use web_sys::RtcIceConnectionState;
    use web_sys::RtcSdpType;

    wasm_bindgen_test_configure!(run_in_browser);

    fn setup_log() {
        console_log::init_with_level(Level::Debug).expect("error initializing log");
    }

    async fn get_fake_permission() {
        let window = web_sys::window().unwrap();
        let nav = window.navigator();
        let media = nav.media_devices().unwrap();
        let mut cons = web_sys::MediaStreamConstraints::new();
        cons.audio(&JsValue::from(true));
        cons.video(&JsValue::from(true));
        cons.fake(true);
        let promise = media.get_user_media_with_constraints(&cons).unwrap();
        JsFuture::from(promise).await.unwrap();
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

        let session1 = SessionManager::new_with_seckey(&key1).unwrap();
        let session2 = SessionManager::new_with_seckey(&key2).unwrap();

        // Peer 1 try to connect peer 2
        let handshake_info1 = transport1
            .get_handshake_info(session1, RtcSdpType::Offer)
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
        // Peer 2 create answer
        let handshake_info2 = transport2
            .get_handshake_info(session2, RtcSdpType::Answer)
            .await
            .unwrap();

        // Peer 1 got answer then register
        let addr2 = transport1
            .register_remote_info(handshake_info2)
            .await
            .unwrap();
        assert_eq!(addr2, key2.address());

        // assert_eq!(
        //     transport2.ice_connection_state().await,
        //     Some(RtcIceConnectionState::Connected)
        // );
        Ok(())
    }

    #[wasm_bindgen_test]
    async fn test_ice_connection_establish() {
        get_fake_permission().await;
        setup_log();
        let transport1 = prepare_transport(None).await.unwrap();
        let transport2 = prepare_transport(None).await.unwrap();
        establish_connection(&transport1, &transport2)
            .await
            .unwrap();
    }
}
