//! Application-owned launch page for the controlled WebView.

use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;
use web_sys::InputEvent;
use web_sys::MouseEvent;
use web_sys::SubmitEvent;
use yew::prelude::*;

use crate::styles;
use crate::webview;

const DEFAULT_TARGET: &str = "https://example.com/";

/// Render the local launch page before the popup becomes a controlled document.
#[function_component(WebviewShell)]
pub(crate) fn webview_shell() -> Html {
    let address = use_state(|| DEFAULT_TARGET.to_string());
    let status = use_state(|| "Enter an HTTP(S) address".to_string());

    let navigate = {
        let address = address.clone();
        let status = status.clone();
        Callback::<()>::from(move |_| {
            let requested = (*address).trim().to_string();
            let path = match webview::controlled_gateway_path(requested.as_str()) {
                Ok(path) => path,
                Err(error) => {
                    status.set(error);
                    return;
                }
            };

            status.set("Connecting through the local Rings gateway".to_string());
            let status = status.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match webview::prepare_browser_gateway().await {
                    Ok(()) => {
                        status.set("Request sent to the local Rings gateway".to_string());
                        let Some(window) = web_sys::window() else {
                            status.set("Unable to access the popup window".to_string());
                            return;
                        };
                        if let Err(error) = window.location().set_href(path.as_str()) {
                            let detail = error
                                .as_string()
                                .unwrap_or_else(|| "unknown browser error".to_string());
                            status.set(format!("Unable to open controlled page: {detail}"));
                        }
                    }
                    Err(error) => status.set(format!("gateway unavailable: {error}")),
                }
            });
        })
    };
    let on_address = {
        let address = address.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(input) = event
                .target()
                .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
            {
                address.set(input.value());
            }
        })
    };
    let on_submit = {
        let navigate = navigate.clone();
        Callback::from(move |event: SubmitEvent| {
            event.prevent_default();
            navigate.emit(());
        })
    };
    let on_close = Callback::from(move |_: MouseEvent| {
        if let Some(window) = web_sys::window() {
            let _closed = window.close();
        }
    });

    html! {
        <main class="webview-shell">
            <style>{ styles::app_css() }</style>
            <header class="webview-toolbar">
                <div class="webview-brand" aria-label="Rings WebView">
                    <img src="assets/icons/rings.svg" alt="" />
                    <span>{ "Rings WebView" }</span>
                </div>
                <form class="webview-address-form" onsubmit={on_submit}>
                    <label class="webview-address-label" for="webview-address">{ "Address" }</label>
                    <input
                        id="webview-address"
                        class="webview-address"
                        value={(*address).clone()}
                        placeholder="https://example.com/"
                        autocomplete="off"
                        spellcheck="false"
                        oninput={on_address}
                    />
                    <button class="webview-go-button" type="submit" aria-label="Open address" title="Open address">
                        <span aria-hidden="true">{ ">" }</span>
                    </button>
                </form>
                <p class="webview-status" aria-live="polite">{ (*status).clone() }</p>
                <button class="webview-icon-button webview-close-button" type="button" aria-label="Close WebView" title="Close WebView" onclick={on_close}>
                    <span aria-hidden="true">{ "x" }</span>
                </button>
            </header>
            <section class="webview-viewport" aria-label="Controlled web page" />
        </main>
    }
}
