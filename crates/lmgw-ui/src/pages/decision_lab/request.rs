//! The Request panel (labs-store design §2.4): the exact body for the head
//! model as written, built by `DecisionForm::body` — the function the server
//! builds each cell with — with each image's payload elided ("data:image/png;
//! base64,… 1.4 MB", a kept image's from the head it is sent with) and byte
//! for byte otherwise.
//!
//! "Edit as JSON" copies the body, payloads included, into a textarea that
//! is from then on the request: the override replaces the body, with only
//! `model` rewritten per compared model. Its only check is "a JSON object":
//! a lab exists to see what the route says, so a malformed question goes
//! out. A kept image (an opened group's) stays `lab-artifact:<id>` in it:
//! the server sends the data URL that image first came as.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::form::ENDPOINT;
use lmgw_api_types::decision_lab::Variant;
use lmgw_api_types::lab_store::ARTIFACT_REF_PREFIX;

use super::form_state::Img;
use super::page::Lab;

/// Copy the body, payloads included, into the override.
fn edit_as_json(lab: Lab) {
    let form = untrack(|| lab.form.text_draft());
    let model = lab.alias.get_untracked();
    let images: Vec<String> = lab
        .form
        .images
        .with_untracked(|v| v.iter().map(Img::sent).collect());
    match form.body(&model, Variant::AsWritten, &images) {
        Ok(b) => lab.override_text.set(Some(b)),
        Err(e) => lab.toasts.err(format!("the form builds no body yet: {e}")),
    }
}

#[component]
pub(super) fn RequestCard(lab: Lab) -> impl IntoView {
    let preview = Memo::new(move |_| {
        let form = lab.form.text_draft();
        let model = lab.alias.get();
        let images: Vec<String> = lab.form.images.with(|v| v.iter().map(Img::shown).collect());
        match form.body(&model, Variant::AsWritten, &images) {
            Ok(b) => b,
            Err(e) => format!("⚠ {e}"),
        }
    });
    let has = Memo::new(move |_| lab.override_text.with(Option::is_some));
    let names_kept = Memo::new(move |_| {
        lab.override_text.with(|o| {
            o.as_deref()
                .is_some_and(|t| t.contains(&format!("\"{ARTIFACT_REF_PREFIX}")))
        })
    });
    view! {
        <details class="card req-card wrap dl-request" open>
            <summary>
                "Request — " <span class="mono-sm">{ENDPOINT}</span>
                {move || lab.override_text.with(Option::is_some).then_some(" · override")}
            </summary>
            {move || {
                if !has.get() {
                    return view! {
                        <pre class="preset">{move || preview.get()}</pre>
                        <div class="row">
                            <button
                                type="button"
                                class="btn ghost sm"
                                title="Copy this body, image payloads included, into an editable override that is sent instead of the form"
                                on:click=move |_| edit_as_json(lab)
                            >
                                "Edit as JSON"
                            </button>
                        </div>
                    }
                        .into_any();
                }
                // Read once: the textarea owns the text while it is typed in.
                let text = lab.override_text.get_untracked().unwrap_or_default();
                view! {
                    <div class="notice warn dl-override-banner">
                        <b>"The override is sent; the form is not"</b>
                        "Each model asked gets these bytes with only "
                        <span class="mono-sm">"model"</span>
                        " rewritten. Perturb is off while it is in effect."
                        {move || {
                            names_kept
                                .get()
                                .then(|| {
                                    view! {
                                        " A "
                                        <span class="mono-sm">"lab-artifact:<id>"</span>
                                        " in images or files is a kept image: it goes out as the data URL it first came as."
                                    }
                                })
                        }}
                        " "
                        <button
                            type="button"
                            class="btn ghost sm"
                            on:click=move |_| lab.override_text.set(None)
                        >
                            "Drop override"
                        </button>
                    </div>
                    <textarea
                        class="input ta mono dl-override"
                        spellcheck="false"
                        prop:value=text
                        on:input=move |ev| lab.override_text.set(Some(event_target_value(&ev)))
                    ></textarea>
                    {move || {
                        let ok = lab.override_ok.get() == Some(true);
                        view! {
                            <p class="mini-note" class:dl-bad-line=!ok>
                                {if ok {
                                    "a JSON object: sent as typed"
                                } else {
                                    "⚠ not a JSON object: Ask is off until it is one"
                                }}
                            </p>
                        }
                    }}
                }
                    .into_any()
            }}
        </details>
    }
}
