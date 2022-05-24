#[cfg(test)]
pub mod test {
    use rings_node::browser::Client;
    use rings_core::ecc::SecretKey;


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

    pub fn test_sdp_exchange() {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();
        let sm1 = SessionManager::new_with_seckey(&key1).unwrap();
        let sm2 = SessionManager::new_with_seckey(&key2).unwrap();
    }
}
