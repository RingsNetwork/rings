//! Browser JavaScript API adapters used by the Yew demo.

use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

pub(crate) fn load_setting(key: &str) -> Option<String> {
    let storage = local_storage().ok()?;
    let get_item = js_method(&storage, "getItem").ok()?;
    get_item
        .call1(&storage, &JsValue::from_str(key))
        .ok()
        .and_then(|value| value.as_string())
}

pub(crate) fn save_setting(key: &str, value: &str) {
    let Some(storage) = local_storage().ok() else {
        return;
    };
    let Some(set_item) = js_method(&storage, "setItem").ok() else {
        return;
    };
    let _stored = set_item.call2(&storage, &JsValue::from_str(key), &JsValue::from_str(value));
}

fn local_storage() -> Result<JsValue, String> {
    let storage = Reflect::get(&js_sys::global(), &JsValue::from_str("localStorage"))
        .map_err(js_error_label)?;
    if storage.is_null() || storage.is_undefined() {
        Err("localStorage unavailable".to_string())
    } else {
        Ok(storage)
    }
}

pub(crate) async fn copy_text_to_clipboard(value: String) -> Result<(), String> {
    let navigator =
        Reflect::get(&js_sys::global(), &JsValue::from_str("navigator")).map_err(js_error_label)?;
    let clipboard =
        Reflect::get(&navigator, &JsValue::from_str("clipboard")).map_err(js_error_label)?;
    if clipboard.is_null() || clipboard.is_undefined() {
        return Err("clipboard API unavailable".to_string());
    }
    let write_text =
        Reflect::get(&clipboard, &JsValue::from_str("writeText")).map_err(js_error_label)?;
    let write_text = write_text
        .dyn_into::<Function>()
        .map_err(|_| "clipboard.writeText unavailable".to_string())?;
    let promise = write_text
        .call1(&clipboard, &JsValue::from_str(&value))
        .map_err(js_error_label)?
        .dyn_into::<Promise>()
        .map_err(|_| "clipboard.writeText did not return a promise".to_string())?;
    JsFuture::from(promise).await.map_err(js_error_label)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DebugUrlOpenResult {
    Opened,
    CopiedInternalUrl,
    ManualInternalUrl,
}

pub(crate) fn open_debug_url(url: &str) -> Result<DebugUrlOpenResult, String> {
    if open_debug_url_with_extension_tabs("browser", url).is_ok()
        || open_debug_url_with_extension_tabs("chrome", url).is_ok()
    {
        return Ok(DebugUrlOpenResult::Opened);
    }

    // Browser-internal pages (`chrome://`, `about:`) are intentionally blocked from ordinary
    // webpages. Avoid calling `window.open` for them, otherwise the user only sees a blocked
    // navigation and the console records a misleading local-resource failure.
    if is_browser_internal_url(url) {
        return Ok(if copy_text_to_clipboard_start(url).is_ok() {
            DebugUrlOpenResult::CopiedInternalUrl
        } else {
            DebugUrlOpenResult::ManualInternalUrl
        });
    }

    open_debug_url_with_window(url).map(|_| DebugUrlOpenResult::Opened)
}

/// Open the application-owned WebView shell in a named browser popup.
///
/// The caller supplies no remote target here: remote addresses are entered only inside the
/// controlled shell and become `/webview/` paths before an iframe receives them.
pub(crate) fn open_webview_popup() -> Result<(), String> {
    let window = web_sys::window().ok_or_else(|| "window unavailable".to_string())?;
    let location = window.location();
    let mut url = location.origin().map_err(js_error_label)?;
    url.push_str(location.pathname().map_err(js_error_label)?.as_str());
    if let Ok(search) = location.search() {
        url.push_str(search.as_str());
    }
    url.push_str("#webview");

    let window_value: JsValue = window.into();
    let open = js_method(&window_value, "open")?;
    let opened = open
        .call3(
            &window_value,
            &JsValue::from_str(url.as_str()),
            &JsValue::from_str("rings-webview"),
            &JsValue::from_str("popup=yes,width=1280,height=860,noopener"),
        )
        .map_err(js_error_label)?;
    if opened.is_null() || opened.is_undefined() {
        return Err("browser blocked the WebView popup".to_string());
    }
    Ok(())
}

fn open_debug_url_with_extension_tabs(namespace: &str, url: &str) -> Result<(), String> {
    let extension_api =
        Reflect::get(&js_sys::global(), &JsValue::from_str(namespace)).map_err(js_error_label)?;
    if extension_api.is_null() || extension_api.is_undefined() {
        return Err(format!("{namespace} extension API unavailable"));
    }
    let tabs = Reflect::get(&extension_api, &JsValue::from_str("tabs")).map_err(js_error_label)?;
    if tabs.is_null() || tabs.is_undefined() {
        return Err(format!("{namespace}.tabs unavailable"));
    }
    let create = Reflect::get(&tabs, &JsValue::from_str("create")).map_err(js_error_label)?;
    let create = create
        .dyn_into::<Function>()
        .map_err(|_| format!("{namespace}.tabs.create unavailable"))?;
    let options = Object::new();
    Reflect::set(&options, &JsValue::from_str("url"), &JsValue::from_str(url))
        .map_err(js_error_label)?;
    let opened = create
        .call1(&tabs, &options.into())
        .map_err(js_error_label)?;
    observe_promise_rejection(opened);
    Ok(())
}

fn open_debug_url_with_window(url: &str) -> Result<(), String> {
    let window =
        Reflect::get(&js_sys::global(), &JsValue::from_str("window")).map_err(js_error_label)?;
    if window.is_null() || window.is_undefined() {
        return Err("window unavailable".to_string());
    }
    let open = Reflect::get(&window, &JsValue::from_str("open")).map_err(js_error_label)?;
    let open = open
        .dyn_into::<Function>()
        .map_err(|_| "window.open unavailable".to_string())?;
    let opened = open
        .call2(
            &window,
            &JsValue::from_str(url),
            &JsValue::from_str("_blank"),
        )
        .map_err(js_error_label)?;
    if opened.is_null() || opened.is_undefined() {
        return Err("browser blocked the debug console tab".to_string());
    }
    Ok(())
}

fn is_browser_internal_url(url: &str) -> bool {
    url.starts_with("chrome://") || url.starts_with("about:")
}

fn copy_text_to_clipboard_start(value: &str) -> Result<(), String> {
    let navigator =
        Reflect::get(&js_sys::global(), &JsValue::from_str("navigator")).map_err(js_error_label)?;
    let clipboard =
        Reflect::get(&navigator, &JsValue::from_str("clipboard")).map_err(js_error_label)?;
    if clipboard.is_null() || clipboard.is_undefined() {
        return Err("clipboard API unavailable".to_string());
    }
    let write_text = js_method(&clipboard, "writeText")?;
    let result = write_text
        .call1(&clipboard, &JsValue::from_str(value))
        .map_err(js_error_label)?;
    observe_promise_rejection(result);
    Ok(())
}

fn observe_promise_rejection(value: JsValue) {
    let Ok(promise) = value.dyn_into::<Promise>() else {
        return;
    };
    let promise: JsValue = promise.into();
    let Ok(catch) = js_method(&promise, "catch") else {
        return;
    };
    let handler =
        wasm_bindgen::closure::Closure::once_into_js(|_error: JsValue| JsValue::UNDEFINED);
    let _ = catch.call1(&promise, &handler);
}

pub(crate) async fn await_js(value: JsValue) -> Result<JsValue, String> {
    JsFuture::from(Promise::from(value))
        .await
        .map_err(js_error_label)
}

pub(crate) fn js_global_prop(name: &str) -> Result<JsValue, String> {
    Reflect::get(&js_sys::global(), &JsValue::from_str(name)).map_err(js_error_label)
}

pub(crate) fn chrome_runtime_on_message() -> Option<JsValue> {
    let runtime = chrome_runtime()?;
    let on_message = js_prop(&runtime, "onMessage").ok()?;
    if on_message.is_null() || on_message.is_undefined() {
        None
    } else {
        Some(on_message)
    }
}

fn chrome_runtime() -> Option<JsValue> {
    let chrome = Reflect::get(&js_sys::global(), &JsValue::from_str("chrome")).ok()?;
    let runtime = js_prop(&chrome, "runtime").ok()?;
    if runtime.is_null() || runtime.is_undefined() {
        None
    } else {
        Some(runtime)
    }
}

pub(crate) fn is_callable(object: &JsValue, name: &str) -> bool {
    js_prop(object, name)
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok())
        .is_some()
}

