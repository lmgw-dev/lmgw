//! The form column: the voice (a description, a voice, or both — as the
//! model takes them), the text (the language's sentence, editable, savable
//! as that language's own) and the candidates (how many, from which seed).

use leptos::prelude::*;
use lmgw_api_types::voice_lab::{VoiceDescriptionMode, VoiceSentences};
use serde_json::json;

use super::page::{Facts, Lab};
use crate::api;
use crate::widgets::Select;

#[component]
pub(super) fn FormCards(lab: Lab) -> impl IntoView {
    view! {
        <Notices lab=lab/>
        <VoiceCard lab=lab/>
        <TextCard lab=lab/>
        <CandidatesCard lab=lab/>
    }
}

/// Why the form cannot be trusted yet: the facts did not load, the last
/// round failed.
#[component]
fn Notices(lab: Lab) -> impl IntoView {
    view! {
        {move || match lab.facts.get() {
            Facts::Failed(e) => Some(view! {
                <div class="notice err">
                    {format!("⚠ {e}")}
                    <button class="btn ghost sm" on:click=move |_| lab.load_facts()>"Retry"</button>
                </div>
            }),
            _ => None,
        }}
        {move || {
            let e = lab.run_err.get();
            (!e.is_empty()).then(|| view! { <div class="notice err">{format!("⚠ {e}")}</div> })
        }}
    }
}

/// The description's label and the line under it, by what the model does
/// with one.
fn description_words(mode: VoiceDescriptionMode, required: bool) -> (&'static str, &'static str) {
    match mode {
        VoiceDescriptionMode::VoiceDesign if required => (
            "Voice description",
            "This model designs its voice from the description and cannot speak without one.",
        ),
        VoiceDescriptionMode::VoiceDesign => (
            "Voice description",
            "This model designs its voice from the description; empty, its row's own \
             description is used.",
        ),
        VoiceDescriptionMode::Style => (
            "Speaking style",
            "This model speaks the voice below in the style described.",
        ),
        VoiceDescriptionMode::Passthrough => (
            "Voice description",
            "Sent to the model as instructions; what it makes of them is the model's own.",
        ),
        VoiceDescriptionMode::None => ("", ""),
    }
}

#[component]
fn VoiceCard(lab: Lab) -> impl IntoView {
    let mode = move || {
        lab.facts()
            .map(|f| (f.description, f.description_required, f.takes_voice))
    };
    let placeholder = move || {
        lab.facts()
            .and_then(|f| f.default_voice)
            .map(|d| format!("model default ({d})"))
            .unwrap_or_else(|| "model default".into())
    };
    view! {
        <div class="card">
            <h3 class="lab-h3">"Voice"</h3>
            {move || match mode() {
                None => view! {
                    <p class="dim mini-note">"Pick a model: the form shows what it takes."</p>
                }
                .into_any(),
                Some((desc, required, takes_voice)) => {
                    let (label, hint) = description_words(desc, required);
                    view! {
                        {(desc != VoiceDescriptionMode::None).then(|| view! {
                            <div class="field">
                                <label>{label}</label>
                                <textarea
                                    class="input ta vl-desc"
                                    placeholder="e.g. a warm, calm female voice in her forties, \
                                                 slightly husky, unhurried"
                                    prop:value=move || lab.description.get()
                                    on:input=move |ev| lab.description.set(event_target_value(&ev))
                                ></textarea>
                                <div class="field-hint">{hint}</div>
                            </div>
                        })}
                        {takes_voice.then(|| view! {
                            <div class="field" style="margin-top:10px">
                                <label>"Voice"</label>
                                <input
                                    class="input"
                                    list="vl-voices"
                                    placeholder=placeholder
                                    prop:value=move || lab.voice.get()
                                    on:input=move |ev| lab.voice.set(event_target_value(&ev))
                                />
                                <datalist id="vl-voices">
                                    {move || {
                                        lab.voices
                                            .get()
                                            .into_iter()
                                            .map(|v| view! { <option value=v></option> })
                                            .collect_view()
                                    }}
                                </datalist>
                                <div class="field-hint">
                                    "A voice the model speaks: one it ships, a preset, or a library sample."
                                </div>
                            </div>
                        })}
                        {(desc == VoiceDescriptionMode::None && !takes_voice).then(|| view! {
                            <p class="dim mini-note">"This model takes neither a description nor a voice."</p>
                        })}
                        {(desc == VoiceDescriptionMode::None && takes_voice).then(|| view! {
                            <p class="dim mini-note">"This model reads no voice description."</p>
                        })}
                    }
                    .into_any()
                }
            }}
        </div>
    }
}

