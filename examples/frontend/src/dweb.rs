//! Dweb protocol panel support.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::provider::Provider;
use serde::Deserialize;
use serde::Serialize;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use yew::Callback;

/// Hosted path table.
pub type Site = Rc<RefCell<HashMap<String, String>>>;

/// Build the default hosted page table.
pub fn default_site() -> HashMap<String, String> {
    HashMap::from([("/".to_string(), default_page())])
}

/// Default page hosted by a newly started browser node.
pub fn default_page() -> String {
    "<h1>Hello from Rings</h1><p>This page is hosted by a browser node.</p>".to_string()
}

/// A rendered dweb response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DwebResponse {
    /// Path returned by the remote peer.
    pub path: String,
    /// HTML body returned by the remote peer.
    pub body: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum DwebMsg {
    #[serde(rename = "req")]
    Req { path: String },
    #[serde(rename = "res")]
    Res { path: String, body: String },
}

/// Register the `dweb` namespace on a provider.
pub fn register(
    provider: &Arc<Provider>,
    site: Site,
    on_response: Callback<DwebResponse>,
) -> Result<(), String> {
    let handler = Closure::<dyn FnMut(JsValue, JsValue) -> JsValue>::new(
        move |ctx: JsValue, event: JsValue| handle(&ctx, &event, &site, &on_response),
    );
    let function: &Function = handler.as_ref().unchecked_ref();
    provider
        .on("dweb".to_string(), JsValue::NULL, function.clone())
        .map_err(|error| format!("register dweb: {error:?}"))?;
    handler.forget();
    Ok(())
}

/// Fetch a path from a peer.
pub async fn fetch(provider: Arc<Provider>, peer: String, path: String) -> Result<(), String> {
    let bytes = serde_json::to_vec(&DwebMsg::Req { path })
        .map_err(|error| format!("encode dweb: {error}"))?;
    JsFuture::from(provider.send_message(
        peer,
        "dweb".to_string(),
        Uint8Array::from(bytes.as_slice()),
    ))
    .await
    .map(|_| ())
    .map_err(|error| format!("send dweb request failed: {error:?}"))
}

fn handle(
    ctx: &JsValue,
    event: &JsValue,
    site: &Site,
    on_response: &Callback<DwebResponse>,
) -> JsValue {
    let result = Object::new();
    let state = Reflect::get(ctx, &JsValue::from_str("state")).unwrap_or(JsValue::NULL);
    let _state = Reflect::set(&result, &JsValue::from_str("state"), &state);
    let effects = Array::new();

    if let Some(message) = decode_event(event) {
        match message {
            IncomingDweb::Request { from, path } => {
                if let Some(effect) = response_effect(&from, &path, site) {
                    effects.push(&effect);
                }
            }
            IncomingDweb::Response { path, body } => on_response.emit(DwebResponse { path, body }),
        }
    }

    let _effects = Reflect::set(&result, &JsValue::from_str("effects"), &effects);
    result.into()
}

enum IncomingDweb {
    Request { from: String, path: String },
    Response { path: String, body: String },
}

fn decode_event(event: &JsValue) -> Option<IncomingDweb> {
    let payload = Reflect::get(event, &JsValue::from_str("payload")).ok()?;
    let bytes = Uint8Array::new(&payload).to_vec();
    let message = serde_json::from_slice::<DwebMsg>(&bytes).ok()?;
    match message {
        DwebMsg::Req { path } => {
            let from = Reflect::get(event, &JsValue::from_str("from"))
                .ok()
                .and_then(|value| value.as_string())?;
            Some(IncomingDweb::Request { from, path })
        }
        DwebMsg::Res { path, body } => Some(IncomingDweb::Response { path, body }),
    }
}

fn response_effect(from: &str, path: &str, site: &Site) -> Option<JsValue> {
    let body = site
        .borrow()
        .get(path)
        .cloned()
        .unwrap_or_else(|| "<h1>404 not found</h1>".to_string());
    let bytes = serde_json::to_vec(&DwebMsg::Res {
        path: path.to_string(),
        body,
    })
    .ok()?;
    let effect = Object::new();
    Reflect::set(&effect, &JsValue::from_str("to"), &JsValue::from_str(from)).ok()?;
    Reflect::set(
        &effect,
        &JsValue::from_str("payload"),
        &Uint8Array::from(bytes.as_slice()),
    )
    .ok()?;
    Some(effect.into())
}
