#![warn(missing_docs)]
//! JavaScript protocol adapter (browser).
//!
//! Bridges a JS handler into the same pure [`Protocol`] model so the browser sees the
//! exact abstraction native does: state lives in the runtime, the handler is a pure
//! transition returning `{ state, effects }`, and the [`Interpreter`] runs the effects.
//!
//! The JS handler has the shape
//!
//! ```text
//!   handler : (Ctx, Event) → { state, effects }
//!     Ctx    = { did: string, state: any }
//!     Event  = { from: string, payload: Uint8Array }
//!     effects: Array<{ to: string, namespace: string, payload: Uint8Array }>
//! ```
//!
//! i.e. `step : (Ctx S, Event) → Transition S` with `S = any` (an opaque JS value).
//! The handler must be pure (no IO); side effects are returned and interpreted.

use std::str::FromStr;

use bytes::Bytes;
use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_core::dht::Did;
use wasm_bindgen::JsValue;

use crate::backend::ext::Ctx;
use crate::backend::ext::Effect;
use crate::backend::ext::Event;
use crate::backend::ext::Protocol;
use crate::backend::ext::Transition;

/// A protocol whose pure transition is a JS function. State is an opaque [`JsValue`].
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

    fn namespace(&self) -> &str {
        self.namespace.as_str()
    }

    fn init(&self) -> JsValue {
        self.initial.clone()
    }

    /// Pure transition delegated to the JS handler. On any JS error the state is left
    /// unchanged and no effects are produced (logged, non-fatal).
    fn step(&self, ctx: Ctx<'_, JsValue>, event: &Event) -> Transition<JsValue> {
        let current = ctx.state.clone();
        match call_handler(&self.handler, ctx.did, ctx.state, event) {
            Ok(transition) => transition,
            Err(err) => {
                tracing::error!("js protocol {:?} step failed: {:?}", self.namespace, err);
                Transition::pure(current)
            }
        }
    }
}

/// Call the JS handler and parse `{ state, effects }`.
/// `call_handler : (Function, Did, S, Event) ⇀ Transition S`.
fn call_handler(
    handler: &Function,
    did: Did,
    state: &JsValue,
    event: &Event,
) -> Result<Transition<JsValue>, JsValue> {
    let ctx_js = Object::new();
    Reflect::set(
        ctx_js.as_ref(),
        JsValue::from_str("did").as_ref(),
        JsValue::from_str(did.to_string().as_str()).as_ref(),
    )?;
    Reflect::set(ctx_js.as_ref(), JsValue::from_str("state").as_ref(), state)?;

    let event_js = Object::new();
    Reflect::set(
        event_js.as_ref(),
        JsValue::from_str("from").as_ref(),
        JsValue::from_str(event.from.to_string().as_str()).as_ref(),
    )?;
    let payload = Uint8Array::from(event.payload.as_ref());
    Reflect::set(
        event_js.as_ref(),
        JsValue::from_str("payload").as_ref(),
        payload.as_ref(),
    )?;

    let result = handler.call2(JsValue::NULL.as_ref(), ctx_js.as_ref(), event_js.as_ref())?;

    let next_state = Reflect::get(result.as_ref(), JsValue::from_str("state").as_ref())?;
    let effects_value = Reflect::get(result.as_ref(), JsValue::from_str("effects").as_ref())?;
    let effects = parse_effects(effects_value)?;
    Ok(Transition::with(next_state, effects))
}

/// Parse the `effects` array returned by a JS handler into [`Effect`]s.
/// `parse_effects : Array → [Effect]`; absent/empty yields `ε`.
fn parse_effects(value: JsValue) -> Result<Vec<Effect>, JsValue> {
    if value.is_null() || value.is_undefined() {
        return Ok(Vec::new());
    }
    let array = Array::from(value.as_ref());
    let mut effects = Vec::with_capacity(array.length() as usize);
    for item in array.iter() {
        let to = string_field(item.as_ref(), "to")?;
        let to = Did::from_str(to.as_str())
            .map_err(|_| JsValue::from_str("effect.to is not a valid did"))?;
        let namespace = string_field(item.as_ref(), "namespace")?;
        let payload_value = Reflect::get(item.as_ref(), JsValue::from_str("payload").as_ref())?;
        let payload = Uint8Array::new(payload_value.as_ref()).to_vec();
        effects.push(Effect::Send {
            to,
            namespace,
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
