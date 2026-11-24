//! The corpora's dialogs: Re-embed, Delete, Export and Import, their
//! fields as preference rows and their actions as pill buttons.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{CorpusView, ExportManifest, ImportReport, MessageAck};
use serde_json::{json, Value};

use crate::fmt::{fmt_bytes, grouped};
use crate::widgets::{
    use_toasts, InfoRow, Modal, ModalFooter, ModelPicker, PrefGroup, StackRow, SwitchRow,
};

use super::{start_job, use_docs};

/// Two things behind one dialog, because they are the same job with and
/// without a new pin. Filling in missing vectors keeps the corpus where it
/// is; naming a model re-pins it and drops every old vector first (half a
/// corpus in each embedding space is worse than none).
#[component]
pub(super) fn ReembedModal(target: RwSignal<Option<CorpusView>>) -> impl IntoView {
    let state = use_docs();
    let open = RwSignal::new(false);
    let model = RwSignal::new(String::new());
    Effect::new(move |_| {
        let want = target.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
        if want {
            model.set(String::new());
        }
    });
    Effect::new(move |_| {
        if !open.get() && target.with_untracked(Option::is_some) {
            target.set(None);
        }
    });
    let go = move |body: Value| {
        let Some(c) = target.get_untracked() else {
            return;
        };
        open.set(false);
        start_job(
            format!("/api/docs/corpora/{}/re-embed", c.id),
            body,
            move || state.bump(),
        )
    };
    let no_model = move || model.with(|m| m.trim().is_empty());
    view! {
        <Modal open=open title="Re-embed corpus">
            <div class="docs-dialog">
                {move || {
                    target
                        .get()
                        .map(|c| {
                            view! {
                                <PrefGroup>
                                    <InfoRow title="Corpus" value=c.corpus_id.clone() mono=true/>
                                    <InfoRow title="Pinned to" value=c.embed_identity.clone() mono=true/>
                                    <InfoRow
                                        title="Chunks without a vector"
                                        value=grouped(c.unembedded_chunks.max(0) as u64)
                                    />
                                </PrefGroup>
                            }
                        })
                }}
                <PrefGroup
                    desc="Re-pinning drops every existing vector before it starts: half a corpus in each embedding space is worse than none. Leaving the picker alone only embeds what is missing."
                >
                    // Every model the gateway can embed with — local, aux and
                    // cloud — from the one catalog the other pickers use.
                    <StackRow title="Move it to another model" sub="Optional" class="kb-picker">
                        <ModelPicker
                            value=model
                            tasks=&["embedding"]
                            empty_label="keep the pinned model"
                            recent_key="docs.embed"
                        />
                    </StackRow>
                </PrefGroup>
            </div>
            <ModalFooter>
                <button class="btn" on:click=move |_| open.set(false)>
                    "Cancel"
                </button>
                <button class="btn" on:click=move |_| go(json!({}))>
                    "Fill in missing vectors"
                </button>
                <button
                    class="btn suggested pill"
                    disabled=no_model
                    on:click=move |_| go(json!({ "embed_model": model.get_untracked() }))
                >
                    "Re-pin and re-embed"
                </button>
            </ModalFooter>
        </Modal>
    }
}

