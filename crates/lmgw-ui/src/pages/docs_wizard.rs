//! The ingest wizard (quickdoc §11): one library at one version, the sources
//! it is built from, and the two models a corpus is a function of.
//!
//! Both models are picked from what the gateway actually serves — the shared
//! model catalog, so a cloud embedding model is offered beside the local ones
//! — and are *validated by the API before the corpus row exists*: a corpus
//! pinned to a model that is not there could never be ingested or queried. So
//! the failure path here is simply to print what the API said: those messages
//! name the model and the reason, and re-wording them would only lose
//! information.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::DocsJobStarted;
use serde_json::json;

use crate::widgets::{use_toasts, Modal, ModalFooter, ModelPicker, Select};

use super::docs::{use_docs, CorpusJobLine, WizardSeed};

/// The source kinds §8 lists, cheapest first — the order the ingest prompt
/// tries them in.
const SOURCE_KINDS: [(&str, &str); 4] = [
    ("llms_txt", "llms.txt / llms-full.txt"),
    ("markdown", "repo markdown / mdBook"),
    ("rustdoc_json", "rustdoc JSON"),
    ("html", "fenced HTML (last resort)"),
];

/// One source row being typed. `key` is stable for the row's lifetime so the
/// `For` is not keyed by position — removing a row would otherwise shift every
/// row below it onto another row's rendered fields.
#[derive(Clone, Debug, PartialEq)]
struct SourceDraft {
    key: u32,
    root: String,
    kind: String,
    fence: String,
}

impl SourceDraft {
    fn new(key: u32) -> Self {
        Self {
            key,
            root: String::new(),
            kind: "llms_txt".into(),
            fence: String::new(),
        }
    }
}

#[component]
pub fn IngestWizard() -> impl IntoView {
    let state = use_docs();
    let open = RwSignal::new(false);
    // Guarded both ways, like the model editors: an unconditional set would
    // notify even when unchanged and the two effects would ping-pong.
    Effect::new(move |_| {
        let want = state.wizard.get().is_some();
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && state.wizard.get_untracked().is_some() {
            state.wizard.set(None);
        }
    });
    view! {
        <Modal open=open title="Ingest a library" guard=true>
            {move || {
                state
                    .wizard
                    .get()
                    .map(|seed| view! { <WizardForm seed=seed open=open/> }.into_any())
            }}
        </Modal>
    }
}