pub(crate) fn js_method(object: &JsValue, name: &str) -> Result<Function, String> {
    js_prop(object, name)?
        .dyn_into::<Function>()
        .map_err(|_| format!("{name} is not callable"))
}

pub(crate) fn js_call0(object: &JsValue, name: &str) -> Result<JsValue, String> {
    js_method(object, name)?
        .call0(object)
        .map_err(|error| format!("{name} failed: {}", js_error_label(error)))
}

pub(crate) fn js_call1(object: &JsValue, name: &str, a: &JsValue) -> Result<JsValue, String> {
    js_method(object, name)?
        .call1(object, a)
        .map_err(|error| format!("{name} failed: {}", js_error_label(error)))
}

pub(crate) fn js_call2(
    object: &JsValue,
    name: &str,
    a: &JsValue,
    b: &JsValue,
) -> Result<JsValue, String> {
    js_method(object, name)?
        .call2(object, a, b)
        .map_err(|error| format!("{name} failed: {}", js_error_label(error)))
}

pub(crate) fn js_call3(
    object: &JsValue,
    name: &str,
    a: &JsValue,
    b: &JsValue,
    c: &JsValue,
) -> Result<JsValue, String> {
    js_method(object, name)?
        .call3(object, a, b, c)
        .map_err(|error| format!("{name} failed: {}", js_error_label(error)))
}

