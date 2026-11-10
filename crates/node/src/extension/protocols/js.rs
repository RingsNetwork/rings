//! JavaScript protocol adapter (browser).
//!
//! Bridges a JS handler into the [`Protocol`] router so browser extensions share the same
//! registration and effect-routing surface as native protocols: state is threaded through the
//! router, the handler returns `{ state, effects }`, and [`JsShell`] interprets the admitted
//! outbound effects.
//!
//! ```text
//!   handler : (Ctx, Event) → { state, effects }
//!     Ctx    = { did: string, state: any }
//!     Event  = { from: string, payload: Uint8Array }
//!     effects: Array<{ to: string, payload: Uint8Array }>
//! ```
//!
//! # Capability & Isolation Boundary
//!
//! Unlike native Rust [`Protocol`] implementations—whose `step` type signature withholds
//! [`EffectScope`], async runtimes, and ambient network handles—[`JsProtocol`] invokes synchronous
//! JavaScript in the host realm (`window` / `WorkerGlobalScope`) inside both [`Protocol::decode`]
//! and [`Protocol::step`]:
//!
//! - **Enforced boundary (outbound overlay namespace):** [`parse_effects`] admits only
//!   `{ to: Did, payload: Uint8Array }` values with no `namespace` field, and [`JsShell::run`]
//!   sends exclusively through its bound [`EffectScope`]. A JS handler cannot forge `from` or
//!   emit an effect targeting another protocol namespace through the returned `effects` array.
//! - **Unchecked host I/O capability:** Rust cannot prevent the JS `handler` function from
//!   reading ambient clocks or randomness (`Date.now()`, `crypto.getRandomValues()`), mutating
//!   shared JS closures or cross-namespace JS state, accessing browser storage/DOM APIs, or
//!   invoking captured host/provider handles synchronously during `step`.
//! - **Partial-rollback hazard on JS exceptions:** If `handler.call2(...)` throws or returns a
//!   malformed transition, [`JsProtocol::step`] keeps the previous `Protocol::State` and emits no
//!   [`JsSend`] effects, but any external JS mutations or host side effects already performed by
//!   the callback before failing are **not** rolled back.
//! - **Replay contract:** Registering a [`JsProtocol`] grants trusted same-origin host execution.
//!   Deterministic trace replay and namespace state isolation hold only if the supplied JS handler
//!   treats `ctx.state` as immutable, avoids ambient time/entropy/I/O, and expresses all external
//!   actions through its returned `effects` array.

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
use crate::extension::ext::EffectScope;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;
use crate::extension::protocols::outbox::OverlayOutbox;

/// A JS handler effect: send `payload` to `to` over the overlay, under the protocol's own
/// namespace (the scope decides the namespace — a handler cannot choose another).
pub struct JsSend {
    to: Did,
    payload: Bytes,
}

/// A protocol whose transition is a JS function. State and event are opaque [`JsValue`]s.
///
/// # Security & Determinism Notes
///
/// Invoking `handler` inside [`Protocol::step`] exercises an **unchecked synchronous host
/// capability**: while returned [`JsSend`] effects are strictly confined to `namespace` by
/// [`JsShell`], the JS callback executes in the ambient JS realm and is not sandboxed against
/// reading wall-clock time/entropy, mutating external JS objects, or performing synchronous host
/// I/O during `step`. See the [module documentation](self) for the full isolation boundary.
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

/// JS protocol interpreter: each parsed handler effect is enqueued onto a bounded per-peer
/// asynchronous outbox so outbound network I/O never blocks `Runner::transition_gate`.
#[derive(Default)]
pub struct JsShell {
    outbox: OverlayOutbox,
}

impl JsShell {
    /// Construct a new JS protocol shell with an isolated per-peer send outbox.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait(?Send)]
impl Interpret for JsShell {
    type Effect = JsSend;

    async fn run(&self, scope: &EffectScope, effect: JsSend) -> crate::error::Result<Vec<Bytes>> {
        self.outbox
            .enqueue(scope.lifecycle(), effect.to, effect.payload)?;
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