#[component]
fn WizardForm(seed: WizardSeed, open: RwSignal<bool>) -> impl IntoView {
    let state = use_docs();
    let toasts = use_toasts();

    let library = RwSignal::new(seed.library.clone());
    let version = RwSignal::new(seed.version.clone());
    let embed_model = RwSignal::new(String::new());
    let ingest_model = RwSignal::new(String::new());
    let start_now = RwSignal::new(true);
    let sources = RwSignal::new(vec![SourceDraft::new(0)]);
    let next_key = StoredValue::new(1u32);
    let creating = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // Row id + label of the corpus once it exists; the wizard then shows the
    // live job rather than a spinner it made up.
    let created = RwSignal::new(None::<(i64, String)>);

    let modal = crate::widgets::use_modal();
    let create = move |_| {
        if creating.get_untracked() {
            return;
        }
        let payload = json!({
            "library": library.get_untracked().trim(),
            "version": version.get_untracked().trim(),
            "embed_model": embed_model.get_untracked().trim(),
            "ingest_model": ingest_model.get_untracked().trim(),
            "sources": sources
                .get_untracked()
                .iter()
                .filter(|s| !s.root.trim().is_empty())
                .map(|s| {
                    json!({
                        "root": s.root.trim(),
                        "kind": s.kind,
                        "fence": s
                            .fence
                            .split([',', '\n'])
                            .map(str::trim)
                            .filter(|f| !f.is_empty())
                            .collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>(),
            "start": start_now.get_untracked(),
        });
        creating.set(true);
        error.set(None);
        spawn_local(async move {
            let res = crate::api::post::<DocsJobStarted, _>("/api/docs/corpora", &payload).await;
            creating.set(false);
            match res {
                Ok(r) => {
                    let corpus = r.corpus.unwrap_or_default();
                    toasts.ok(format!("corpus {} created", corpus.corpus_id));
                    created.set(Some((corpus.id, corpus.corpus_id)));
                    state.bump();
                    // Saved: the modal stays up to show the ingest, and
                    // closing it now loses nothing.
                    if let Some(m) = modal {
                        m.saved();
                    }
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    let add_source = move |_| {
        let key = next_key.get_value();
        next_key.set_value(key + 1);
        sources.update(|v| v.push(SourceDraft::new(key)));
    };

    view! {
        <div class="wiz">
            {(seed.reason.is_some() || seed.requested_by.is_some())
                .then(|| {
                    view! {
                        <div class="notice" style="margin-bottom:12px">
                            <b>"Prefilled from a doc request"</b>
                            {seed
                                .requested_by
                                .clone()
                                .map(|c| {
                                    view! { <span class="detail">"asked for by " {c}</span> }
                                })}
                            {seed.reason.clone().map(|r| view! { <span class="detail">{r}</span> })}
                        </div>
                    }
                })}

            {move || {
                match created.get() {
                    Some((id, label)) => {
                        view! {
                            <div class="wiz-ok">
                                "Corpus " <span class="mono-sm">{label}</span>
                                {if start_now.get_untracked() {
                                    " created — ingesting now. Closing this window does not stop it."
                                } else {
                                    " created. Start the ingest from its card when you are ready."
                                }}
                            </div>
                            <CorpusJobLine id=id/>
                            <ModalFooter>
                                <button class="btn" on:click=move |_| open.set(false)>
                                    "Close"
                                </button>
                            </ModalFooter>
                        }
                            .into_any()
                    }
                    None => {
                        view! {
                            <div class="spec-grid">
                                <div class="field">
                                    <label>"Library"</label>
                                    <input
                                        class="input mono"
                                        placeholder="axum"
                                        prop:value=move || library.get()
                                        on:input=move |ev| library.set(event_target_value(&ev))
                                    />
                                </div>
                                <div class="field">
                                    <label>"Version"</label>
                                    <input
                                        class="input mono"
                                        placeholder="0.8"
                                        prop:value=move || version.get()
                                        on:input=move |ev| version.set(event_target_value(&ev))
                                    />
                                </div>
                                // The same picker the corpus list's re-embed offers —
                                // one definition of "what this gateway can embed
                                // with", so the two forms cannot disagree.
                                <div class="field">
                                    <label>"Embedding model · pinned at ingest"</label>
                                    <ModelPicker value=embed_model tasks=&["embedding"] recent_key="docs.embed"/>
                                </div>
                                // Small local models are the intended ones (§8) —
                                // ingestion is batch work — but any chat model the
                                // gateway serves will do.
                                <div class="field">
                                    <label>"Extraction model · drives the span emitter"</label>
                                    <ModelPicker value=ingest_model tasks=&["chat"] recent_key="docs.ingest"/>
                                </div>
                            </div>

                            <div class="mini-head" style="margin-top:14px">
                                "Sources — where the documents come from"
                            </div>
                            <For each=move || sources.get() key=|s| s.key let:draft>
                                <SourceFields key=draft.key sources=sources/>
                            </For>
                            <div class="row" style="margin-top:8px">
                                <button class="btn ghost" on:click=add_source>
                                    "Add source"
                                </button>
                                <label class="row dim" style="gap:5px">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || start_now.get()
                                        on:change=move |ev| start_now.set(event_target_checked(&ev))
                                    />
                                    "start ingesting as soon as the corpus exists"
                                </label>
                            </div>

                            {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}

                            <ModalFooter>
                                <button class="btn ghost" on:click=move |_| open.set(false)>
                                    "Cancel"
                                </button>
                                <button
                                    class="btn primary"
                                    disabled=move || creating.get()
                                    on:click=create
                                >
                                    {move || {
                                        if creating.get() { "Creating…" } else { "Create corpus" }
                                    }}
                                </button>
                            </ModalFooter>
                        }
                            .into_any()
                    }
                }
            }}
        </div>
    }
}

/// One source row. The list is plain data in a single signal and every field
/// writes back into the row with this `key` — addressing by position would put
/// a row's typing into its neighbour the moment one above it is removed.
#[component]
fn SourceFields(key: u32, sources: RwSignal<Vec<SourceDraft>>) -> impl IntoView {
    let read = move |f: fn(&SourceDraft) -> String| {
        sources
            .get()
            .iter()
            .find(|s| s.key == key)
            .map(f)
            .unwrap_or_default()
    };
    let write = move |f: fn(&mut SourceDraft, String)| {
        move |ev: web_sys::Event| {
            let val = event_target_value(&ev);
            sources.update(|v| {
                if let Some(s) = v.iter_mut().find(|s| s.key == key) {
                    f(s, val);
                }
            });
        }
    };

    // `Select` writes a signal rather than firing an event, so the kind needs
    // one of its own, mirrored back into the row.
    let kind = RwSignal::new(read(|s| s.kind.clone()));
    Effect::new(move |_| {
        let k = kind.get();
        sources.update(|v| {
            if let Some(s) = v.iter_mut().find(|s| s.key == key) {
                s.kind = k;
            }
        });
    });
    let kind_opts = Signal::derive(|| {
        SOURCE_KINDS
            .iter()
            .map(|(v, l)| (v.to_string(), l.to_string()))
            .collect::<Vec<_>>()
    });

    view! {
        <div class="card" style="margin-top:8px">
            <div class="spec-grid">
                <div class="field">
                    <label>"Root URL"</label>
                    <input
                        class="input mono"
                        placeholder="https://docs.rs/axum/0.8.0/axum/"
                        prop:value=move || read(|s| s.root.clone())
                        on:input=write(|s, v| s.root = v)
                    />
                </div>
                <div class="field">
                    <label>"Kind"</label>
                    <Select value=kind options=kind_opts/>
                </div>
                <div class="field" style="grid-column:1 / -1">
                    <label>"Domain fence · empty = this root's own host, and nothing else"</label>
                    <input
                        class="input mono"
                        placeholder="docs.rs, github.com"
                        prop:value=move || read(|s| s.fence.clone())
                        on:input=write(|s, v| s.fence = v)
                    />
                </div>
            </div>
            <Show when=move || sources.with(|v| v.len() > 1)>
                <div class="row" style="justify-content:flex-end; margin-top:8px">
                    <button
                        class="btn ghost"
                        on:click=move |_| sources.update(|v| v.retain(|s| s.key != key))
                    >
                        "Remove source"
                    </button>
                </div>
            </Show>
        </div>
    }
}