#[component]
fn TextCard(lab: Lab) -> impl IntoView {
    let options = Signal::derive(move || {
        let mut out: Vec<(String, String)> = lab.sentences.with(|v| {
            v.iter()
                .map(|s| (s.language.clone(), format!("{} ({})", s.name, s.language)))
                .collect()
        });
        // The model's own languages, and the form's, are pickable too: a
        // language without a sentence starts with an empty box.
        let mut more: Vec<String> = lab.facts().map(|f| f.languages).unwrap_or_default();
        more.push(lab.language.get());
        for code in more {
            if !code.is_empty() && !out.iter().any(|(c, _)| *c == code) {
                out.push((code.clone(), code));
            }
        }
        out
    });
    let entry = move || lab.sentence(&lab.language.get());
    let in_effect = move || entry().map(|s| s.text).unwrap_or_default();
    let edited = move || lab.text.get().trim() != in_effect().trim();
    let saving = RwSignal::new(false);
    let save = move |text: String| {
        let language = lab.language.get_untracked();
        saving.set(true);
        lab.scope.spawn(async move {
            let body = json!({"language": language, "text": text});
            match api::post::<VoiceSentences, _>("/voice-lab/api/sentences", &body).await {
                Ok(t) => {
                    lab.set_sentences(t);
                    lab.toasts.ok(if text.trim().is_empty() {
                        "back to the built-in sentence"
                    } else {
                        "saved: this is the sentence for the language now"
                    });
                    if text.trim().is_empty() {
                        let s = lab.sentence(&language).map(|s| s.text).unwrap_or_default();
                        lab.text.set(s);
                    }
                }
                Err(e) => lab.toasts.err(e.to_string()),
            }
            saving.set(false);
        });
    };
    let name = move || {
        entry()
            .map(|s| s.name)
            .unwrap_or_else(|| lab.language.get())
    };
    view! {
        <div class="card">
            <h3 class="lab-h3">"Text"</h3>
            <div class="field-grid" style="--field-min:180px">
                <div class="field">
                    <label>"Language"</label>
                    <Select value=lab.language options=options placeholder="pick a language"/>
                </div>
            </div>
            <div class="field" style="margin-top:10px">
                <label>"What every candidate says"</label>
                <textarea
                    class="input ta vl-text"
                    placeholder="No sentence for this language yet: write one, then save it for the language."
                    prop:value=move || lab.text.get()
                    on:input=move |ev| lab.text.set(event_target_value(&ev))
                ></textarea>
                <div class="field-hint">
                    "A cloning model learns the voice from this clip, so a phonetically rich passage "
                    "of about ten seconds works best. It becomes the sample's transcript as the model was sent it, "
                    "so listen to the end before saving: a model that skips words leaves a transcript a cloning model trips over."
                </div>
            </div>
            <div class="row vl-text-actions">
                <Show when=move || edited() && !lab.text.get().trim().is_empty()>
                    <button
                        class="btn sm"
                        disabled=move || saving.get()
                        title="Keep this text as the language's sentence: it is the prefill from now on"
                        on:click=move |_| save(lab.text.get_untracked())
                    >
                        {move || format!("Save as the {} sentence", name())}
                    </button>
                    <button
                        class="btn ghost sm"
                        title="Put the language's sentence back in the box"
                        on:click=move |_| lab.text.set(in_effect())
                    >
                        "Undo edits"
                    </button>
                </Show>
                <Show when=move || entry().is_some_and(|s| s.saved && s.builtin.is_some())>
                    <button
                        class="btn ghost sm"
                        disabled=move || saving.get()
                        title="Drop your saved sentence: the built-in one is the prefill again"
                        on:click=move |_| save(String::new())
                    >
                        "Back to the built-in sentence"
                    </button>
                </Show>
                <span class="dim mini-note">
                    {move || match entry() {
                        Some(s) if s.saved => "your saved sentence".to_string(),
                        Some(_) => "the built-in sentence".to_string(),
                        None => "no sentence for this language".to_string(),
                    }}
                </span>
            </div>
            {move || {
                let e = lab.sentences_err.get();
                (!e.is_empty()).then(|| view! {
                    <div class="notice err vl-file-err">{format!("⚠ Your saved sentences are not used: {e}")}</div>
                })
            }}
            {move || {
                lab.facts()
                    .and_then(|f| f.language_note)
                    .map(|n| view! { <div class="field-hint vl-warn">{n}</div> })
            }}
        </div>
    }
}

#[component]
fn CandidatesCard(lab: Lab) -> impl IntoView {
    let reads_seed = move || lab.facts().is_some_and(|f| f.reads_seed);
    view! {
        <div class="card">
            <h3 class="lab-h3">"Candidates"</h3>
            <div class="field-grid" style="--field-min:160px">
                <div class="field">
                    <label>"How many"</label>
                    <input
                        class="input w-num"
                        type="number"
                        min="1"
                        step="1"
                        prop:value=move || lab.count.get()
                        on:input=move |ev| lab.count.set(event_target_value(&ev))
                    />
                </div>
                <Show when=reads_seed>
                    <div class="field">
                        <label>"First seed"</label>
                        <input
                            class="input"
                            inputmode="numeric"
                            placeholder="random per candidate"
                            prop:value=move || lab.seed.get()
                            on:input=move |ev| lab.seed.set(event_target_value(&ev))
                        />
                    </div>
                </Show>
            </div>
            <div class="field-hint">
                {move || {
                    match lab.facts() {
                        Some(f) if f.reads_seed && f.row_pins_seed => {
                            "Each candidate is sent a seed of its own (the next ones count up from \
                             the first). This row also pins a seed in its default request options."
                        }
                        Some(f) if f.reads_seed => {
                            "Each candidate is sent a seed of its own; the next ones count up from \
                             the first. Rendered one after another."
                        }
                        Some(f) if !f.local => {
                            "A cloud model is sent no seed: its candidates differ by its own chance."
                        }
                        Some(_) => {
                            "This engine takes no seed: its candidates may come out alike."
                        }
                        None => "Rendered one after another.",
                    }
                }}
            </div>
        </div>
    }
}
