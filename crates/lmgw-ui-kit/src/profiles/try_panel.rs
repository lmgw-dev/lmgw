//! Trying a draft without saving it: the static part with its token count,
//! one model call (Test), and a spoken sample (Speak). All three send the
//! form as it stands and store nothing.

use leptos::prelude::*;
use lmgw_api_types::chat_profiles::{PreviewAnswer, ProfileDraft, TestAnswer};

use super::api;
use super::fields::seg_btn;
use super::model::{self, Form, Turn};
use crate::scope::Scope;
use crate::widgets::model_picker::ModelPicker;
use crate::widgets::{ActionRow, AudioPlayer, PrefGroup, StackRow, TextRow};

/// What Show fetched, and the draft it was fetched for.
#[derive(Clone)]
struct Shown {
    for_draft: ProfileDraft,
    answer: PreviewAnswer,
}

#[component]
pub(super) fn StaticPart(
    form: Signal<Form>,
    thread_id: Option<i64>,
    /// The generic voice block's reference, filled by Show.
    reference: RwSignal<Option<String>>,
    /// The model Count runs on; the host's choice survives a profile change.
    count_model: RwSignal<String>,
) -> impl IntoView {
    let http = crate::http::use_http();
    let shown = RwSignal::new(None::<Shown>);
    let busy = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let scope = Scope::new();

    let run = move |count: bool| {
        if busy.get_untracked() {
            return;
        }
        let draft = form.get_untracked().draft();
        let model = count
            .then(|| count_model.get_untracked())
            .filter(|m| !m.is_empty());
        busy.set(true);
        err.set(None);
        scope.spawn(async move {
            let res = api::preview(http, draft.clone(), thread_id, model).await;
            busy.set(false);
            match res {
                Ok(answer) => {
                    reference.set(Some(answer.voice_turn.clone()));
                    shown.set(Some(Shown {
                        for_draft: draft,
                        answer,
                    }));
                }
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    let stale = move || {
        shown
            .get()
            .is_some_and(|s| s.for_draft != form.get().draft())
    };
    let part = move |title: &'static str, turn: Turn| {
        shown.get().map(|s| {
            let text = match turn {
                Turn::Static => s.answer.static_part.clone(),
                Turn::Text => s.answer.text_turn.clone(),
                Turn::Voice => s.answer.voice_turn.clone(),
            };
            let tokens = s.answer.tokens.as_ref().map(|t| model::token_line(t, turn));
            view! {
                <details class="pf-part">
                    <summary>
                        {title}
                        {tokens.map(|t| view! { <span class="pf-tokens">{t}</span> })}
                    </summary>
                    <pre class="pf-pre">
                        {if text.is_empty() { "(empty)".to_string() } else { text }}
                    </pre>
                </details>
            }
        })
    };
    view! {
        <PrefGroup
            title="Static part"
            desc="Show assembles the system messages this draft produces. Count adds the tokens on the chosen model; counting on a local model loads it."
        >
            <ActionRow title="Show the messages" sub="The static part, the text turn and the voice turn as this draft assembles them.">
                <button type="button" class="btn sm" disabled=move || busy.get() on:click=move |_| run(false)>
                    "Show"
                </button>
            </ActionRow>
            <StackRow title="Count on" sub="Counting on a local model loads it.">
                <ModelPicker value=count_model tasks=&["chat"] empty_label="pick a model".to_string() />
            </StackRow>
            <ActionRow title="Count tokens" sub="The same, with the tokens on the model above.">
                <button
                    type="button"
                    class="btn sm"
                    disabled=move || busy.get() || count_model.get().is_empty()
                    title="Counting on a local model loads it"
                    on:click=move |_| run(true)
                >
                    "Count"
                </button>
            </ActionRow>
            {move || {
                let any = err.with(Option::is_some) || shown.with(Option::is_some);
                any.then(|| {
                    view! {
                        <StackRow>
                            {move || err.get().map(|e| view! { <div class="prow-err" role="alert">{e}</div> })}
                            {move || stale().then(|| view! { <div class="pf-warn">"Edited since — show again."</div> })}
                            {move || part("Static part (persona, length rule, examples)", Turn::Static)}
                            {move || part("Text turn", Turn::Text)}
                            {move || part("Voice turn", Turn::Voice)}
                        </StackRow>
                    }
                })
            }}
        </PrefGroup>
    }
}

#[component]
pub(super) fn TestPanel(
    form: Signal<Form>,
    thread_id: Option<i64>,
    model: RwSignal<String>,
    /// The last reply, for Speak's starting text.
    last_reply: RwSignal<Option<String>>,
) -> impl IntoView {
    let http = crate::http::use_http();
    let text = RwSignal::new(String::new());
    let voice = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let answer = RwSignal::new(None::<TestAnswer>);
    let scope = Scope::new();
    let run = move |_| {
        if busy.get_untracked() {
            return;
        }
        let (m, t) = (model.get_untracked(), text.get_untracked());
        let draft = form.get_untracked().draft();
        let v = voice.get_untracked();
        busy.set(true);
        err.set(None);
        scope.spawn(async move {
            let res = api::test(http, draft, thread_id, m, t, v).await;
            busy.set(false);
            match res {
                Ok(a) => {
                    last_reply.set(Some(a.reply.clone()));
                    answer.set(Some(a));
                }
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    view! {
        <PrefGroup title="Test" desc="One call with this draft; nothing is stored.">
            <StackRow title="Model">
                <ModelPicker value=model tasks=&["chat"] empty_label="pick a model".to_string() />
            </StackRow>
            <ActionRow title="As">
                <div class="seg" role="group" aria-label="Turn kind">
                    {seg_btn("Text", Signal::derive(move || !voice.get()), move || voice.set(false))}
                    {seg_btn("Voice", Signal::derive(move || voice.get()), move || voice.set(true))}
                </div>
            </ActionRow>
            <TextRow
                title="Message"
                value=text
                placeholder="Something this profile should handle…"
            />
            <ActionRow title="Run the test" sub="Sends the message once on the model above.">
                <button
                    type="button"
                    class="btn primary sm"
                    disabled=move || {
                        busy.get() || model.get().is_empty() || text.get().trim().is_empty()
                    }
                    on:click=run
                >
                    {move || if busy.get() { "Running…" } else { "Run test" }}
                </button>
            </ActionRow>
            {move || {
                let any = err.with(Option::is_some) || answer.with(Option::is_some);
                any.then(|| {
                    view! {
                        <StackRow>
                            {move || err.get().map(|e| view! { <div class="prow-err" role="alert">{e}</div> })}
                            {move || answer.get().map(answer_view)}
                        </StackRow>
                    }
                })
            }}
        </PrefGroup>
    }
}

/// A test's reply, what it took and the folds behind it.
fn answer_view(a: TestAnswer) -> impl IntoView {
    let timing = model::timing_line(a.first_token_ms, a.reasoning_ms, a.total_ms);
    let usage = match (a.usage.prompt_tokens, a.usage.completion_tokens) {
        (Some(p), Some(c)) => Some(format!("{p} prompt · {c} completion tokens")),
        (Some(p), None) => Some(format!("{p} prompt tokens")),
        (None, Some(c)) => Some(format!("{c} completion tokens")),
        _ => None,
    };
    view! {
        <div class="pf-answer">
            <pre class="pf-pre pf-reply">{a.reply.clone()}</pre>
            <div class="pf-meta">
                {timing} {usage.map(|u| format!(" · {u}"))}
                {a.answered_by.clone().map(|b| format!(" · answered by {b}"))}
            </div>
            {a.reasoning_note.clone().map(|n| view! { <div class="pf-note">{n}</div> })}
            {(!a.reasoning.is_empty())
                .then(|| {
                    view! {
                        <details class="pf-part">
                            <summary>"Reasoning"</summary>
                            <pre class="pf-pre">{a.reasoning.clone()}</pre>
                        </details>
                    }
                })}
            <details class="pf-part">
                <summary>"System message sent"</summary>
                <pre class="pf-pre">{a.system.clone()}</pre>
            </details>
        </div>
    }
}

/// An object URL for the bytes of a WAV, to hand an `<audio>` element.
fn object_url(bytes: &[u8], kind: &str) -> Option<String> {
    let arr = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::of1(&arr);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(if kind.is_empty() { "audio/wav" } else { kind });
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

#[component]
pub(super) fn SpeakPanel(
    form: Signal<Form>,
    thread_id: Option<i64>,
    last_reply: RwSignal<Option<String>>,
) -> impl IntoView {
    let http = crate::http::use_http();
    let text = RwSignal::new(String::new());
    // Once the owner has typed, the starting text stops following the
    // examples and the last reply; emptying the box hands it back.
    let edited = RwSignal::new(false);
    Effect::new(move |_| {
        let start = model::speak_default(
            last_reply.get().as_deref(),
            &form.with(|f| f.wire_examples()),
        );
        if !edited.get_untracked() {
            text.set(start);
        }
    });
    let busy = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let url = RwSignal::new(None::<String>);
    let old = StoredValue::new(None::<String>);
    on_cleanup(move || {
        if let Some(u) = old.get_value() {
            let _ = web_sys::Url::revoke_object_url(&u);
        }
    });
    let scope = Scope::new();
    let run = move |_| {
        if busy.get_untracked() {
            return;
        }
        let draft = form.get_untracked().draft();
        let t = text.get_untracked();
        busy.set(true);
        err.set(None);
        scope.spawn(async move {
            let res = api::speak(http, draft, thread_id, t).await;
            busy.set(false);
            match res {
                Ok((bytes, kind)) => match object_url(&bytes, &kind) {
                    Some(u) => {
                        if let Some(prev) = old.get_value() {
                            let _ = web_sys::Url::revoke_object_url(&prev);
                        }
                        old.set_value(Some(u.clone()));
                        url.set(Some(u));
                    }
                    None => err.set(Some("the browser could not open the audio".into())),
                },
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    view! {
        <PrefGroup title="Speak a sample">
            <StackRow
                title="Text"
                sub="Starts as the last test reply, else the first example's reply. Spoken with this draft's voice, on the default output."
            >
                <textarea
                    class="prow-area"
                    rows="3"
                    aria-label="Text to speak"
                    prop:value=move || text.get()
                    on:input=move |ev| {
                        let t = event_target_value(&ev);
                        edited.set(!t.is_empty());
                        text.set(t);
                    }
                ></textarea>
            </StackRow>
            <ActionRow title="Speak" sub="Plays it once the voice has it.">
                <button
                    type="button"
                    class="btn primary sm"
                    disabled=move || busy.get() || text.get().trim().is_empty()
                    on:click=run
                >
                    {move || if busy.get() { "Speaking…" } else { "Speak" }}
                </button>
            </ActionRow>
            {move || {
                let any = err.with(Option::is_some) || url.with(Option::is_some);
                any.then(|| {
                    view! {
                        <StackRow>
                            {move || err.get().map(|e| view! { <div class="prow-err" role="alert">{e}</div> })}
                            {move || {
                                url.get()
                                    .map(|u| {
                                        view! {
                                            <AudioPlayer src=Some(u) autoplay=true label="The spoken sample"/>
                                        }
                                    })
                            }}
                        </StackRow>
                    }
                })
            }}
        </PrefGroup>
    }
}
