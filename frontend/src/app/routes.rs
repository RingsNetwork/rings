//! Hash routing of the shell: which page and dialog a URL addresses, and how a shell state is
//! written back to the URL.
//!
//! The shell is hash-routed so static hosting serves every screen unchanged; the WebView
//! gateway alone is path-routed. `route_for_hash` parses and `shell_route_fragment` prints, and
//! the test below holds them inverse on every page. The extension origin has no landing page,
//! so there the parsed page is always the console.

use wasm_bindgen::JsValue;
use web_sys::Window;
use yew::prelude::*;

use crate::controls::ActiveDialog;
use crate::controls::ShellPage;
use crate::extension;
use crate::webview;

pub(super) fn initial_shell_page() -> ShellPage {
    current_shell_route().page
}

pub(super) fn initial_shell_dialog() -> ActiveDialog {
    current_shell_route().dialog
}

/// The shell state a URL addresses.
#[derive(Clone, Copy)]
pub(super) struct ShellRoute {
    pub(super) page: ShellPage,
    pub(super) dialog: ActiveDialog,
}

pub(super) fn current_shell_route() -> ShellRoute {
    let route = routed_shell_route();
    if extension::extension_node_bridge().is_some() {
        return ShellRoute {
            page: ShellPage::Console,
            dialog: route
                .map(|route| route.dialog)
                .unwrap_or(ActiveDialog::None),
        };
    }
    route.unwrap_or(ShellRoute {
        page: ShellPage::Home,
        dialog: ActiveDialog::None,
    })
}

fn routed_shell_route() -> Option<ShellRoute> {
    let location = web_sys::window()?.location();
    let pathname = location.pathname().ok()?;
    if is_webview_path(pathname.as_str()) {
        return Some(ShellRoute {
            page: ShellPage::Webview,
            dialog: ActiveDialog::None,
        });
    }
    let hash = location.hash().ok()?;
    route_for_hash(hash.as_str())
}

fn is_webview_path(pathname: &str) -> bool {
    pathname == "/webview" || pathname.starts_with(webview::GATEWAY_PREFIX)
}

fn route_for_hash(hash: &str) -> Option<ShellRoute> {
    match hash.trim_start_matches('#').trim_start_matches('/') {
        "" | "home" => Some(ShellRoute {
            page: ShellPage::Home,
            dialog: ActiveDialog::None,
        }),
        "guide" => Some(ShellRoute {
            page: ShellPage::Guide,
            dialog: ActiveDialog::None,
        }),
        "node" => Some(ShellRoute {
            page: ShellPage::Console,
            dialog: ActiveDialog::None,
        }),
        "webview" => Some(ShellRoute {
            page: ShellPage::Webview,
            dialog: ActiveDialog::None,
        }),
        "node/settings" | "settings" => Some(ShellRoute {
            page: ShellPage::Console,
            dialog: ActiveDialog::Settings,
        }),
        "node/workbench" | "workbench" => Some(ShellRoute {
            page: ShellPage::Console,
            dialog: ActiveDialog::Workbench,
        }),
        _ => None,
    }
}

pub(super) fn navigate_shell_page(
    page: ShellPage,
    active_page: &UseStateHandle<ShellPage>,
    active_dialog: &UseStateHandle<ActiveDialog>,
) {
    let route = current_shell_route();
    if **active_page == page && route.page == page && route.dialog == ActiveDialog::None {
        return;
    }
    write_shell_route(page, ActiveDialog::None, false);
    active_page.set(page);
    active_dialog.set(ActiveDialog::None);
}

pub(super) fn open_shell_dialog(
    dialog: ActiveDialog,
    active_page: &UseStateHandle<ShellPage>,
    active_dialog: &UseStateHandle<ActiveDialog>,
) {
    if !dialog.is_open() {
        close_shell_dialog(active_page, active_dialog);
        return;
    }
    let replace = (**active_dialog).is_open();
    write_shell_route(ShellPage::Console, dialog, replace);
    active_page.set(ShellPage::Console);
    active_dialog.set(dialog);
}

pub(super) fn close_shell_dialog(
    active_page: &UseStateHandle<ShellPage>,
    active_dialog: &UseStateHandle<ActiveDialog>,
) {
    if !(**active_dialog).is_open() {
        return;
    }
    let page = current_shell_route().page;
    write_shell_route(page, ActiveDialog::None, true);
    active_page.set(page);
    active_dialog.set(ActiveDialog::None);
}

pub(super) fn clear_shell_dialog_route() {
    let route = current_shell_route();
    if route.dialog.is_open() {
        write_shell_route(route.page, ActiveDialog::None, true);
    }
}

fn write_shell_route(page: ShellPage, dialog: ActiveDialog, replace: bool) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(target) = shell_route_url(&window, page, dialog) else {
        return;
    };
    if !replace && current_path_search_hash(&window) == Some(target.clone()) {
        return;
    }
    let Ok(history) = window.history() else {
        return;
    };
    if replace {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&target));
    } else {
        let _ = history.push_state_with_url(&JsValue::NULL, "", Some(&target));
    }
}

fn shell_route_url(window: &Window, page: ShellPage, dialog: ActiveDialog) -> Option<String> {
    let location = window.location();
    let mut target = location.pathname().ok()?;
    if let Ok(search) = location.search() {
        target.push_str(&search);
    }
    if let Some(fragment) = shell_route_fragment(page, dialog) {
        target.push('#');
        target.push_str(fragment);
    }
    Some(target)
}

/// The fragment a shell state is written as. The landing page is the empty fragment (the
/// site root), every other page its slug, and a dialog is addressed under the console; the
/// round-trip test holds this printer and `route_for_hash` inverse to each other.
fn shell_route_fragment(page: ShellPage, dialog: ActiveDialog) -> Option<&'static str> {
    match (page, dialog) {
        (ShellPage::Home, ActiveDialog::None) => None,
        (page, ActiveDialog::None) => Some(page.slug()),
        (_, ActiveDialog::Settings) => Some("node/settings"),
        (_, ActiveDialog::Workbench) => Some("node/workbench"),
    }
}

fn current_path_search_hash(window: &Window) -> Option<String> {
    let location = window.location();
    let mut current = location.pathname().ok()?;
    if let Ok(search) = location.search() {
        current.push_str(&search);
    }
    if let Ok(hash) = location.hash() {
        current.push_str(&hash);
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `route_for_hash ∘ fragment = id` on every page without a dialog: the fragment a page
    /// writes is the fragment that reads back as that page, the landing page included, whose
    /// fragment is empty.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_page_fragments_round_trip_through_the_hash_router() {
        for page in [
            ShellPage::Home,
            ShellPage::Guide,
            ShellPage::Console,
            ShellPage::Webview,
        ] {
            let fragment = shell_route_fragment(page, ActiveDialog::None).unwrap_or("");
            let route = route_for_hash(&format!("#{fragment}"));
            assert!(
                matches!(route, Some(ShellRoute { page: routed, dialog: ActiveDialog::None }) if routed == page),
                "fragment {fragment:?} did not read back as its page"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_guide_hash_routes_to_the_guide_page() {
        assert!(matches!(
            route_for_hash("#guide"),
            Some(ShellRoute {
                page: ShellPage::Guide,
                dialog: ActiveDialog::None
            })
        ));
        assert!(matches!(
            route_for_hash("#/guide"),
            Some(ShellRoute {
                page: ShellPage::Guide,
                dialog: ActiveDialog::None
            })
        ));
    }
}
