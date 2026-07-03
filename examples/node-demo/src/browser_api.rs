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

pub(crate) async fn open_debug_url(url: &str) -> Result<(), String> {
    match open_debug_url_with_extension_tabs("browser", url).await {
        Ok(()) => Ok(()),
        Err(_) => match open_debug_url_with_extension_tabs("chrome", url).await {
            Ok(()) => Ok(()),
            Err(_) => open_debug_url_with_window(url),
        },
    }
}

async fn open_debug_url_with_extension_tabs(namespace: &str, url: &str) -> Result<(), String> {
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
    if let Ok(promise) = opened.dyn_into::<Promise>() {
        JsFuture::from(promise).await.map_err(js_error_label)?;
    }
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
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}