pub(crate) fn js_prop(object: &JsValue, name: &str) -> Result<JsValue, String> {
    Reflect::get(object, &JsValue::from_str(name)).map_err(js_error_label)
}

pub(crate) fn js_string_field(object: &JsValue, name: &str) -> Result<String, String> {
    js_prop(object, name)?
        .as_string()
        .ok_or_else(|| format!("missing string field {name}"))
}

pub(crate) fn js_bool_field(object: &JsValue, name: &str) -> Result<bool, String> {
    js_prop(object, name)?
        .as_bool()
        .ok_or_else(|| format!("missing bool field {name}"))
}

pub(crate) fn js_set(object: &Object, name: &str, value: &JsValue) -> Result<(), String> {
    Reflect::set(object, &JsValue::from_str(name), value)
        .map(|_| ())
        .map_err(js_error_label)
}

pub(crate) fn js_error_label(error: JsValue) -> String {
    if let Some(message) = js_error_message(&error) {
        return message;
    }
    if let Some(text) = error.as_string() {
        return compact_js_error_text(&text);
    }
    compact_js_error_text(&format!("{error:?}"))
}

fn js_error_message(error: &JsValue) -> Option<String> {
    let message = string_property(error, "message")?;
    let name = string_property(error, "name");
    match name.as_deref() {
        Some(name) if !name.is_empty() && name != "Error" && !message.starts_with(name) => {
            Some(format!("{name}: {message}"))
        }
        _ => Some(message),
    }
}

fn string_property(object: &JsValue, name: &str) -> Option<String> {
    Reflect::get(object, &JsValue::from_str(name))
        .ok()
        .and_then(|value| value.as_string())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn compact_js_error_text(raw: &str) -> String {
    let mut text = raw.trim();
    if let Some(inner) = text
        .strip_prefix("JsValue(")
        .and_then(|value| value.strip_suffix(')'))
    {
        text = inner.trim();
    }
    text.lines()
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown JavaScript error")
        .to_string()
}

#[cfg(test)]
mod tests {
    use js_sys::Error;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    #[wasm_bindgen_test]
    fn js_error_label_uses_error_message_without_stack() {
        let error = Error::new("Onion route error: no live onion exit offers service \"https\"");

        assert_eq!(
            js_error_label(error.into()),
            "Onion route error: no live onion exit offers service \"https\""
        );
    }

    #[wasm_bindgen_test]
    fn js_error_label_compacts_debug_fallback() {
        assert_eq!(
            compact_js_error_text("JsValue(Error: boom\n    at wasm-function[1])"),
            "Error: boom"
        );
    }

    #[wasm_bindgen_test]
    fn browser_internal_debug_urls_are_detected() {
        assert!(is_browser_internal_url("chrome://webrtc-internals/"));
        assert!(is_browser_internal_url("about:webrtc"));
        assert!(!is_browser_internal_url("https://example.test/"));
    }
}
