use std::cell::RefCell;
use std::rc::Rc;

use futures::future::Either;
use futures::FutureExt;
use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_core::utils::js_utils;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::AbortController;

use super::limits::checked_status_code;
use super::limits::reject_content_length_over_limit;
use super::limits::usize_to_u64;
use super::normalize_method;
use super::FetchResponse;
use super::OnionHttpsRequest;
use super::OnionHttpsRuntime;
use crate::error::Error;
use crate::error::Result;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::target::validate_public_ip_literal;
use crate::onion::OnionExitPolicy;

const HTTPS_EXIT_REQUEST_TIMEOUT_MS: i32 = 30_000;

pub(super) async fn execute_https_request(
    url: &str,
    target: &OnionProxyTarget,
    request: &OnionHttpsRequest,
    max_body_bytes: u64,
    runtime: &OnionHttpsRuntime,
    policy: &OnionExitPolicy,
) -> Result<FetchResponse> {
    validate_public_ip_literal(target)?;
    let global = js_sys::global();
    let fetch = Reflect::get(global.as_ref(), JsValue::from_str("fetch").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let controller = AbortController::new().map_err(js_error)?;
    let signal = controller.signal();
    let init = fetch_init(request, signal.as_ref())?;
    let promise = fetch
        .call2(
            global.as_ref(),
            JsValue::from_str(url).as_ref(),
            init.as_ref(),
        )
        .map_err(js_error)?;
    let fetch_task = async move {
        let response = JsFuture::from(Promise::from(promise))
            .await
            .map_err(js_error)?;
        let status = Reflect::get(response.as_ref(), JsValue::from_str("status").as_ref())
            .map_err(js_error)?
            .as_f64()
            .ok_or_else(|| {
                Error::HttpRequestError("fetch response status is not numeric".to_string())
            })
            .and_then(checked_status_code)?;
        let headers = collect_headers(&response)?;
        reject_content_length_over_limit(&headers, max_body_bytes)?;
        let body = response_body(&response, max_body_bytes, runtime, policy).await?;
        Ok::<FetchResponse, Error>(FetchResponse {
            status,
            headers,
            body,
        })
    };
    let timeout = js_utils::window_sleep(HTTPS_EXIT_REQUEST_TIMEOUT_MS).fuse();
    futures::pin_mut!(fetch_task, timeout);
    match futures::future::select(fetch_task, timeout).await {
        Either::Left((result, _)) => result,
        Either::Right((_, _)) => {
            controller.abort();
            Err(Error::HttpRequestError(
                "browser HTTPS proxy request timed out".to_string(),
            ))
        }
    }
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

async fn response_body(
    response: &JsValue,
    max_body_bytes: u64,
    runtime: &OnionHttpsRuntime,
    policy: &OnionExitPolicy,
) -> Result<Vec<u8>> {
    let body = Reflect::get(response, JsValue::from_str("body").as_ref()).map_err(js_error)?;
    if body.is_null() || body.is_undefined() {
        return Ok(Vec::new());
    }
    let get_reader = Reflect::get(body.as_ref(), JsValue::from_str("getReader").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let reader = get_reader.call0(body.as_ref()).map_err(js_error)?;
    let read = Reflect::get(reader.as_ref(), JsValue::from_str("read").as_ref())
        .map_err(js_error)?
        .dyn_into::<Function>()
        .map_err(js_error)?;
    let cancel = Reflect::get(reader.as_ref(), JsValue::from_str("cancel").as_ref())
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok());
    let mut body = Vec::new();
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
            break;
        }
        let value =
            Reflect::get(chunk.as_ref(), JsValue::from_str("value").as_ref()).map_err(js_error)?;
        if value.is_null() || value.is_undefined() {
            continue;
        }
        let bytes = Uint8Array::new(value.as_ref()).to_vec();
        let body_len = usize_to_u64(body.len())?;
        let bytes_len = usize_to_u64(bytes.len())?;
        if max_body_bytes > 0 && body_len.saturating_add(bytes_len) > max_body_bytes {
            if let Some(cancel) = &cancel {
                let _ = cancel.call0(reader.as_ref());
            }
            return Err(Error::NoPermission);
        }
        if let Err(error) = runtime.record_exit_bytes(policy, bytes_len) {
            if let Some(cancel) = &cancel {
                let _ = cancel.call0(reader.as_ref());
            }
            return Err(error);
        }
        body.extend_from_slice(bytes.as_slice());
    }
    Ok(body)
}

fn js_error(error: JsValue) -> Error {
    Error::JsError(format!("{error:?}"))
}
