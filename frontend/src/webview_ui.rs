//! Application-owned launch surface for the controlled WebView.

use yew::prelude::*;

use crate::styles;

const DEFAULT_TARGET: &str = "https://example.com/";

/// Render only the local page surface. The browser chrome and debug console are
/// provided by `assets/webview-overlay.js` in every WebView document.
#[function_component(WebviewShell)]
pub(crate) fn webview_shell() -> Html {
    html! {
        <main
            class="webview-shell"
            data-rings-webview-default-target={DEFAULT_TARGET}
            aria-label="Rings WebView"
        >
            <style>{ styles::app_css() }</style>
            <section class="webview-viewport" aria-label="Controlled web page" />
        </main>
    }
}
