//! Shared Yew form controls.

use wasm_bindgen::JsCast;
use web_sys::Event;
use web_sys::HtmlInputElement;
use web_sys::HtmlSelectElement;
use web_sys::HtmlTextAreaElement;
use web_sys::InputEvent;
use yew::prelude::*;

pub(crate) fn text_input(label: &'static str, state: UseStateHandle<String>) -> Html {
    text_field(label, state, None)
}

/// A text input that offers `suggestions` through a `<datalist>`: the value stays free text,
/// the list only shortens the common cases. `list_id` names the datalist and must be unique on
/// the page.
pub(crate) fn text_input_with_suggestions(
    label: &'static str,
    state: UseStateHandle<String>,
    list_id: &'static str,
    suggestions: &'static [&'static str],
) -> Html {
    text_field(
        label,
        state,
        Some(Suggestions {
            list_id,
            values: suggestions,
        }),
    )
}

/// A `<datalist>` and the id an input names it by.
struct Suggestions {
    list_id: &'static str,
    values: &'static [&'static str],
}

/// A labelled text field bound to `state`, with or without a datalist: the two public forms
/// differ only in that option.
fn text_field(
    label: &'static str,
    state: UseStateHandle<String>,
    suggestions: Option<Suggestions>,
) -> Html {
    let oninput = {
        let state = state.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(value) = input_value(&event) {
                state.set(value);
            }
        })
    };
    let list_id = suggestions.as_ref().map(|suggestions| suggestions.list_id);
    html! {
        <label class="field">
            <span>{ label }</span>
            <input value={(*state).clone()} list={list_id} {oninput} />
            if let Some(suggestions) = suggestions {
                <datalist id={suggestions.list_id}>
                    { for suggestions.values.iter().map(|value| html! { <option value={*value} /> }) }
                </datalist>
            }
        </label>
    }
}

pub(crate) fn textarea(label: &'static str, state: UseStateHandle<String>) -> Html {
    let oninput = {
        let state = state.clone();
        Callback::from(move |event: InputEvent| {
            if let Some(value) = textarea_value(&event) {
                state.set(value);
            }
        })
    };
    html! {
        <label class="field">
            <span>{ label }</span>
            <textarea value={(*state).clone()} {oninput} />
        </label>
    }
}

pub(crate) fn readonly_textarea(label: &'static str, value: String) -> Html {
    html! {
        <label class="field payload-output">
            <span>{ label }</span>
            <textarea readonly=true value={value} placeholder="Waiting for generated SDP" />
        </label>
    }
}

fn input_value(event: &InputEvent) -> Option<String> {
    event
        .target()
        .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
        .map(|input| input.value())
}

fn textarea_value(event: &InputEvent) -> Option<String> {
    event
        .target()
        .and_then(|target| target.dyn_into::<HtmlTextAreaElement>().ok())
        .map(|input| input.value())
}

pub(crate) fn select_value(event: &Event) -> Option<String> {
    event
        .target()
        .and_then(|target| target.dyn_into::<HtmlSelectElement>().ok())
        .map(|select| select.value())
}
