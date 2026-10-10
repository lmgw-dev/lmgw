//! The Files tab: drag-and-drop or pick several files, what became of each,
//! and the table of what the base holds — status and reason, pages, skipped
//! pages, chunks — with view text, download original, re-ingest and delete on
//! each row, and Resume / Cancel for the base's job.

use leptos::prelude::*;
use serde_json::{json, Value};
use wasm_bindgen::JsValue;

use crate::fmt::{grouped, human_bytes};
use crate::widgets::{use_toasts, MenuItem, RowMenu};

use super::knowledge::{use_kb, KbFile, KbJobLine};
use super::knowledge_source::{SourceModal, SourceRef};

use lmgw_api_types::knowledge::{
    ResumeResult, UploadItem, UploadResult as UploadResponse, UploadVerdict,
};

fn drag_has_files(dt: &web_sys::DataTransfer) -> bool {
    dt.types().includes(&JsValue::from_str("Files"), 0)
}

/// One toast for the whole upload: how many of each outcome.
fn summary(items: &[UploadItem]) -> String {
    let mut parts = Vec::new();
    for o in [
        UploadVerdict::Added,
        UploadVerdict::Replaced,
        UploadVerdict::Unchanged,
        UploadVerdict::Duplicate,
        UploadVerdict::Refused,
    ] {
        let n = items.iter().filter(|i| i.outcome == o).count();
        if n > 0 {
            parts.push(format!("{n} {}", o.as_str()));
        }
    }
    parts.join(", ")
}

#[component]
pub fn FilesTab() -> impl IntoView {
    let ctx = use_kb();
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    let over = RwSignal::new(false);
    let results = RwSignal::new(Vec::<UploadItem>::new());
    let source = RwSignal::new(None::<SourceRef>);
    let input: NodeRef<leptos::html::Input> = NodeRef::new();

    let upload = move |files: web_sys::FileList| {
        if files.length() == 0 || busy.get_untracked() {
            return;
        }
        let Ok(form) = web_sys::FormData::new() else {
            toasts.err("this browser has no FormData");
            return;
        };
        for i in 0..files.length() {
            if let Some(f) = files.get(i) {
                let _ = form.append_with_blob_and_filename("file", &f, &f.name());
            }
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            let res = crate::api::post_raw::<UploadResponse>(
                format!("/api/knowledge/bases/{}/files", ctx.id),
                form,
            )
            .await;
            match res {
                Ok(r) => {
                    let s = summary(&r.items);
                    if r.items.iter().any(|i| i.outcome == UploadVerdict::Refused) {
                        toasts.warn(s);
                    } else {
                        toasts.ok(s);
                    }
                    if ctx.scope.alive() {
                        results.set(r.items);
                    }
                }
                Err(e) => toasts.err(e.to_string()),
            }
            if ctx.scope.alive() {
                busy.set(false);
                ctx.bump();
            }
        });
    };

    let pick = move |_| {
        if let Some(i) = input.get() {
            if let Some(files) = i.files() {
                upload(files);
            }
            // The same file picked again is a new change.
            i.set_value("");
        }
    };

    let resume = move |_| {
        leptos::task::spawn_local(async move {
            match crate::api::post::<ResumeResult, _>(
                format!("/api/knowledge/bases/{}/resume", ctx.id),
                &json!({}),
            )
            .await
            {
                Ok(v) => match v.message {
                    Some(m) => toasts.ok(m),
                    None => toasts.ok("resumed"),
                },
                Err(e) => toasts.err(e.to_string()),
            }
            if ctx.scope.alive() {
                ctx.bump();
            }
        });
    };
    let needs_resume = Memo::new(move |_| {
        ctx.kb.with(|k| {
            k.as_ref().is_some_and(|k| {
                super::knowledge::kb_job(k.id).is_none()
                    && (k.counts.pending > 0
                        || k.counts.embedded < k.counts.chunks
                        || k.status == "re_embed_required")
            })
        })
    });
    let has_job = Memo::new(move |_| super::knowledge::kb_job(ctx.id).is_some());

    view! {
        <div
            class="kb-drop"
            class:drag-over=move || over.get()
            on:dragover=move |ev| {
                if ev.data_transfer().is_some_and(|dt| drag_has_files(&dt)) {
                    ev.prevent_default();
                    over.set(true);
                }
            }
            on:dragleave=move |ev| {
                ev.prevent_default();
                over.set(false);
            }
            on:drop=move |ev| {
                let Some(dt) = ev.data_transfer() else { return };
                if !drag_has_files(&dt) {
                    return;
                }
                ev.prevent_default();
                over.set(false);
                if let Some(files) = dt.files() {
                    upload(files);
                }
            }
        >
            <span class="dim">
                {move || {
                    if busy.get() {
                        "Uploading…"
                    } else {
                        "Drop files here — PDF, Office, text, Markdown, spreadsheets, code — or"
                    }
                }}
            </span>
            <input
                node_ref=input
                type="file"
                multiple
                hidden
                on:change=pick
            />
            <button
                class="btn"
                disabled=move || busy.get()
                on:click=move |_| {
                    if let Some(i) = input.get() {
                        i.click();
                    }
                }
            >
                "Choose files…"
            </button>
            <Show when=move || needs_resume.get()>
                <button class="btn primary" on:click=resume title="Start the waiting work again">
                    "Resume"
                </button>
            </Show>
        </div>

        <Show when=move || has_job.get()>
            <div class="card">
                <KbJobLine id=ctx.id/>
            </div>
        </Show>

        <Show when=move || !results.with(Vec::is_empty)>
            <div class="card kb-results">
                <div class="card-head">
                    <span class="mini-head">"Last upload"</span>
                    <span class="spacer" style="flex:1"></span>
                    <button class="btn ghost sm" on:click=move |_| results.set(Vec::new())>
                        "Dismiss"
                    </button>
                </div>
                <For each=move || results.get() key=|i| (i.name.clone(), i.outcome, i.reason.clone()) let:i>
                    <div class="kb-result">
                        <span class=match i.outcome.as_str() {
                            "added" | "replaced" => "chip ok",
                            "refused" => "chip err",
                            _ => "chip off",
                        }>{i.outcome.as_str()}</span>
                        <span class="mono-sm">{i.name.clone()}</span>
                        {i.reason.clone().map(|r| view! { <span class="dim mini-note">{r}</span> })}
                    </div>
                </For>
            </div>
        </Show>

        <div class="card pad0 kb-files">
            <table class="data">
                <thead>
                    <tr>
                        <th>"File"</th>
                        <th>"Kind"</th>
                        <th class="num-h">"Size"</th>
                        <th>"Status"</th>
                        <th class="num-h">"Pages"</th>
                        <th class="num-h">"Skipped"</th>
                        <th class="num-h">"Chunks"</th>
                        <th class="actions"></th>
                    </tr>
                </thead>
                <tbody>
                    <For each=move || ctx.files.get() key=|f| (f.id, f.status.clone(), f.chunk_count, f.error.clone(), f.size) let:f>
                        <FileRow f=f source=source/>
                    </For>
                </tbody>
            </table>
            <Show when=move || ctx.files.with(Vec::is_empty)>
                <div class="empty">"No files yet — drop some above."</div>
            </Show>
        </div>
        <SourceModal target=source/>
    }
}

