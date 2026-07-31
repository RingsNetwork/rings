use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use futures::future::AbortHandle;
use futures::future::Abortable;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::future_to_promise;

use super::browser_failure_with;
use super::dispatch_browser_request;
use super::WebviewNode;

thread_local! {
    static BROWSER_GATEWAY: RefCell<Option<BrowserGatewayBinding>> = const { RefCell::new(None) };
}

struct BrowserGatewayBinding {
    // This closure must outlive every Service Worker request sent to the active node.
    _handler: Closure<dyn FnMut(JsValue, JsValue) -> Promise>,
    _cancel: Closure<dyn FnMut(JsValue) -> bool>,
    pending: Rc<RefCell<BTreeMap<u64, PendingBrowserGatewayRequest>>>,
}

impl Drop for BrowserGatewayBinding {
    fn drop(&mut self) {
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        for request in pending.into_values() {
            request.abort.abort();
        }
    }
}

struct PendingBrowserGatewayRequest {
    generation: u64,
    abort: AbortHandle,
}

/// Open the local WebView popup without ever passing it a remote URL.
pub(crate) fn open_webview_popup() -> Result<(), String> {
    crate::browser_api::open_webview_popup()
}

/// Install the current node as the only Service Worker gateway executor.
pub(crate) fn install_browser_gateway(gateway: Option<WebviewNode>) -> Result<bool, String> {
    clear_browser_gateway();
    let Some(gateway) = gateway else {
        return Ok(false);
    };
    let pending = Rc::new(RefCell::new(BTreeMap::new()));
    let next_generation = Rc::new(Cell::new(0u64));
    let handler_pending = pending.clone();
    let handler_generation = next_generation.clone();
    let handler = Closure::wrap(Box::new(move |request: JsValue, request_id: JsValue| {
        let gateway = gateway.clone();
        let request_id = match browser_request_id(&request_id) {
            Ok(request_id) => request_id,
            Err(error) => {
                return future_to_promise(async move {
                    Ok::<JsValue, JsValue>(browser_failure_with(
                        400,
                        "invalid_gateway_request",
                        "Malformed Rings WebView gateway request.",
                        error,
                    ))
                });
            }
        };
        let generation = handler_generation.get().wrapping_add(1).max(1);
        handler_generation.set(generation);
        let (abort, registration) = AbortHandle::new_pair();
        if let Some(previous) =
            handler_pending
                .borrow_mut()
                .insert(request_id, PendingBrowserGatewayRequest {
                    generation,
                    abort,
                })
        {
            previous.abort.abort();
        }
        let request_pending = handler_pending.clone();
        future_to_promise(async move {
            let response = Abortable::new(dispatch_browser_request(gateway, request), registration)
                .await
                .unwrap_or_else(|_| {
                    browser_failure_with(
                        504,
                        "local_gateway_timeout",
                        "Local Rings node gateway timed out.",
                        "gateway request was cancelled by the Service Worker".to_string(),
                    )
                });
            let mut pending = request_pending.borrow_mut();
            if pending.get(&request_id).map(|request| request.generation) == Some(generation) {
                pending.remove(&request_id);
            }
            Ok::<JsValue, JsValue>(response)
        })
    }) as Box<dyn FnMut(JsValue, JsValue) -> Promise>);
    let cancel_pending = pending.clone();
    let cancel = Closure::wrap(Box::new(move |request_id: JsValue| {
        let Ok(request_id) = browser_request_id(&request_id) else {
            return false;
        };
        let Some(request) = cancel_pending.borrow_mut().remove(&request_id) else {
            return false;
        };
        request.abort.abort();
        true
    }) as Box<dyn FnMut(JsValue) -> bool>);
    let bridge = Object::new();
    crate::browser_api::js_set(&bridge, "handle", handler.as_ref())?;
    crate::browser_api::js_set(&bridge, "cancel", cancel.as_ref())?;
    Reflect::set(
        &js_sys::global(),
        &JsValue::from_str("RingsWebviewGateway"),
        bridge.as_ref(),
    )
    .map_err(crate::browser_api::js_error_label)?;
    BROWSER_GATEWAY.with(|slot| {
        *slot.borrow_mut() = Some(BrowserGatewayBinding {
            _handler: handler,
            _cancel: cancel,
            pending,
        });
    });
    Ok(true)
}

pub(super) fn browser_request_id(value: &JsValue) -> Result<u64, String> {
    let value = value
        .as_f64()
        .ok_or_else(|| "gateway request id must be numeric".to_string())?;
    if !value.is_finite()
        || value.fract() != 0.0
        || !(1.0..=9_007_199_254_740_991.0).contains(&value)
    {
        return Err("gateway request id must be a positive safe integer".to_string());
    }
    Ok(value as u64)
}

/// Remove the browser host binding when the local node stops.
pub(crate) fn clear_browser_gateway() {
    BROWSER_GATEWAY.with(|slot| {
        *slot.borrow_mut() = None;
    });
    let _deleted =
        Reflect::delete_property(&js_sys::global(), &JsValue::from_str("RingsWebviewGateway"));
}

/// Register the active application page as the Service Worker's gateway host.
pub(crate) async fn register_browser_gateway() -> Result<(), String> {
    let bridge = crate::browser_api::js_global_prop("RingsWebviewHost")?;
    let registered = crate::browser_api::js_call0(&bridge, "registerGatewayHost")?;
    let ready = crate::browser_api::await_js(registered).await?;
    if ready.as_bool() == Some(false) {
        return Err("Service Worker rejected gateway host registration".to_string());
    }
    Ok(())
}
