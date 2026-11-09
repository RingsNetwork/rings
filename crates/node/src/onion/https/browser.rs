use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AbortController;

use super::limits::drive_cancellable_https_fetch;
use super::limits::CancellableHttpsFetch;
use super::limits::FetchCancelTarget;
use super::normalize_method;
use super::OnionHttpsRequest;
use super::OnionHttpsResponse;
use crate::error::Error;
use crate::error::Result;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::target::validate_public_ip_literal;

const HTTPS_EXIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct BrowserFetchCancelTarget {
    controller: AbortController,
}

impl FetchCancelTarget for BrowserFetchCancelTarget {
    type Body = JsValue;
    type Reader = JsValue;

    fn cancel_body(&mut self, body: &Self::Body) -> bool {
        cancel_js_stream(body)
    }

    fn cancel_reader(&mut self, reader: &Self::Reader) -> bool {
        cancel_js_stream(reader)
    }

    fn abort(&mut self) {
        self.controller.abort();
    }
}

struct BrowserHttpsFetch {
    promise: Option<JsValue>,
}

impl CancellableHttpsFetch for BrowserHttpsFetch {
    type Response = JsValue;
    type Body = JsValue;
    type Reader = JsValue;
    type Cancel = BrowserFetchCancelTarget;

    async fn await_response(&mut self) -> Result<Self::Response> {
        let promise = self
            .promise
            .take()
            .ok_or_else(|| Error::HttpRequestError("fetch promise already awaited".to_string()))?;
        JsFuture::from(Promise::from(promise))
            .await
            .map_err(js_error)
    }

    fn response_status(&self, response: &Self::Response) -> Result<f64> {
        Reflect::get(response.as_ref(), JsValue::from_str("status").as_ref())
            .map_err(js_error)?
            .as_f64()
            .ok_or_else(|| {
                Error::HttpRequestError("fetch response status is not numeric".to_string())
            })
    }

    fn response_body(&self, response: &Self::Response) -> Result<Option<Self::Body>> {
        let body = Reflect::get(response.as_ref(), JsValue::from_str("body").as_ref())
            .map_err(js_error)?;
        if body.is_null() || body.is_undefined() {
            Ok(None)
        } else {
            Ok(Some(body))
        }
    }

    fn response_headers(&self, response: &Self::Response) -> Result<Vec<(String, String)>> {
        collect_headers(response)
    }

    fn body_reader(&self, body: &Self::Body) -> Result<Self::Reader> {
        let get_reader = Reflect::get(body.as_ref(), JsValue::from_str("getReader").as_ref())
            .map_err(js_error)?
            .dyn_into::<Function>()
            .map_err(js_error)?;
        get_reader.call0(body.as_ref()).map_err(js_error)
    }

    async fn read_chunk(&self, reader: &Self::Reader) -> Result<Option<Vec<u8>>> {
        let read = Reflect::get(reader.as_ref(), JsValue::from_str("read").as_ref())
            .map_err(js_error)?
            .dyn_into::<Function>()
            .map_err(js_error)?;
        loop {
            let chunk = JsFuture::from(Promise::from(
                read.call0(reader.as_ref()).map_err(js_error)?,
            ))
            .await
            .map_err(js_error)?;
            let done = Reflect::get(chunk.as_ref(), JsValue::from_str("done").as_ref())
                .map_err(js_error)?
                .as_bool()
                .unwrap_or(false);
            if done {
                return Ok(None);
            }
            let value = Reflect::get(chunk.as_ref(), JsValue::from_str("value").as_ref())
                .map_err(js_error)?;
            if value.is_null() || value.is_undefined() {
                continue;
            }
            return Ok(Some(Uint8Array::new(value.as_ref()).to_vec()));
        }
    }
}

pub(super) async fn execute_https_request(
    url: &str,
    target: &OnionProxyTarget,
    request: &OnionHttpsRequest,
    max_body_bytes: u64,
    record_bytes: impl Fn(u64) -> Result<()>,
) -> Result<OnionHttpsResponse> {
    validate_public_ip_literal(target)?;
    let global = js_sys::global();
    let fetch = Reflect::get(global.as_ref(), JsValue::from_str("fetch").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let controller = AbortController::new().map_err(js_error)?;
    let signal = controller.signal();
    let init = fetch_init(request, signal.as_ref())?;
    let cancel_target = BrowserFetchCancelTarget { controller };
    let promise = fetch
        .call2(
            global.as_ref(),
            JsValue::from_str(url).as_ref(),
            init.as_ref(),
        )
        .map_err(js_error)?;
    drive_cancellable_https_fetch(
        BrowserHttpsFetch {
            promise: Some(promise),
        },
        cancel_target,
        max_body_bytes,
        HTTPS_EXIT_REQUEST_TIMEOUT,
        record_bytes,
    )
    .await
}

fn cancel_js_stream(stream: &JsValue) -> bool {
    let Ok(cancel) = Reflect::get(stream, JsValue::from_str("cancel").as_ref()) else {
        return false;
    };
    let Ok(cancel) = cancel.dyn_into::<Function>() else {
        return false;
    };
    let _ = cancel.call0(stream);
    true
}

fn fetch_init(request: &OnionHttpsRequest, signal: &JsValue) -> Result<Object> {
    let init = Object::new();
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("method").as_ref(),
        JsValue::from_str(normalize_method(&request.method).as_str()).as_ref(),
    )
    .map_err(js_error)?;
    let headers = Object::new();
    for (name, value) in &request.headers {
        Reflect::set(
            headers.as_ref(),
            JsValue::from_str(name).as_ref(),
            JsValue::from_str(value).as_ref(),
        )
        .map_err(js_error)?;
    }
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("headers").as_ref(),
        headers.as_ref(),
    )
    .map_err(js_error)?;
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("credentials").as_ref(),
        JsValue::from_str("omit").as_ref(),
    )
    .map_err(js_error)?;
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("referrerPolicy").as_ref(),
        JsValue::from_str("no-referrer").as_ref(),
    )
    .map_err(js_error)?;
    Reflect::set(
        init.as_ref(),
        JsValue::from_str("redirect").as_ref(),
        JsValue::from_str("error").as_ref(),
    )
    .map_err(js_error)?;
    Reflect::set(init.as_ref(), JsValue::from_str("signal").as_ref(), signal).map_err(js_error)?;
    if !request.body.is_empty() {
        let body = Uint8Array::from(request.body.as_slice());
        Reflect::set(
            init.as_ref(),
            JsValue::from_str("body").as_ref(),
            body.as_ref(),
        )
        .map_err(js_error)?;
    }
    Ok(init)
}

fn collect_headers(response: &JsValue) -> Result<Vec<(String, String)>> {
    let headers =
        Reflect::get(response, JsValue::from_str("headers").as_ref()).map_err(js_error)?;
    let for_each = Reflect::get(headers.as_ref(), JsValue::from_str("forEach").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let pairs = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let pairs_for_callback = pairs.clone();
    let callback = Closure::wrap(Box::new(move |value: JsValue, name: JsValue| {
        if let (Some(name), Some(value)) = (name.as_string(), value.as_string()) {
            pairs_for_callback.borrow_mut().push((name, value));
        }
    }) as Box<dyn FnMut(JsValue, JsValue)>);
    for_each
        .call1(headers.as_ref(), callback.as_ref().unchecked_ref())
        .map_err(js_error)?;
    drop(callback);
    let collected = pairs.borrow().clone();
    Ok(collected)
}

fn js_error(error: JsValue) -> Error {
    Error::JsError(format!("{error:?}"))
}
