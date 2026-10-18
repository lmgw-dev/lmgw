//! The state editor (labs-store design §2.3): one textarea under a Text |
//! JSON seg. Switching keeps the text; in JSON mode the line under it says
//! whether the text parses, and a JSON state goes out exactly as typed.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::StateMode;
use serde::de::IgnoredAny;

use super::page::Lab;
use crate::fmt::grouped;

/// The line under the textarea: the size, and in JSON mode whether it
/// parses. `(text, bad)`.
pub(super) fn state_line(mode: StateMode, text: &str) -> (String, bool) {
    let size = format!("{} characters", grouped(text.chars().count() as u64));
    match mode {
        StateMode::Text => (format!("{size} · sent as a JSON string"), false),
        StateMode::Json if text.trim().is_empty() => (size, false),
        StateMode::Json => match serde_json::from_str::<IgnoredAny>(text) {
            Ok(_) => (format!("{size} · valid JSON, sent as typed"), false),
            Err(e) => (format!("{size} · not JSON: {e}"), true),
        },
    }
}

#[component]
pub(super) fn StateCard(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let seg = move |mode: StateMode, label: &'static str, title: &'static str| {
        view! {
            <button
                type="button"
                class="seg-btn"
                class:active=move || f.mode.get() == mode
                title=title
                on:click=move |_| f.mode.set(mode)
            >
                {label}
            </button>
        }
    };
    view! {
        <div class="card dl-state">
            <div class="dl-card-head">
                <h3 class="lab-h3">"State"</h3>
                <div class="seg">
                    {seg(StateMode::Text, "Text", "Plain text, sent as a JSON string")}
                    {seg(
                        StateMode::Json,
                        "JSON",
                        "JSON text (an object, an array, null for the types that take no state), sent as typed",
                    )}
                </div>
            </div>
            <textarea
                class="input ta mono dl-state-ta"
                spellcheck="false"
                placeholder=move || match f.mode.get() {
                    StateMode::Text => "What the questions are about: a message, a ticket, a transcript…",
                    StateMode::Json => "[{\"role\": \"user\", \"content\": \"…\"}]",
                }
                prop:value=move || f.state.get()
                on:input=move |ev| f.state.set(event_target_value(&ev))
            ></textarea>
            {move || {
                let (text, bad) = f.state.with(|s| state_line(f.mode.get(), s));
                view! {
                    <p class="mini-note dl-state-line" class:bad=bad>
                        {text}
                    </p>
                }
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_says_whether_json_parses() {
        assert_eq!(
            state_line(StateMode::Text, "hi"),
            ("2 characters · sent as a JSON string".to_string(), false)
        );
        assert!(!state_line(StateMode::Json, "null").1);
        let (t, bad) = state_line(StateMode::Json, "{\"a\":");
        assert!(bad);
        assert!(t.contains("not JSON"), "{t}");
    }
}
