#![warn(missing_docs)]
//! JavaScript protocol adapter (browser).
//!
//! Bridges a JS handler into the same [`Protocol`] model so the browser sees the exact
//! abstraction native does: state lives in the runtime, the handler is a (trusted-pure)
//! transition returning `{ state, effects }`, and the extension's [`Interpret`] shell
//! ([`JsShell`]) runs the effects.
//!
//! ```text
//!   handler : (Ctx, Event) → { state, effects }
//!     Ctx    = { did: string, state: any }
//!     Event  = { from: string, payload: Uint8Array }
//!     effects: Array<{ to: string, payload: Uint8Array }>
//! ```
//!
//! Effects are namespace-scoped: a handler's `send` always goes out under the protocol's own
//! namespace (the one it was registered on via `provider.on(...)`), so a JS extension cannot
//! address another namespace — there is no `namespace` field on an effect.

use std::str::FromStr;

use bytes::Bytes;
use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_core::dht::Did;
use wasm_bindgen::JsValue;

use crate::extension::ext::Ctx;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;

/// A JS handler effect: send `payload` to `to` over the overlay, under the protocol's own
/// namespace (the scope decides the namespace — a handler cannot choose another).
pub struct JsSend {
    to: Did,
    payload: Bytes,
}

/// A protocol whose transition is a JS function. State and event are opaque [`JsValue`]s.
pub struct JsProtocol {
    namespace: String,
    initial: JsValue,
    handler: Function,
}

impl JsProtocol {
    /// Build a JS protocol from its namespace, initial state and handler function.
    pub fn new(namespace: String, initial: JsValue, handler: Function) -> Self {
        Self {
            namespace,
            initial,
            handler,
        }
    }
}

impl Protocol for JsProtocol {
    type State = JsValue;
    type Event = JsValue;
    type Effect = JsSend;

    fn namespace(&self) -> &str {
        self.namespace.as_str()
    }

    fn init(&self) -> JsValue {
        self.initial.clone()
    }

    /// Build the JS `event` object `{ from, payload }` at the boundary.
    fn decode(&self, wire: Wire<'_>) -> Result<JsValue, Reject> {
        build_event(wire.from, wire.payload).map_err(|e| Reject(format!("js event build: {e:?}")))
    }

    /// Transition delegated to the JS handler. On any JS error the state is left unchanged
    /// and no effects are produced (logged, non-fatal).
    fn step(&self, ctx: Ctx<'_, JsValue>, event: JsValue) -> Transition<JsValue, JsSend> {
        let current = ctx.state.clone();
        match call_handler(&self.handler, ctx.did, ctx.state, &event) {
            Ok(transition) => transition,
            Err(err) => {
                tracing::error!("js protocol {:?} step failed: {:?}", self.namespace, err);
                Transition::pure(current)
            }
        }
    }
}

/// JS protocol interpreter: each parsed handler effect is an overlay `send`.
pub struct JsShell;

#[async_trait::async_trait(?Send)]
impl Interpret for JsShell {
    type Effect = JsSend;

    async fn run(&self, scope: &Scope, effect: JsSend) -> crate::error::Result<Vec<Bytes>> {
        scope.send(effect.to, effect.payload).await?;
        Ok(Vec::new())
    }
}

/// Build the JS `event` object `{ from, payload }`.
fn build_event(from: Did, payload: &[u8]) -> Result<JsValue, JsValue> {
    let event_js = Object::new();
    Reflect::set(
        event_js.as_ref(),
        JsValue::from_str("from").as_ref(),
        JsValue::from_str(from.to_string().as_str()).as_ref(),
    )?;
    let payload = Uint8Array::from(payload);
    Reflect::set(
        event_js.as_ref(),
        JsValue::from_str("payload").as_ref(),
        payload.as_ref(),
    )?;
    Ok(event_js.into())
}

/// Call the JS handler and parse `{ state, effects }`.
fn call_handler(
    handler: &Function,
    did: Did,
    state: &JsValue,
    event: &JsValue,
) -> Result<Transition<JsValue, JsSend>, JsValue> {
    let ctx_js = Object::new();
    Reflect::set(
        ctx_js.as_ref(),
        JsValue::from_str("did").as_ref(),
        JsValue::from_str(did.to_string().as_str()).as_ref(),
    )?;
    Reflect::set(ctx_js.as_ref(), JsValue::from_str("state").as_ref(), state)?;

    let result = handler.call2(JsValue::NULL.as_ref(), ctx_js.as_ref(), event)?;

    let next_state = Reflect::get(result.as_ref(), JsValue::from_str("state").as_ref())?;
    let effects_value = Reflect::get(result.as_ref(), JsValue::from_str("effects").as_ref())?;
    let effects = parse_effects(effects_value)?;
    Ok(Transition::with(next_state, effects))
}

/// Parse the `effects` array returned by a JS handler into [`JsSend`]s; absent/empty → `ε`.
fn parse_effects(value: JsValue) -> Result<Vec<JsSend>, JsValue> {
    if value.is_null() || value.is_undefined() {
        return Ok(Vec::new());
    }
    let array = Array::from(value.as_ref());
    let mut effects = Vec::with_capacity(array.length() as usize);
    for item in array.iter() {
        let to = string_field(item.as_ref(), "to")?;
        let to = Did::from_str(to.as_str())
            .map_err(|_| JsValue::from_str("effect.to is not a valid did"))?;
        let payload_value = Reflect::get(item.as_ref(), JsValue::from_str("payload").as_ref())?;
        let payload = Uint8Array::new(payload_value.as_ref()).to_vec();
        effects.push(JsSend {
            to,
            payload: Bytes::from(payload),
        });
    }
    Ok(effects)
}

/// Read a required string field off a JS object.
fn string_field(object: &JsValue, key: &str) -> Result<String, JsValue> {
    Reflect::get(object, JsValue::from_str(key).as_ref())?
        .as_string()
        .ok_or_else(|| JsValue::from_str("expected a string field"))
}