#[component]
pub(super) fn DeleteCorpusModal(target: RwSignal<Option<CorpusView>>) -> impl IntoView {
    let state = use_docs();
    let toasts = use_toasts();
    let location = leptos_router::hooks::use_location();
    let navigate = StoredValue::new(leptos_router::hooks::use_navigate());
    let open = RwSignal::new(false);
    Effect::new(move |_| {
        let want = target.with(Option::is_some);
        if open.get_untracked() != want {
            open.set(want);
        }
    });
    Effect::new(move |_| {
        if !open.get() && target.with_untracked(Option::is_some) {
            target.set(None);
        }
    });
    let delete = move |_| {
        let Some(c) = target.get_untracked() else {
            return;
        };
        open.set(false);
        let id = c.id;
        spawn_local(async move {
            match crate::api::post::<MessageAck, _>(
                format!("/api/docs/corpora/{id}/delete"),
                &json!({}),
            )
            .await
            {
                Ok(v) => {
                    toasts.ok(v.message);
                    state.bump();
                    // The deleted corpus was the one shown: back to the list.
                    if location.pathname.get_untracked() == super::corpus_href(id) {
                        navigate.with_value(|n| n("/docs", Default::default()));
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };
    view! {
        <Modal open=open title="Delete corpus">
            <p>
                "Delete "
                <span class="mono-sm">
                    {move || target.with(|t| t.as_ref().map(|c| c.corpus_id.clone()).unwrap_or_default())}
                </span>
                " — its documents, chunks, vectors, golden queries and eval history?"
            </p>
            <ModalFooter>
                <button class="btn" on:click=move |_| open.set(false)>
                    "Keep it"
                </button>
                <button class="btn destructive pill" on:click=delete>
                    "Delete corpus"
                </button>
            </ModalFooter>
        </Modal>
    }
}

/// What the whole-DB download contains, before committing to it. The manifest
/// travels beside the file rather than inside it (§10) — the artifact stays a
/// plain SQLite database so `rsync`-ing it is still a complete backup.
#[component]
pub(super) fn ExportModal(open: RwSignal<bool>) -> impl IntoView {
    let manifest = LocalResource::new(move || {
        open.get();
        crate::api::get::<ExportManifest>("/api/docs/export/manifest")
    });
    view! {
        <Modal open=open title="Export corpora">
            <div class="docs-dialog">
                <p class="pref-desc">
                    "The export is the corpus database itself, checkpointed first — one file, "
                    "restorable by copying it back or importing it on another gateway."
                </p>
                {move || match manifest.get() {
                    None => view! { <div class="dim">"Reading the manifest…"</div> }.into_any(),
                    Some(Err(e)) => {
                        view! { <div class="notice err">{e.to_string()}</div> }.into_any()
                    }
                    Some(Ok(m)) => {
                        view! {
                            <PrefGroup>
                                <InfoRow title="Schema" value=format!("v{}", m.schema_version)/>
                                <InfoRow title="Size" value=fmt_bytes(m.byte_size)/>
                            </PrefGroup>
                            <PrefGroup title="Corpora in it">
                                <div class="table-scroll">
                                    <table class="data">
                                        <thead>
                                            <tr>
                                                <th>"Corpus"</th>
                                                <th>"Embedding"</th>
                                                <th class="num-h">"Chunks"</th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            <For
                                                each=move || m.corpora.clone()
                                                key=|c| c.corpus_id.clone()
                                                let:c
                                            >
                                                <tr>
                                                    <td class="mono-sm">{c.corpus_id.clone()}</td>
                                                    <td class="dim mono-sm">
                                                        {format!(
                                                            "{}/{} @{}",
                                                            c.embed_upstream,
                                                            c.embed_model,
                                                            c.embed_dims,
                                                        )}
                                                    </td>
                                                    <td class="num">{grouped(c.chunk_count.max(0) as u64)}</td>
                                                </tr>
                                            </For>
                                        </tbody>
                                    </table>
                                </div>
                            </PrefGroup>
                        }
                            .into_any()
                    }
                }}
            </div>
            <ModalFooter>
                <button class="btn" on:click=move |_| open.set(false)>
                    "Close"
                </button>
                <a class="btn suggested pill" href="/api/docs/export" download>
                    "Download quickdoc.db"
                </a>
            </ModalFooter>
        </Modal>
    }
}

/// Import is two deliberate steps: a **dry run** that runs every check and
/// writes nothing, then the commit. `replace` is never implied — overwriting a
/// `library@version` that already exists here is a choice, so the switch says
/// so and the dry run reports it first.
#[component]
pub(super) fn ImportModal(open: RwSignal<bool>) -> impl IntoView {
    let state = use_docs();
    let toasts = use_toasts();
    let file = RwSignal::new(None::<web_sys::File>);
    let name = RwSignal::new(String::new());
    let replace = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let report = RwSignal::new(None::<ImportReport>);
    let error = RwSignal::new(None::<String>);
    let input: NodeRef<leptos::html::Input> = NodeRef::new();

    let send = move |validate_only: bool| {
        let Some(f) = file.get_untracked() else {
            toasts.err("pick a corpus file first");
            return;
        };
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        report.set(None);
        error.set(None);
        let url = format!(
            "/api/docs/import?replace={}&validate_only={validate_only}",
            replace.get_untracked()
        );
        spawn_local(async move {
            let res = crate::api::post_raw::<ImportReport>(url, f).await;
            busy.set(false);
            match res {
                Ok(r) => {
                    let committed = !r.dry_run;
                    report.set(Some(r));
                    if committed {
                        toasts.ok("corpus file imported");
                        state.bump();
                    }
                }
                // Verbatim: these messages name the corpus and the model it
                // wants, which is the whole point of validating first.
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    let pick = move |ev: web_sys::Event| {
        let el: web_sys::HtmlInputElement = event_target(&ev);
        let picked = el.files().and_then(|l| l.get(0));
        name.set(picked.as_ref().map(|f| f.name()).unwrap_or_default());
        file.set(picked);
        report.set(None);
        error.set(None);
        if file.get_untracked().is_some() {
            send(true);
        }
    };
    // A changed choice is validated again before Import is offered.
    Effect::new(move |prev: Option<bool>| {
        let now = replace.get();
        if prev.is_some_and(|p| p != now) && file.get_untracked().is_some() {
            send(true);
        }
        now
    });
    let cannot_import = move || busy.get() || report.get().is_none_or(|r| !r.dry_run);
    let no_file = move || busy.get() || file.with(Option::is_none);

    view! {
        <Modal open=open title="Import a corpus file">
            <div class="docs-dialog">
                <PrefGroup
                    desc="A corpus file is a plain SQLite database exported from this or another gateway. It is validated first — schema version and whether the models it is pinned to resolve here — and nothing is written until you import."
                >
                    <div class="prow">
                        <div class="prow-text">
                            <span class="prow-title">"Corpus file"</span>
                            <span class="prow-sub" title=move || name.get()>
                                {move || {
                                    let n = name.get();
                                    if n.is_empty() { ".db, .sqlite or .sqlite3".to_string() } else { n }
                                }}
                            </span>
                        </div>
                        <input
                            node_ref=input
                            type="file"
                            hidden
                            accept=".db,.sqlite,.sqlite3"
                            on:change=pick
                        />
                        <button
                            type="button"
                            class="btn"
                            on:click=move |_| {
                                if let Some(i) = input.get() {
                                    i.click();
                                }
                            }
                        >
                            "Choose file…"
                        </button>
                    </div>
                    <SwitchRow
                        title="Overwrite what exists"
                        sub="A library@version that is already here is replaced"
                        value=replace
                    />
                </PrefGroup>

                {move || error.get().map(|e| view! { <div class="wiz-err">{e}</div> })}
                {move || {
                    report
                        .get()
                        .map(|r| {
                            let head = if r.dry_run {
                                "Validated — nothing written yet".to_string()
                            } else {
                                "Imported".to_string()
                            };
                            view! {
                                <PrefGroup
                                    title=head
                                    desc=format!(
                                        "File schema v{}, this gateway v{}",
                                        r.file_schema_version,
                                        r.schema_version,
                                    )
                                >
                                    <div class="table-scroll">
                                        <table class="data">
                                            <thead>
                                                <tr>
                                                    <th>"Corpus"</th>
                                                    <th>"Pinned embedding"</th>
                                                    <th>"Resolves as"</th>
                                                    <th class="num-h">"Chunks"</th>
                                                </tr>
                                            </thead>
                                            <tbody>
                                                <For
                                                    each=move || r.imported.clone()
                                                    key=|c| c.corpus_id.clone()
                                                    let:c
                                                >
                                                    <tr>
                                                        <td class="mono-sm">
                                                            {c.corpus_id.clone()}
                                                            {c
                                                                .replaced
                                                                .then(|| {
                                                                    view! { " " <span class="chip warn">"replaced"</span> }
                                                                })}
                                                        </td>
                                                        <td class="dim mono-sm">{c.embed_model.clone()}</td>
                                                        <td class="mono-sm">
                                                            {c.embed_alias.clone().unwrap_or_else(|| "—".into())}
                                                        </td>
                                                        <td class="num">{grouped(c.chunks.max(0) as u64)}</td>
                                                    </tr>
                                                </For>
                                            </tbody>
                                        </table>
                                    </div>
                                </PrefGroup>
                            }
                        })
                }}
            </div>
            <ModalFooter>
                <button class="btn" disabled=no_file on:click=move |_| send(true)>
                    {move || if busy.get() { "Checking…" } else { "Validate again" }}
                </button>
                <button class="btn suggested pill" disabled=cannot_import on:click=move |_| send(false)>
                    "Import"
                </button>
            </ModalFooter>
        </Modal>
    }
}
