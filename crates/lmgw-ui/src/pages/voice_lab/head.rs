//! The head line: the text-to-speech model, then what it is — its family,
//! the licence its package states (information only), whether the GPU hold
//! hands it to a fallback now — and Reload.

use leptos::prelude::*;

use super::page::{Facts, Lab};
use crate::widgets::ModelPicker;

#[component]
pub(super) fn Head(lab: Lab) -> impl IntoView {
    view! {
        <span class="lab-label">"Model"</span>
        <ModelPicker
            value=lab.alias
            tasks=lmgw_api_types::realtime::SPEECH_TASKS
            recent_key="voicelab"
        />
        {move || match lab.facts.get() {
            Facts::Loaded(f) => {
                let kind = match &f.row {
                    Some(r) => view! {
                        <span class="type-badge" title="the audio.cpp family of the local row">
                            {r.family.clone()}
                        </span>
                    }
                    .into_any(),
                    None => view! {
                        <span class="type-badge" title="a cloud model: lmgw sends it no seed and no language">
                            "cloud"
                        </span>
                    }
                    .into_any(),
                };
                let licence = f.licence.as_ref().and_then(|l| {
                    let label = l.label()?.to_string();
                    let title = format!(
                        "The licence the model's package states{} — shown for information, never \
                         used to filter",
                        l.link.as_deref().map(|u| format!(": {u}")).unwrap_or_default()
                    );
                    Some(view! { <span class="chip off" title=title>{format!("licence: {label}")}</span> })
                });
                let held = match (&f.fallback, f.held) {
                    (Some(fb), held) => Some(view! {
                        <span
                            class="chip live"
                            title=if held {
                                "The GPU hold pauses this model now: its configured fallback answers"
                            } else {
                                "This model cannot take the card now: its configured fallback answers"
                            }
                        >
                            <i class="dot"></i>
                            {format!("answers through {fb}")}
                        </span>
                    }
                    .into_any()),
                    (None, true) => Some(view! {
                        <span class="chip err" title="The GPU hold pauses this model and it has no fallback">
                            <i class="dot"></i>
                            "held — no fallback"
                        </span>
                    }
                    .into_any()),
                    (None, false) => None,
                };
                view! { {kind} {licence} {held} }.into_any()
            }
            Facts::Loading => view! { <span class="dim mini-note">"reading…"</span> }.into_any(),
            Facts::Failed(_) | Facts::None => ().into_any(),
        }}
        <span class="spacer"></span>
        <button
            class="btn ghost"
            title="Read the model's facts, the rounds and the voice library again"
            on:click=move |_| {
                lab.load_facts();
                lab.load_rounds();
                lab.load_samples();
                lab.load_sentences();
            }
        >
            "Reload"
        </button>
    }
}
