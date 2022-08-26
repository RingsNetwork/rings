use wasm_bindgen_futures::future_to_promise;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen_test]
async fn test_std_channel() {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    async {
        sender.send("test").unwrap();
    }
    .await;
    async {
        assert_eq!(receiver.recv().unwrap(), "test");
    }
    .await;
}

// ref https://rustwasm.github.io/wasm-bindgen/api/src/wasm_bindgen_futures/lib.rs.html#NaN
#[wasm_bindgen_test]
async fn test_async_channel_via_future_to_promise() {
    let (sender, mut receiver) = futures::channel::mpsc::channel(1);
    let fut = async move {
        let mut sender = sender.clone();
        assert!(!sender.is_closed());
        sender.try_send("test").unwrap();
        Ok("ok".into())
    };
    let promise = future_to_promise(fut);
    JsFuture::from(promise).await.unwrap();
    assert_eq!(receiver.try_next().unwrap(), Some("test"));
}

// ref https://rustwasm.github.io/wasm-bindgen/api/src/wasm_bindgen_futures/lib.rs.html#77-82
#[wasm_bindgen_test]
fn test_async_channel_in_spwan_local() {
    let (sender, mut receiver) = futures::channel::mpsc::channel(1);
    spawn_local(async move {
        let mut sender = sender.clone();
        assert!(!sender.is_closed());
        sender.try_send("test").unwrap();
    });
    spawn_local(async move {
        let next = receiver.try_next().unwrap();
        assert_eq!(next, Some("test"));
    })
}
