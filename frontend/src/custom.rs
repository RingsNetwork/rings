//! User-defined custom namespace messaging.

use std::sync::Arc;

use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::provider::Provider;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use yew::Callback;

use crate::node;

/// Namespaces installed automatically for the browser frontend.
pub const DEMO_NAMESPACES: &[&str] = &["custom", "example"];

/// Custom message observed by the local node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomEvent {
    /// Namespace that received this event.
    pub namespace: String,
    /// Verified sender DID.
    pub from: String,
    /// Payload decoded lossily as UTF-8.
    pub payload: String,
}

/// Register a custom namespace and emit inbound events to the UI.
pub fn register(
    provider: &Arc<Provider>,
    namespace: String,
    on_event: Callback<CustomEvent>,
) -> Result<(), String> {
    let namespace_for_handler = namespace.clone();
    let handler = Closure::<dyn FnMut(JsValue, JsValue) -> JsValue>::new(
        move |ctx: JsValue, event: JsValue| handle(&ctx, &event, &namespace_for_handler, &on_event),
    );
    let function: &Function = handler.as_ref().unchecked_ref();
    provider
        .on(namespace, JsValue::NULL, function.clone())
        .map_err(|error| format!("register custom namespace: {error:?}"))?;
    handler.forget();
    Ok(())
}

/// Send a custom namespace message.
pub async fn send(
    provider: Arc<Provider>,
    did: String,
    namespace: String,
    payload: String,
) -> Result<(), String> {
    node::send_message(provider, did, namespace, payload.into_bytes()).await
}

fn handle(
    ctx: &JsValue,
    event: &JsValue,
    namespace: &str,
    on_event: &Callback<CustomEvent>,
) -> JsValue {
    if let Some(custom_event) = decode_event(event, namespace) {
        on_event.emit(custom_event);
    }

    let result = Object::new();
    let state = Reflect::get(ctx, &JsValue::from_str("state")).unwrap_or(JsValue::NULL);
    let _state = Reflect::set(&result, &JsValue::from_str("state"), &state);
    let _effects = Reflect::set(&result, &JsValue::from_str("effects"), &Array::new());
    result.into()
}

fn decode_event(event: &JsValue, namespace: &str) -> Option<CustomEvent> {
    let from = Reflect::get(event, &JsValue::from_str("from"))
        .ok()
        .and_then(|value| value.as_string())?;
    let payload = Reflect::get(event, &JsValue::from_str("payload")).ok()?;
    let bytes = Uint8Array::new(&payload).to_vec();
    let payload = String::from_utf8_lossy(&bytes).to_string();
    Some(CustomEvent {
        namespace: namespace.to_string(),
        from,
        payload,
    })
}