#[component]
fn FileRow(f: KbFile, source: RwSignal<Option<SourceRef>>) -> impl IntoView {
    let ctx = use_kb();
    let toasts = use_toasts();
    let id = f.id;
    let name = f.name.clone();
    let post = move |what: &'static str, done: &'static str| {
        leptos::task::spawn_local(async move {
            match crate::api::post::<Value, _>(
                format!("/api/knowledge/files/{id}/{what}"),
                &json!({}),
            )
            .await
            {
                Ok(_) => toasts.ok(done),
                Err(e) => toasts.err(e.to_string()),
            }
            if ctx.scope.alive() {
                ctx.bump();
            }
        });
    };
    let items = {
        let name = name.clone();
        let busy = f.status == "ingesting";
        Signal::derive(move || {
            let name = name.clone();
            vec![
                MenuItem::new("View text", move || {
                    source.set(Some(SourceRef {
                        file_id: id,
                        ..Default::default()
                    }))
                }),
                MenuItem::new("Download original", move || {
                    super::docs::download(&format!("/api/knowledge/files/{id}/original"), &name)
                }),
                MenuItem::new("Re-ingest", move || {
                    post("reingest", "queued for re-ingest")
                })
                .disabled(busy),
                MenuItem::new("Delete", move || post("delete", "file deleted"))
                    .danger()
                    .disabled(busy),
            ]
        })
    };
    let chip = match f.status.as_str() {
        "ready" => "chip ok",
        "failed" => "chip err",
        "ingesting" => "chip live",
        _ => "chip warn",
    };
    let reason = f.error.clone();
    let notes = f.notes.clone();
    view! {
        <tr>
            <td>
                <button
                    class="link-btn"
                    title="View the extracted text"
                    on:click=move |_| source.set(Some(SourceRef { file_id: id, ..Default::default() }))
                >
                    {f.name.clone()}
                </button>
            </td>
            <td class="mono-sm dim" title=f.mime.clone()>
                {f.kind.clone()}
                {(!f.sub.is_empty() && f.sub != f.kind).then(|| format!(" · {}", f.sub))}
            </td>
            <td class="num">{human_bytes(f.size.max(0) as u64)}</td>
            <td>
                <span class=chip>
                    <span class="dot"></span>
                    {f.status.clone()}
                </span>
                {reason.map(|r| view! { <div class="mini-note kb-reason">{r}</div> })}
                <For each=move || notes.clone() key=|n| n.clone() let:n>
                    <div class="dim mini-note">{n}</div>
                </For>
            </td>
            <td class="num">{f.pages.map(|p| p.to_string()).unwrap_or_default()}</td>
            <td class="num">
                {if f.skipped_pages > 0 { grouped(f.skipped_pages as u64) } else { String::new() }}
            </td>
            <td class="num">{grouped(f.chunk_count.max(0) as u64)}</td>
            <td class="actions">
                <RowMenu items=items/>
            </td>
        </tr>
    }
}
